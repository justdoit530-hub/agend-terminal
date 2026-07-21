//! #2453: Structural tests for root `AppState` and thin `run_app` loop orchestration.

// structural tests read sources from disk; no super imports needed

/// Durable loop owners: all 16 fields that move from loose locals in `run_app` into AppState.
const DURABLE_LOOP_OWNERS_2453: [&str; 16] = [
    "ui",
    "known_remote_agents",
    "pending_fwd",
    "needs_resize",
    "last_remote_sync",
    "last_session_save",
    "last_session_json",
    "last_draw",
    "dirty",
    "last_notif_sync",
    "last_decision_sync",
    "pending_decisions_total",
    "booting",
    "boot_start",
    "attaches_expected",
    "restart",
];

fn app_prod_region() -> String {
    // Cut mod.rs at its first #[cfg(test)] so test helpers do not pollute the
    // production region, then append app_state.rs (which holds AppState/impl).
    // Order matters: appending before the cutoff would drop AppState entirely.
    let mut prod = std::fs::read_to_string("src/app/mod.rs")
        .or_else(|_| std::fs::read_to_string("agend-terminal/src/app/mod.rs"))
        .expect("src/app/mod.rs must be readable from test cwd");
    let cutoff = prod.find("#[cfg(test)]").unwrap_or(prod.len());
    prod.truncate(cutoff);
    if let Ok(app_state) = std::fs::read_to_string("src/app/app_state.rs")
        .or_else(|_| std::fs::read_to_string("agend-terminal/src/app/app_state.rs"))
    {
        prod.push('\n');
        prod.push_str(&app_state);
    }
    prod
}

fn struct_body<'a>(source: &'a str, name: &str) -> &'a str {
    let start = source
        .find(name)
        .unwrap_or_else(|| panic!("struct `{name}` must exist in production source"));
    let body_start = source[start..]
        .find('{')
        .map(|offset| start + offset)
        .expect("struct declaration must have a body starting with `{`");
    let body_end = source[body_start..]
        .find("\n}")
        .map(|offset| body_start + offset)
        .expect("unterminated struct body");
    &source[body_start..=body_end]
}

#[test]
fn app_state_owns_all_durable_loop_owners_2453() {
    let prod = app_prod_region();
    assert!(
        prod.contains("struct AppState"),
        "#2453: root `struct AppState` must exist in the app production region"
    );
    let body = struct_body(&prod, "struct AppState");
    for owner in DURABLE_LOOP_OWNERS_2453 {
        let direct = format!("{owner}:");
        let via_restart = matches!(
            owner,
            "restart_outcome" | "restart_probe" | "restart_commit_pending" | "restart"
        ) && body.contains("restart:");
        assert!(
            body.contains(&direct) || via_restart,
            "#2453: AppState must own durable loop owner `{owner}` \
             (directly or via the typed RestartState field)"
        );
    }
}

#[test]
fn restart_state_typed_owner_bounded_2453() {
    let prod = app_prod_region();
    assert!(
        prod.contains("struct RestartState"),
        "#2453: bounded typed `struct RestartState` must exist in the app production region"
    );
    let body = struct_body(&prod, "struct RestartState");
    for field in ["restart_outcome", "restart_probe", "restart_commit_pending"] {
        assert!(
            body.contains(&format!("{field}:")),
            "#2453: RestartState must own `{field}`"
        );
    }
    for other in DURABLE_LOOP_OWNERS_2453 {
        if matches!(
            other,
            "restart_outcome" | "restart_probe" | "restart_commit_pending" | "restart"
        ) {
            continue;
        }
        assert!(
            !body.contains(&format!("{other}:")),
            "#2453: RestartState is BOUNDED to the three restart owners — \
             `{other}` must not migrate into it"
        );
    }
    let app_state = struct_body(&prod, "struct AppState");
    assert!(
        app_state.contains("restart:"),
        "#2453: AppState must hold the typed RestartState sub-owner"
    );
}

#[test]
fn run_app_no_loose_durable_owner_locals_2453() {
    // Only scan `run_app` itself — AppState methods may use short local names
    // like `let needs_resize = outcome...` without re-owning the durable field.
    let region = run_app_region();
    let mut loose: Vec<String> = Vec::new();
    for owner in DURABLE_LOOP_OWNERS_2453 {
        if owner == "restart" {
            continue;
        }
        for needle in [
            format!("let mut {owner} ="),
            format!("let mut {owner}:"),
            format!("let {owner} ="),
            format!("let {owner}:"),
        ] {
            if region.contains(&needle) {
                loose.push(needle);
            }
        }
    }
    assert!(
        loose.is_empty(),
        "#2453: durable loop owners must live in AppState, not as loose \
         run_app locals; found: {loose:?}"
    );
}

#[test]
fn run_app_and_run_core_remain_separate_2453() {
    let prod = app_prod_region();
    assert!(
        prod.contains("fn run_app("),
        "#2453: fn run_app must remain in src/app/mod.rs"
    );
    let daemon = std::fs::read_to_string("src/daemon/mod.rs")
        .or_else(|_| std::fs::read_to_string("agend-terminal/src/daemon/mod.rs"))
        .expect("daemon source must be readable from test cwd");
    assert!(
        daemon.contains("fn run_core("),
        "#2453: fn run_core must remain in src/daemon/mod.rs"
    );
}

fn run_app_region() -> String {
    let source = std::fs::read_to_string("src/app/mod.rs")
        .or_else(|_| std::fs::read_to_string("agend-terminal/src/app/mod.rs"))
        .expect("source file must be readable from test cwd");
    let start = source
        .find("fn run_app(")
        .expect("fn run_app must exist in src/app/mod.rs");
    let end = source[start..]
        .find("\nfn setup_app_bootstrap(")
        .map(|offset| start + offset)
        .expect("fn setup_app_bootstrap must follow run_app");
    source[start..end].to_string()
}

const LOOP_LOGIC_WITNESSES_2453S2: [&str; 5] = [
    "render::drain_all_panes_until",
    "session::save_session_if_changed",
    "pane_factory::create_remote_pane",
    "should_sync_notifications",
    "kill_agent",
];

#[test]
fn run_app_is_thin_orchestration_2453s2() {
    let region = run_app_region();
    let lines = region.lines().count();
    assert!(
        lines <= 90,
        "#2453 Slice 2: run_app must be thin orchestration (~80 lines, cap 90); \
         currently {lines} lines"
    );
}

#[test]
fn loop_logic_lives_in_app_state_methods_2453s2() {
    let prod = app_prod_region();
    let impl_start = prod.find("impl AppState").unwrap_or_else(|| {
        panic!(
            "#2453 Slice 2: `impl AppState` must exist in the app production \
             region — the loop logic has not been extracted into methods"
        )
    });
    // Scan from `impl AppState` to EOF of the production region (do NOT stop
    // at the first nested `}` — that would only cover UiState inside `new()`).
    let body = &prod[impl_start..];
    for witness in LOOP_LOGIC_WITNESSES_2453S2 {
        assert!(
            body.contains(witness),
            "#2453 Slice 2: loop logic `{witness}` must live inside an \
             AppState method, not a free helper (laundering) or inline in run_app"
        );
    }
}

#[test]
fn run_app_witnesses_moved_out_2453s2() {
    let region = run_app_region();
    let still_inline: Vec<&str> = LOOP_LOGIC_WITNESSES_2453S2
        .into_iter()
        .filter(|witness| region.contains(witness))
        .collect();
    assert!(
        still_inline.is_empty(),
        "#2453 Slice 2: loop logic must move out of run_app into AppState \
         methods; still inline: {still_inline:?}"
    );
}

#[test]
fn run_app_keeps_orchestration_skeleton_2453s2() {
    let region = run_app_region();
    for anchor in [
        "let mut state = AppState",
        "crossbeam_channel::select!",
        "app_teardown(",
    ] {
        assert!(
            region.contains(anchor),
            "#2453 Slice 2: run_app must keep the orchestration anchor `{anchor}`"
        );
    }
}

#[test]
fn app_prod_region_bans_ownership_evasion_2453() {
    let prod = app_prod_region();
    for evasion in ["RefCell", "mem::take"] {
        assert!(
            !prod.contains(evasion),
            "#2453: `{evasion}` must not appear in the app production region"
        );
    }
}
