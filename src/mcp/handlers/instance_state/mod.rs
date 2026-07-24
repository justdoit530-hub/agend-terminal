use serde_json::{json, Value};
use std::path::Path;

pub(crate) mod lifecycle;
pub(super) mod spawn;

/// CR-2026-06-14 (resource-leak): upper bound on a team-mode spawn count. A
/// caller-supplied `count` flows into `vec![backend; count]`, so an unbounded
/// value (e.g. a few billion) triggers an enormous allocation → OOM/abort DoS.
/// 64 is already far beyond any real team size; reject above it at the MCP
/// boundary, before the allocation and the CREATE_TEAM RPC.
const MAX_TEAM_COUNT: usize = 64;

pub(super) fn handle_create_instance(
    home: &Path,
    args: &Value,
    instance_name: &str,
    runtime: Option<&super::dispatch::RuntimeContext>,
) -> Value {
    // #2037 (6): name + team = spawn THIS name, then join the team — team-mode
    // used to silently rename to `<team>-N` (the fixup-1 incident). With
    // count>1/backends the names are generated, so an explicit name errors.
    if let (Some(team_name), Some(explicit)) = (
        args.get("team").and_then(|v| v.as_str()),
        args.get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty()),
    ) {
        // H7 (high/security): validate the team name at the MCP boundary. It
        // becomes member names (`<team>-N`) + `workspace_dir(home).join(name)`
        // downstream; `PathBuf::join` keeps `..`, so an unvalidated traversal
        // name like "../../tmp/evil" escapes the workspace root. Reject here,
        // exactly as the single-instance path does.
        crate::validate_name_or_err!(team_name);
        if args.get("count").and_then(|v| v.as_u64()).unwrap_or(1) > 1
            || args.get("backends").is_some()
        {
            return json!({"error": "explicit 'name' with count>1/backends is ambiguous — drop 'name' (generated <team>-N names) or spawn one instance at a time"});
        }
        // Normal single path keeps the explicit name + all single-spawn behavior.
        let mut single = args.clone();
        if let Some(obj) = single.as_object_mut() {
            obj.remove("team");
            obj.remove("count");
        }
        let mut spawned = handle_create_instance(home, &single, instance_name, runtime);
        if spawned.get("error").is_some() {
            return spawned;
        }
        let team_resp = crate::teams::update(home, &json!({"name": team_name, "add": [explicit]}));
        if team_resp.get("error").is_some() {
            // Instance EXISTS — surface the partial state honestly.
            return json!({"name": explicit, "spawned": true, "team": team_name,
                "team_join_error": team_resp["error"].clone()});
        }
        spawned["team"] = json!(team_name);
        spawned["joined_team"] = json!(true);
        return spawned;
    }
    // Team mode: spawn count instances and group them
    if let Some(team_name) = args.get("team").and_then(|v| v.as_str()) {
        // H7 (high/security): validate the team name BEFORE the CREATE_TEAM RPC.
        // `create_team` derives member names `<team>-N` and `workspace_dir(home)
        // .join(name)`; `PathBuf::join` preserves `..`, so an unvalidated name
        // like "../../tmp/evil" creates + registers fleet entries outside the
        // workspace root. The single-instance path already validates; this
        // forwarded the raw name straight to the daemon.
        crate::validate_name_or_err!(team_name);
        let default_backend = args["backend"]
            .as_str()
            .or_else(|| args["command"].as_str())
            .unwrap_or("claude");
        let per_member_backends: Vec<String> = match args.get("backends").and_then(|v| v.as_array())
        {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            None => {
                let count = args.get("count").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
                // CR-2026-06-14 (resource-leak): cap BEFORE the `vec!` allocation
                // — a huge `count` would OOM the daemon at the allocation itself.
                if count > MAX_TEAM_COUNT {
                    return json!({"error": format!(
                        "team count {count} exceeds the maximum {MAX_TEAM_COUNT}"
                    )});
                }
                vec![default_backend.to_string(); count]
            }
        };
        if per_member_backends.is_empty() {
            return json!({"error": "count must be >= 1 (or backends must be non-empty)"});
        }
        // CR-2026-06-14 (resource-leak): also bound the explicit-`backends` path
        // (already materialized by serde, so no OOM here, but enforce the same
        // team-size limit consistently at the boundary).
        if per_member_backends.len() > MAX_TEAM_COUNT {
            return json!({"error": format!(
                "team size {} exceeds the maximum {MAX_TEAM_COUNT}",
                per_member_backends.len()
            )});
        }
        let task = args.get("task").and_then(|v| v.as_str()).map(String::from);
        // #2454 Slice 2: CREATE_TEAM in-process (no socket loopback).
        let params = json!({
            "name": team_name,
            "backends": per_member_backends,
            "description": args.get("description"),
        });
        match super::runtime_bridge::create_team_in_process(home, runtime, &params) {
            Ok(resp) if resp["ok"].as_bool() == Some(true) => {
                let spawned: Vec<String> = resp["result"]["spawned"]
                    .as_array()
                    .or_else(|| resp["spawned"].as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();

                if let Some(task_text) = task {
                    let home = home.to_path_buf();
                    let names = spawned.clone();
                    let registry = runtime.map(|r| r.registry.clone());
                    let externals = runtime.map(|r| r.externals.clone());
                    // fire-and-forget: team task injection waits 3s for agents to
                    // initialize, then injects task text. No JoinHandle needed —
                    // losing the injection on shutdown is acceptable (M5 §10.5).
                    std::thread::Builder::new()
                        .name("team_task_inject".into())
                        .spawn(move || {
                            std::thread::sleep(std::time::Duration::from_secs(3));
                            for inst_name in &names {
                                // #2454: prefer in-process inject_input when runtime
                                // registries were captured; otherwise skip (no socket).
                                if let (Some(reg), Some(ext)) = (&registry, &externals) {
                                    if let Err(e) = crate::agent_ops::inject_input(
                                        reg,
                                        ext,
                                        &home,
                                        inst_name,
                                        &task_text,
                                        false,
                                    ) {
                                        tracing::warn!(
                                            agent = %inst_name,
                                            error = %e.message(),
                                            "team-spawn task inject failed"
                                        );
                                    }
                                }
                            }
                        })
                        .ok();
                }
                let mut result = json!({
                    "team": team_name,
                    "spawned": spawned,
                    "backends": per_member_backends,
                });
                let failed = resp
                    .get("result")
                    .and_then(|r| r.get("failed"))
                    .or_else(|| resp.get("failed"));
                if let Some(failed) = failed {
                    result["failed"] = failed.clone();
                }
                result
            }
            Ok(resp) => {
                json!({"error": resp["error"].as_str().unwrap_or("team creation failed")})
            }
            Err(e) => json!({"error": e}),
        }
    } else {
        spawn::spawn_single_instance(home, instance_name, args, runtime)
    }
}

pub(super) fn handle_delete_instance(
    home: &Path,
    args: &Value,
    runtime: Option<&super::dispatch::RuntimeContext>,
) -> Value {
    let name = match super::require_instance(args) {
        Ok(n) => n,
        Err(e) => return e,
    };
    crate::validate_name_or_err!(name);
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok();
    if let Some(ref config) = fleet {
        if config.channel.is_some()
            && config.instances.contains_key(name)
            && config.instances.len() <= 1
        {
            return json!({"error": "cannot delete the last instance — channel needs at least one instance to receive messages"});
        }
    }
    // Full multi-store teardown lives in the `lifecycle` submodule of this
    // `instance_state` concept (Sprint 54 P1-B Bug 1). #2454: pass runtime
    // so the daemon DELETE step is in-process.
    match lifecycle::full_delete_instance(home, name, runtime) {
        Ok(()) => json!({"name": name}),
        Err(detail) => json!({
            "name": name,
            "error": format!(
                "delete completed with residual state — fleet may resurrect on next reconcile: {detail}"
            ),
        }),
    }
}

pub(super) fn handle_start_instance(
    home: &Path,
    args: &Value,
    runtime: Option<&super::dispatch::RuntimeContext>,
) -> Value {
    let name = match super::require_instance(args) {
        Ok(n) => n,
        Err(e) => return e,
    };
    crate::validate_name_or_err!(name);
    // #1744-PR-B (latch-scope): operator-initiated recovery resets the terminal
    // self-orch once-off latch, so a fresh terminal death after this start re-pages.
    crate::daemon::escalation_persist::clear_failed_escalated(home, name);
    let fleet_path = crate::fleet::fleet_yaml_path(home);
    if !fleet_path.exists() {
        return json!({"error": "No fleet.yaml"});
    }
    let config = match crate::fleet::FleetConfig::load(&fleet_path) {
        Ok(c) => c,
        Err(e) => return json!({"error": format!("fleet.yaml: {e}")}),
    };
    match config.resolve_instance(name) {
        Some(resolved) => {
            let cmd_args = resolved.args.join(" ");
            // #900: forward the resolved env explicitly so the daemon's
            // SPAWN handler doesn't have to re-read fleet.yaml for what
            // we already have in hand. params.env wins over the fleet
            // fallback in handle_spawn, which keeps this RPC the
            // single-source-of-truth for the instance start.
            let env_json = serde_json::to_value(&resolved.env).unwrap_or(serde_json::Value::Null);
            let params = json!({
                "name": name, "backend": resolved.backend_command, "args": cmd_args,
                "mode": "resume",
                "working_directory": resolved.working_directory.map(|p| p.display().to_string()),
                "env": env_json,
            });
            // #2454 Slice 2: SPAWN in-process.
            match super::runtime_bridge::spawn_in_process(home, runtime, &params) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => json!({"name": name}),
                Ok(resp) => {
                    json!({"error": resp["error"].as_str().unwrap_or("spawn failed")})
                }
                Err(e) => json!({"error": e}),
            }
        }
        None => json!({"error": format!("Instance '{name}' not in fleet.yaml")}),
    }
}

pub(super) fn handle_replace_instance(
    home: &Path,
    args: &Value,
    runtime: Option<&super::dispatch::RuntimeContext>,
) -> Value {
    let name = match super::require_instance(args) {
        Ok(n) => n,
        Err(e) => return e,
    };
    crate::validate_name_or_err!(name);
    // #1744-PR-B (latch-scope): operator-initiated recovery resets the terminal
    // self-orch once-off latch, so a fresh terminal death after this replace re-pages.
    crate::daemon::escalation_persist::clear_failed_escalated(home, name);
    let reason = args["reason"].as_str().unwrap_or("manual replacement");

    // Capture backend + working_directory before kill so we can respawn.
    // Prefer fleet.yaml (short name like "claude") over LIST API (which may
    // store a resolved path). SPAWN expects the short preset name.
    let fleet_resolved = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
        .ok()
        .and_then(|f| f.resolve_instance(name));

    // #2454 Slice 2: list snapshot in-process (no LIST socket loopback).
    let list_resp = super::runtime_bridge::list_agents_in_process(home, runtime);
    let (backend, handover) = {
        let fleet_backend = fleet_resolved.as_ref().map(|r| r.backend_command.clone());
        let list_info = list_resp.ok().and_then(|resp| {
            resp["result"]["agents"]
                .as_array()?
                .iter()
                .find(|a| a["name"].as_str() == Some(name))
                .map(|a| {
                    let backend = a["backend"].as_str().unwrap_or("claude").to_string();
                    let handover = format!(
                        "Previous instance state: {}, health: {}. Replaced due to: {reason}",
                        a["agent_state"].as_str().unwrap_or("unknown"),
                        a["health_state"].as_str().unwrap_or("unknown")
                    );
                    (backend, handover)
                })
        });
        let backend = fleet_backend.unwrap_or_else(|| {
            list_info
                .as_ref()
                .map(|(b, _)| b.clone())
                .unwrap_or_else(|| "claude".to_string())
        });
        let handover = list_info
            .map(|(_, h)| h)
            .unwrap_or_else(|| format!("Replaced due to: {reason}"));
        (backend, handover)
    };

    // Resolve working_directory from fleet.yaml (survives kill).
    let working_dir = fleet_resolved
        .and_then(|r| r.working_directory)
        .map(|p| p.display().to_string());

    // Session-reset inbox settle: a replace is always context-lost (fresh
    // spawn), so settle all DELIVERING rows to PROCESSED before the old
    // instance is killed. This prevents reclaim_stale_delivering from
    // reverting them to UNREAD and re-injecting stale messages into the
    // new, context-less session (agend-customization#159).
    crate::inbox::settle_delivering_for_session_reset(home, name);

    // #1366: kill via DELETE with no_wait — sends kill signal and removes
    // registry entry without blocking up to 5 s for child exit. The OS
    // reaps the old process asynchronously; the new spawn gets its own
    // PID / PTY / port with no resource collision.
    // #2454 Slice 2: in-process DELETE (no socket loopback).
    let _ = super::runtime_bridge::delete_in_process(home, runtime, name, true);

    // Enqueue handover context for the new instance.
    persist_or_log!(
        crate::inbox::enqueue(
            home,
            name,
            crate::inbox::InboxMessage {
                schema_version: 0,
                id: None,
                read_at: None,
                delivering_at: None,
                thread_id: None,
                parent_id: None,
                task_id: None,
                force_meta: None,
                correlation_id: None,
                reviewed_head: None,
                from: "system:replace".to_string(),
                text: format!("[handover] {handover}"),
                kind: Some("handover".to_string()),
                timestamp: chrono::Utc::now().to_rfc3339(),
                channel: None,
                delivery_mode: None,
                attachments: vec![],
                in_reply_to_msg_id: None,
                in_reply_to_excerpt: None,
                superseded_by: None,
                delivery_nonce: None,
                review_assignment: None,
                from_id: None,
                broadcast_context: None,
                sequencing: None,
                eta_minutes: None,
                reporting_cadence: None,
                worktree_binding_required: None,
                pr_number: None,
                terminal: None,
            },
        ),
        "replace_instance_handover",
        name
    );

    // Spawn fresh instance with same backend + working directory.
    // #1431: `layout: same-tab` tells the TUI to return the new pane to the
    // tab the replaced pane occupied (recorded on its removal) instead of
    // opening a fresh tab.
    let mut spawn_params = json!({"name": name, "backend": backend, "layout": "same-tab"});
    if let Some(wd) = &working_dir {
        spawn_params["working_directory"] = json!(wd);
    }
    // #2454 Slice 2: SPAWN in-process.
    let spawn_result =
        super::runtime_bridge::spawn_in_process(home, runtime, &spawn_params);

    let spawned = spawn_result
        .as_ref()
        .map(|r| r["ok"].as_bool() == Some(true))
        .unwrap_or(false);

    tracing::info!(%name, %reason, %spawned, "replace_instance");
    json!({"name": name, "reason": reason, "spawned": spawned})
}

/// #1625: assemble the SPAWN params for a restart. Mirrors replace_instance by
/// tagging `layout: same-tab` so the respawned pane returns to the tab the
/// killed pane occupied (recorded on its DELETE) instead of opening a fresh
/// tab. `mode` only toggles backend resume args — placement is identical for
/// resume and fresh restarts — so the hint is applied unconditionally.
fn restart_spawn_params(
    name: &str,
    backend_command: &str,
    args: &[String],
    working_directory: Option<&Path>,
    env: &std::collections::HashMap<String, String>,
    mode: &str,
) -> Value {
    let mut spawn_params = json!({
        "name": name,
        "backend": backend_command,
        "args": args.join(" "),
        "working_directory": working_directory.map(|p| p.display().to_string()),
        "env": serde_json::to_value(env).unwrap_or(serde_json::Value::Null),
        "layout": "same-tab",
    });
    if mode == "resume" {
        spawn_params["mode"] = json!("resume");
    } else {
        // fresh restart only: arm the daemon's first-turn self-kick so the
        // respawned (context-lost) instance runs its recovery sequence instead of
        // sitting idle until an operator happens to type (the overnight
        // restart-strands-the-fleet failure). INDEPENDENT flag — the SPAWN handler
        // must NOT derive self-kick from SpawnMode::Fresh, which initial fleet
        // spawns also map to; only THIS restart-fresh path sets it.
        spawn_params["self_kick_on_ready"] = json!(true);
    }
    spawn_params
}

/// Grace ceiling for [`await_unsent_draft_or_grace`]: even while the operator
/// keeps typing, force the restart after this long so a context-full / stuck
/// agent can't be deferred indefinitely. The primary release is the operator
/// submitting (draft clears well before this); the ceiling only bounds the
/// pathological continuous-typing case. Tunable.
const RESTART_DRAFT_GRACE: std::time::Duration = std::time::Duration::from_secs(60);
/// Re-check cadence while deferring — silent (no per-poll event / nudge).
const RESTART_DRAFT_POLL: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Debug, PartialEq, Eq)]
enum DraftGate {
    Proceed,
    Defer,
}

/// Pure restart-gate decision (unit-tested exhaustively): proceed with the kill
/// iff `force`, there is no live operator draft, or the grace ceiling has
/// elapsed; otherwise keep deferring. Kept pure (no clock / no IO) so the whole
/// decision matrix is deterministic without real sleeps.
fn restart_draft_gate(
    force: bool,
    has_live_draft: bool,
    elapsed: std::time::Duration,
    grace: std::time::Duration,
) -> DraftGate {
    if force || !has_live_draft || elapsed >= grace {
        DraftGate::Proceed
    } else {
        DraftGate::Defer
    }
}

/// Block the restart while the operator has unsent keystrokes in `name`'s input
/// line, releasing the instant the draft is submitted/cleared or after
/// [`RESTART_DRAFT_GRACE`]. Emits exactly two log lines (defer-start, proceed) —
/// no per-poll noise. Thread-safety rationale is at the call site.
fn await_unsent_draft_or_grace(home: &Path, name: &str, force: bool) {
    if restart_draft_gate(
        force,
        crate::inbox::notify::operator_has_live_draft(home, name),
        std::time::Duration::ZERO,
        RESTART_DRAFT_GRACE,
    ) == DraftGate::Proceed
    {
        return; // fast path: force, or no live draft — no wait, no log.
    }
    tracing::info!(%name, "restart deferred: operator has an unsent draft in the input line");
    let start = std::time::Instant::now();
    while restart_draft_gate(
        force,
        crate::inbox::notify::operator_has_live_draft(home, name),
        start.elapsed(),
        RESTART_DRAFT_GRACE,
    ) == DraftGate::Defer
    {
        std::thread::sleep(RESTART_DRAFT_POLL);
    }
    tracing::info!(
        %name,
        elapsed_ms = start.elapsed().as_millis() as u64,
        "restart proceeding: draft submitted/cleared or grace ceiling reached"
    );
}

pub(super) fn handle_restart_instance(
    home: &Path,
    args: &Value,
    runtime: Option<&super::dispatch::RuntimeContext>,
) -> Value {
    let name = match super::require_instance(args) {
        Ok(n) => n,
        Err(e) => return e,
    };
    crate::validate_name_or_err!(name);
    // #1744-PR-B (latch-scope): operator-initiated recovery resets the terminal
    // self-orch once-off latch, so a fresh terminal death after this restart re-pages.
    crate::daemon::escalation_persist::clear_failed_escalated(home, name);
    let reason = args["reason"].as_str().unwrap_or("manual restart");
    let mode = args["mode"].as_str().unwrap_or("resume");

    // #2476: a `fresh` restart DROPS the agent's in-memory context (that is its
    // value — it releases a stale prompt cache while a dev idles waiting on
    // review/CI). But fresh-restart-as-routine must not silently discard
    // UNCOMMITTED groundwork in the agent's bound worktree. Pre-flight: if the
    // bound worktree has uncommitted changes, refuse unless `force:true`, telling
    // the caller to push / leave a board handoff first. `resume` is unaffected
    // (it keeps context), and an unbound agent has no worktree to protect.
    if mode != "resume" && !args["force"].as_bool().unwrap_or(false) {
        if let Some(wt) = crate::binding::read(home, name)
            .and_then(|b| b["worktree"].as_str().map(std::path::PathBuf::from))
        {
            if wt.exists() && crate::worktree::has_uncommitted_changes(&wt) {
                return json!({
                    "error": "refusing fresh restart: bound worktree has uncommitted changes \
                              that a context drop would strand. Commit/push (or leave a task-board \
                              handoff) first, then retry — or pass force:true to drop context anyway.",
                    "name": name,
                    "worktree": wt.display().to_string(),
                    "code": "uncommitted_work_at_risk",
                });
            }
        }
    }

    let fleet_path = crate::fleet::fleet_yaml_path(home);
    let config = match crate::fleet::FleetConfig::load(&fleet_path) {
        Ok(c) => c,
        Err(e) => return json!({"error": format!("fleet.yaml: {e}")}),
    };
    let resolved = match config.resolve_instance(name) {
        Some(r) => r,
        None => return json!({"error": format!("Instance '{name}' not in fleet.yaml")}),
    };

    // t-95913-5: the operator's unsent keystrokes live ONLY in the input line of
    // the process we're about to kill — a fresh OR resume restart destroys them
    // (`--continue` restores the conversation, not the input line). If the pane
    // has a live draft, defer the kill until the operator submits (draft clears)
    // or a grace ceiling elapses (so continuous typing can't defer forever).
    // Mode-agnostic; `force:true` bypasses. Safe to block here: each api tool call
    // runs on its own `api_handler` thread (`api::serve` per-session spawn), and
    // the operator's submit arrives via the TUI write path, not this thread.
    await_unsent_draft_or_grace(home, name, args["force"].as_bool().unwrap_or(false));

    // Session-reset inbox settle: for a FRESH restart (context-lost), settle
    // all DELIVERING rows to PROCESSED before killing the old instance.
    // Resume restarts preserve context → the implicit next-drain ack (A)
    // handles it; settle would prematurely close messages the resumed agent
    // still has in context. (agend-customization#159)
    if mode != "resume" {
        crate::inbox::settle_delivering_for_session_reset(home, name);
    }

    // #2454 Slice 2: DELETE + SPAWN in-process (no socket loopback).
    let _ = super::runtime_bridge::delete_in_process(home, runtime, name, true);

    let spawn_params = restart_spawn_params(
        name,
        &resolved.backend_command,
        &resolved.args,
        resolved.working_directory.as_deref(),
        &resolved.env,
        mode,
    );

    let spawn_result =
        super::runtime_bridge::spawn_in_process(home, runtime, &spawn_params);
    let spawned = spawn_result
        .as_ref()
        .map(|r| r["ok"].as_bool() == Some(true))
        .unwrap_or(false);

    tracing::info!(%name, %reason, %mode, %spawned, "restart_instance");
    json!({"name": name, "reason": reason, "mode": mode, "spawned": spawned})
}

/// #t-777-3: daemon-autonomic self-heal entry — the respawn-stuck watchdog's
/// narrow path to a **Fresh** restart. Wraps `handle_restart_instance(mode=fresh)`,
/// which round-trips the PROVEN direct `DELETE`(no_wait)+`SPAWN` api::calls →
/// `ApiEvent::InstanceCreated` → app pane Fresh respawn (the same path the
/// operator's manual `restart_instance fresh` takes, working in the live
/// app-mode daemon where the crash_tx→respawn machinery is inert).
///
/// **Gate-exempt BY CONSTRUCTION** (no new operator-gate surface): the inner
/// `DELETE`/`SPAWN` are DIRECT api methods — operator-transport, which
/// `operator_gate::check_operation_allowed` returns `Ok` for before `classify`
/// is consulted. Reached ONLY from the per-tick hang-detection watchdog (never
/// agent-invocable), so the narrowness is enforced by the trigger, exactly like
/// crash-respawn / hang-recovery (`operator_gate` module scope note). Returns
/// whether the SPAWN succeeded so the caller can escalate a failed recovery.
pub(crate) fn restart_instance_autonomic(home: &Path, name: &str, reason: &str) -> bool {
    let result = handle_restart_instance(
        home,
        &json!({"name": name, "mode": "fresh", "reason": reason}), None);
    result
        .get("spawned")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

pub(super) fn resolve_team_layout(
    home: &Path,
    name: &str,
    layout_arg: Option<&serde_json::Value>,
    target_pane_arg: Option<&serde_json::Value>,
) -> (&'static str, Option<String>) {
    let caller_set_layout = layout_arg.and_then(|v| v.as_str()).is_some();
    let caller_set_target = target_pane_arg
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .is_some();
    if !caller_set_layout && !caller_set_target {
        if let Some(team) = crate::teams::find_team_for(home, name) {
            let anchor = team.orchestrator.or_else(|| team.members.first().cloned());
            return ("split-right", anchor);
        }
    }
    let layout = layout_arg.and_then(|v| v.as_str()).unwrap_or("tab");
    let target = target_pane_arg
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    let layout = match layout {
        "split-right" => "split-right",
        "split-below" => "split-below",
        _ => "tab",
    };
    (layout, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn dirty_worktree(tag: &str) -> std::path::PathBuf {
        let wt = std::env::temp_dir().join(format!(
            "agend-2476-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&wt).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&wt)
                .env("AGEND_GIT_BYPASS", "1")
                .output()
                .expect("git");
        };
        git(&["init", "-b", "main"]);
        // Untracked file → `git status --porcelain` non-empty → work-at-risk.
        std::fs::write(wt.join("wip.txt"), "uncommitted groundwork").unwrap();
        wt
    }

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn bind_worktree(home: &std::path::Path, agent: &str, wt: &std::path::Path) {
        let dir = crate::paths::runtime_dir(home).join(agent);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("binding.json"),
            serde_json::json!({"worktree": wt.display().to_string()}).to_string(),
        )
        .unwrap();
    }

    /// #2476: a `fresh` restart must refuse when the bound worktree has
    /// uncommitted changes (context drop would strand them), unless `force`.
    /// `resume` keeps context so it is never guarded.
    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn fresh_restart_guards_uncommitted_worktree_2476() {
        let home = std::env::temp_dir().join(format!(
            "agend-2476-home-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let wt = dirty_worktree("wt");
        bind_worktree(&home, "dev", &wt);

        // fresh + dirty + no force → refused at the guard (before any spawn).
        let refused = handle_restart_instance(
            &home,
            &serde_json::json!({"instance": "dev", "mode": "fresh"}), None);
        assert_eq!(
            refused["code"], "uncommitted_work_at_risk",
            "got: {refused}"
        );

        // force bypasses the guard (proceeds past it — a later error is NOT the guard).
        let forced = handle_restart_instance(
            &home,
            &serde_json::json!({"instance": "dev", "mode": "fresh", "force": true}), None);
        assert_ne!(
            forced["code"], "uncommitted_work_at_risk",
            "force must bypass: {forced}"
        );

        // resume keeps context → never guarded.
        let resumed = handle_restart_instance(
            &home,
            &serde_json::json!({"instance": "dev", "mode": "resume"}), None);
        assert_ne!(
            resumed["code"], "uncommitted_work_at_risk",
            "resume must not guard: {resumed}"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&wt).ok();
    }

    // #1625: every restart, regardless of mode, must carry the same-tab layout
    // hint so the respawned pane returns to its original tab (the fresh path
    // previously omitted it and fell out into a new tab).
    #[test]
    fn restart_spawn_params_carries_same_tab_fresh() {
        let env = HashMap::new();
        let p = restart_spawn_params("dev", "claude", &[], None, &env, "fresh");
        assert_eq!(p["layout"], "same-tab");
        // fresh must NOT request a resume.
        assert!(p.get("mode").is_none());
        // fresh restart arms the daemon self-kick (the independent flag).
        assert_eq!(p["self_kick_on_ready"].as_bool(), Some(true));
    }

    #[test]
    fn restart_spawn_params_carries_same_tab_resume() {
        let env = HashMap::new();
        let p = restart_spawn_params("dev", "claude", &[], None, &env, "resume");
        assert_eq!(p["layout"], "same-tab");
        assert_eq!(p["mode"], "resume");
        // resume preserves context → must NOT self-kick.
        assert!(p.get("self_kick_on_ready").is_none());
    }

    /// must-follow ②: the self-kick flag is INDEPENDENT — set ONLY by the
    /// fresh-restart path, NEVER derived from `SpawnMode::Fresh` (initial fleet
    /// spawns / create_instance / team-spawn are Fresh too but never set it). The
    /// SPAWN handler reads `self_kick_on_ready` with `unwrap_or(false)`, so any
    /// spawn-params shape that lacks the flag (every non-restart-fresh spawn)
    /// gates the self-kick OUT, fail-safe.
    #[test]
    fn self_kick_flag_set_only_by_fresh_restart_fail_safe_default() {
        let env = HashMap::new();
        // fresh restart → flag present + true.
        let fresh = restart_spawn_params("dev", "claude", &[], None, &env, "fresh");
        assert!(fresh["self_kick_on_ready"].as_bool().unwrap_or(false));
        // resume restart → no flag → reads false.
        let resume = restart_spawn_params("dev", "claude", &[], None, &env, "resume");
        assert!(!resume["self_kick_on_ready"].as_bool().unwrap_or(false));
        // a generic spawn-params object (the initial-fleet / create_instance shape,
        // which also maps to SpawnMode::Fresh) carries no flag → reads false.
        let initial = json!({"name": "dev", "backend": "claude", "layout": "tab"});
        assert!(!initial["self_kick_on_ready"].as_bool().unwrap_or(false));
    }

    // ── t-95913-5: defer fresh/resume restart while the operator has an unsent
    // draft in the target pane's input line. The pure `restart_draft_gate` covers
    // the whole decision matrix deterministically (no clock, no sleep). ──

    #[test]
    fn restart_draft_gate_proceeds_when_no_live_draft() {
        // No unsent draft → kill immediately (gate is a no-op), any elapsed.
        assert_eq!(
            restart_draft_gate(false, false, std::time::Duration::ZERO, RESTART_DRAFT_GRACE),
            DraftGate::Proceed
        );
    }

    #[test]
    fn restart_draft_gate_defers_live_draft_within_grace() {
        // Live draft, still inside the grace window → defer the kill.
        assert_eq!(
            restart_draft_gate(
                false,
                true,
                std::time::Duration::from_secs(10),
                RESTART_DRAFT_GRACE
            ),
            DraftGate::Defer
        );
    }

    #[test]
    fn restart_draft_gate_proceeds_when_grace_ceiling_reached() {
        // Live draft but grace elapsed → force the kill so continuous typing can't
        // defer forever. Boundary: elapsed == grace proceeds.
        assert_eq!(
            restart_draft_gate(false, true, RESTART_DRAFT_GRACE, RESTART_DRAFT_GRACE),
            DraftGate::Proceed
        );
        assert_eq!(
            restart_draft_gate(
                false,
                true,
                std::time::Duration::from_secs(61),
                RESTART_DRAFT_GRACE
            ),
            DraftGate::Proceed
        );
    }

    #[test]
    fn restart_draft_gate_force_bypasses_live_draft() {
        // force:true → kill immediately even with a live draft inside the window.
        assert_eq!(
            restart_draft_gate(true, true, std::time::Duration::ZERO, RESTART_DRAFT_GRACE),
            DraftGate::Proceed
        );
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn operator_has_live_draft_reflects_unsent_keystrokes() {
        let home = std::env::temp_dir().join(format!(
            "agend-live-draft-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        // No metadata → no draft.
        assert!(!crate::inbox::notify::operator_has_live_draft(&home, "a"));
        // A keystroke with no following submit → a live unsent draft.
        crate::notification_queue::record_input_activity(&home, "a");
        assert!(crate::inbox::notify::operator_has_live_draft(&home, "a"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn await_unsent_draft_or_grace_returns_fast_without_a_draft() {
        // No draft → the fast path returns immediately (no block). A regression that
        // dropped the no-draft check would hang this test to the nextest timeout.
        let home = std::env::temp_dir().join(format!(
            "agend-await-nodraft-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        await_unsent_draft_or_grace(&home, "a", false);
        std::fs::remove_dir_all(&home).ok();
    }
}
