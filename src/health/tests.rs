//! Tests for src/health.rs — extracted to a sibling file (anti-monolith split).
//! Test modules are exempt from the LOC ceiling; see tests/src_file_size_invariant.rs.

use super::*;

/// #1744-H2 windows-safe past `Instant`: built via the wall-clock projection
/// (`epoch_ms_to_instant`) instead of `Instant::now() - Duration::from_secs(n)`,
/// which UNDERFLOWS (panics) on a freshly-booted VM whose monotonic clock has
/// run for < `secs` (the test-side underflow class; production was root-fixed
/// in #1836). `epoch_ms_to_instant` uses `checked_sub` → never panics; on a
/// normal-uptime runner it is a real `secs`-ago instant, on a sub-`secs`-uptime
/// VM it clamps to now.
fn ago(secs: u64) -> Instant {
    epoch_ms_to_instant(crate::daemon::heartbeat_pair::now_ms().saturating_sub(secs * 1000))
}

/// [M1] §3.9: `total_crashes` decay is 1 unit per STABILITY_WINDOW, NOT a
/// per-tick burst. `maybe_decay` runs every ~10s; before the fix, once one
/// 30-min window elapsed every subsequent call decremented, draining the whole
/// budget in seconds (defeating the max-retries crash-loop cap for a
/// slow-flapping agent). Repeated calls within ONE window must decay ≤1.
/// (Uses `base + offset` — forward Instants never underflow on windows.)
#[test]
fn crash_decay_one_unit_per_window_not_tick_burst_m1() {
    let mut h = HealthTracker::new();
    let base = Instant::now();
    h.total_crashes = 4;
    for _ in 0..4 {
        h.crash_times.push_back(base);
    }
    h.state = HealthState::Unstable;

    // Many ticks within the SAME (first) window → at most ONE decay.
    let t1 = base + Duration::from_secs(31 * 60);
    for _ in 0..10 {
        h.maybe_decay_at(t1, true);
    }
    assert_eq!(
        h.total_crashes, 3,
        "[M1] repeated ticks within one window decay at most 1 unit (not burst to 0)"
    );

    // A later window → exactly one more.
    let t2 = base + Duration::from_secs(62 * 60);
    for _ in 0..10 {
        h.maybe_decay_at(t2, true);
    }
    assert_eq!(
        h.total_crashes, 2,
        "[M1] the next window decays exactly 1 more"
    );
}

/// [M1] §3.9: the same 1-unit-per-window discipline for `recovery_restart_count`
/// (decay anchored on `last_stage2_fired_at`, advanced on each decrement).
#[test]
fn recovery_restart_count_decay_one_unit_per_window_m1() {
    let mut h = HealthTracker::new();
    let base = Instant::now();
    h.recovery_restart_count = 3;
    h.last_stage2_fired_at = Some(base);

    let t1 = base + Duration::from_secs(31 * 60);
    for _ in 0..10 {
        h.maybe_decay_at(t1, true);
    }
    assert_eq!(
        h.recovery_restart_count, 2,
        "[M1] recovery count decays at most 1 within one window"
    );

    let t2 = base + Duration::from_secs(62 * 60);
    for _ in 0..10 {
        h.maybe_decay_at(t2, true);
    }
    assert_eq!(
        h.recovery_restart_count, 1,
        "[M1] the next window decays exactly 1 more"
    );
}

/// #1701 ① + ②: a self-orchestrator crash escalates on the FIRST crash
/// (unlike `record_crash`'s recent>=2 `should_notify`), and the cooldown
/// blocks an immediate re-fire so a crash-loop can't spam the operator.
#[test]
fn self_orch_crash_first_fires_then_cooldown_blocks_1701() {
    let mut h = HealthTracker::new();
    // ① single crash → fires (fresh tracker, no prior notification).
    assert!(
        h.self_orch_crash_should_notify(),
        "#1701: a self-orchestrator's FIRST crash must escalate (no recent>=2 gate)"
    );
    // ② immediate re-fire (crash-loop) → suppressed by the NOTIFY_COOLDOWN
    // stamp from the first fire.
    assert!(
        !h.self_orch_crash_should_notify(),
        "#1701: a re-fire within NOTIFY_COOLDOWN must be suppressed (no crash-loop spam)"
    );
}

/// #1701 contrast: the generic crash gate stays recent>=2 — a regular
/// agent's FIRST crash is silent (it has peers + self-heals). This pins that
/// the #1701 self-orch path did NOT loosen the generic path.
#[test]
fn generic_first_crash_still_silent_1701() {
    let mut h = HealthTracker::new();
    let (_respawn, _delay, notify) = h.record_crash();
    assert!(
        !notify,
        "#1701: a non-orchestrator's first crash stays silent"
    );
}

/// #1701 Hung ① + ⑤: a self-orchestrator Hung past the confirm-window
/// escalates, and the cooldown blocks an immediate re-page while it persists.
#[test]
fn hung_escalation_fires_after_window_then_cooldown_blocks_1701() {
    let window = Duration::from_secs(60);
    let mut h = HealthTracker::new();
    h.state = HealthState::Hung;
    h.hung_since = Some(ago(61)); // sustained > window
    assert!(
        h.hung_escalation_due(window),
        "#1701: Hung sustained past the confirm-window must escalate"
    );
    assert!(
        !h.hung_escalation_due(window),
        "#1701: re-page within NOTIFY_COOLDOWN must be suppressed"
    );
}

/// #1701 Hung ②: a transient Hung (under the confirm-window) does NOT escalate
/// — this is the FP filter (F39/F10/keystroke-draining blips).
#[test]
fn hung_escalation_not_due_within_window_1701() {
    let mut h = HealthTracker::new();
    h.state = HealthState::Hung;
    h.hung_since = Some(Instant::now()); // just entered, 0 elapsed
    assert!(
        !h.hung_escalation_due(Duration::from_secs(60)),
        "#1701: Hung shorter than the confirm-window must NOT escalate"
    );
}

/// #1701 Hung ③: a non-Hung state (e.g. IdleLong — the 04:00 idle false-alarm
/// the Hung/IdleLong split already excludes) never escalates, even if a stale
/// `hung_since` lingers.
#[test]
fn hung_escalation_not_due_when_not_hung_1701() {
    let mut h = HealthTracker::new();
    h.state = HealthState::IdleLong;
    h.hung_since = Some(ago(300));
    assert!(
        !h.hung_escalation_due(Duration::from_secs(60)),
        "#1701: only HealthState::Hung escalates (IdleLong is the 348-FP case)"
    );
}

// ── #1744-H3: per-class cooldown split ──

/// #1744-H3: a crash page and a hung page no longer share one cooldown, so a
/// recent crash escalation must NOT suppress a due hung escalation (and vice
/// versa). Pre-#1744 both stamped/read `last_notification`, so within 300s the
/// first fired suppressed the other.
#[test]
fn cooldown_split_independent_crash_hung_1744_h3() {
    // crash page fires + stamps the crash cooldown.
    let mut h = HealthTracker::new();
    assert!(h.self_orch_crash_should_notify(), "first crash escalates");
    assert!(
        !h.self_orch_crash_should_notify(),
        "same-class re-fire within cooldown is suppressed"
    );
    // A hung escalation that is due must STILL fire — its cooldown is separate.
    h.state = HealthState::Hung;
    h.hung_since = Some(ago(61));
    assert!(
        h.hung_escalation_due(Duration::from_secs(60)),
        "#1744-H3: a recent CRASH page must not suppress a due HUNG page"
    );

    // Symmetric: a hung page must not suppress a crash page.
    let mut h2 = HealthTracker::new();
    h2.state = HealthState::Hung;
    h2.hung_since = Some(ago(61));
    assert!(
        h2.hung_escalation_due(Duration::from_secs(60)),
        "hung fires"
    );
    assert!(
        h2.self_orch_crash_should_notify(),
        "#1744-H3: a recent HUNG page must not suppress a crash page"
    );
}

// ── #1744-H2: persist / rehydrate escalation state across restart ──

/// #1744-H2 (pure): an `Instant` projected to wall-clock epoch-ms and back
/// round-trips to within a small slop, so a cooldown/anchor keeps its real
/// age across a (simulated) restart.
#[test]
fn instant_epoch_ms_round_trip_1744_h2() {
    let original = ago(120);
    let restored = super::epoch_ms_to_instant(super::instant_to_epoch_ms(original));
    let drift = restored
        .elapsed()
        .as_millis()
        .abs_diff(original.elapsed().as_millis());
    assert!(drift < 2_000, "round-trip drift {drift}ms too large");
}

/// #1744-H2: snapshot→rehydrate preserves the escalation semantics across a
/// simulated daemon restart — a recently-stamped cooldown STILL suppresses, a
/// crash older than `CRASH_WINDOW` is pruned, `total_crashes` is restored, and
/// the Hung confirm-window anchor keeps its real age (does NOT reset to 0).
#[test]
fn escalation_snapshot_rehydrate_round_trip_1744_h2() {
    // #1744-H2: build the persisted snapshot directly from wall-clock
    // epoch-ms. The previous version fabricated the "before" state via
    // `Instant::now() - Duration::from_secs(700)`, which UNDERFLOWS (panics)
    // on a freshly-booted windows VM whose monotonic clock has run for less
    // than 700s — the windows-only CI panic this fixes. Rehydrate prunes /
    // projects via wall-clock, so the round-trip is verifiable without
    // depending on machine uptime.
    let now = crate::daemon::heartbeat_pair::now_ms();
    let snap = PersistedEscalation {
        total_crashes: 2,
        // One in-window crash (10s ago) + one stale (700s ago > 600s window).
        crash_times_epoch_ms: vec![now.saturating_sub(700_000), now.saturating_sub(10_000)],
        last_crash_notification_epoch_ms: Some(now.saturating_sub(100_000)), // within 300s
        last_hung_notification_epoch_ms: None,
        hung_since_epoch_ms: Some(now.saturating_sub(45_000)),
        failed_escalated: false,
    };

    // Simulate restart: a brand-new tracker, then rehydrate.
    let mut after = HealthTracker::new();
    after.rehydrate_escalation(&snap);

    assert_eq!(after.total_crashes, 2, "crash budget restored");
    assert_eq!(
        after.crash_times.len(),
        1,
        "#1744-H2: a crash older than CRASH_WINDOW is pruned on rehydrate (wall-clock)"
    );
    assert!(
        !cooldown_elapsed(after.last_crash_notification),
        "#1744-H2: a cooldown stamped 100s ago must STILL suppress after restart (no duplicate P0)"
    );
    // #1744-H2: the confirm-window anchor keeps its real age across the
    // round-trip (NOT reset to 0). Asserted via WALL-CLOCK — re-snapshot the
    // rehydrated tracker and check the projected epoch age — so it does not
    // depend on the monotonic clock's ability to represent 45s in the past
    // (the same property that lets this run on a freshly-booted VM).
    let resnap = after.escalation_snapshot();
    let hung_ms = resnap
        .hung_since_epoch_ms
        .expect("confirm-window anchor restored, not dropped");
    let age_ms = crate::daemon::heartbeat_pair::now_ms().saturating_sub(hung_ms);
    assert!(
            age_ms >= 40_000,
            "#1744-H2: the anchor keeps its ~45s age across snapshot→rehydrate (not reset to 0), got {age_ms}ms"
        );
}

/// #1744-H2 (windows underflow root-fix): the rehydrate/snapshot helpers must
/// NOT panic when a stamp reads in the SAME coarse-clock tick as `now`
/// (b >= a) — the windows `Instant`/`Duration` underflow class. Exercised
/// directly with b>=a / future stamps so the underflow path is covered
/// deterministically, without relying on a freshly-booted VM's clock
/// granularity. Fine-clock behaviour is unchanged (b>=a → 0 elapsed anyway).
#[test]
fn rehydrate_helpers_saturate_on_coarse_clock_1744_h2() {
    let now_ms = crate::daemon::heartbeat_pair::now_ms();

    // A same-tick instant projects to ~now (saturating elapsed = 0), no panic.
    let e = instant_to_epoch_ms(Instant::now());
    assert!(
        e + 5_000 >= now_ms && now_ms + 5_000 >= e,
        "same-tick instant projects to ~now, got {e} vs {now_ms}"
    );

    // A FUTURE epoch stamp (clock skew, b >= a) clamps to ~now, never panics.
    let future = epoch_ms_to_instant(now_ms + 60_000);
    assert!(
        Instant::now().saturating_duration_since(future) < Duration::from_secs(1),
        "a future stamp clamps to ~now (not projected into the past)"
    );

    // A cooldown stamped in the current tick reads as "not yet elapsed".
    assert!(
        !cooldown_elapsed(Some(Instant::now())),
        "a same-tick cooldown stamp must read as not-elapsed, not underflow"
    );

    // A full rehydrate of an all-now / future snapshot must not panic.
    let snap = PersistedEscalation {
        total_crashes: 1,
        crash_times_epoch_ms: vec![now_ms, now_ms + 30_000],
        last_crash_notification_epoch_ms: Some(now_ms),
        last_hung_notification_epoch_ms: Some(now_ms + 5_000),
        hung_since_epoch_ms: Some(now_ms),
        failed_escalated: false,
    };
    let mut h = HealthTracker::new();
    h.rehydrate_escalation(&snap); // must not panic
    assert_eq!(h.total_crashes, 1, "rehydrate completed without underflow");
}

/// #1744-H2: the crash budget survives a restart, so an agent that has already
/// burned most of `max_retries` reaches `Failed` on the next crash instead of
/// resetting to 0 and respawning forever (the restart-reset gap).
#[test]
fn rehydrate_preserves_crash_budget_to_failed_1744_h2() {
    let mut before = HealthTracker::new();
    before.total_crashes = DEFAULT_MAX_RETRIES - 1;
    let snap = before.escalation_snapshot();

    let mut after = HealthTracker::new();
    after.rehydrate_escalation(&snap);
    assert_eq!(after.total_crashes, DEFAULT_MAX_RETRIES - 1);

    // The next crash crosses the budget → Failed, no respawn.
    let (respawn, _delay, notify) = after.record_crash();
    assert!(
        !respawn,
        "#1744-H2: budget survived restart → terminal Failed, not infinite respawn"
    );
    assert!(notify, "terminal Failed returns should_notify=true");
    assert_eq!(after.state, HealthState::Failed);
}

/// #1744-H2 (C): a `hung_since` rehydrated across a restart SURVIVES the first
/// post-restart Hung re-entry (`get_or_insert`, not `= now`) so the
/// confirm-window continues; and a genuine Hung EXIT clears it so a recovered
/// episode's anchor never lingers / is persisted stale.
#[test]
fn hung_since_survives_reentry_and_clears_on_exit_1744_h2() {
    let mut h = HealthTracker::new();
    // Rehydrated anchor from before the restart (agent spawns Healthy).
    h.hung_since = Some(ago(50));
    assert_eq!(h.state, HealthState::Healthy);

    // First post-restart Hung detection (E1: silent past threshold + input
    // pending past heartbeat). Must KEEP the rehydrated anchor.
    let entered = h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0),
        1_000_000,
        0,
    );
    assert!(entered, "agent classified Hung");
    assert_eq!(h.state, HealthState::Hung);
    assert!(
            h.hung_since.expect("anchor kept").elapsed() >= Duration::from_secs(45),
            "#1744-H2: get_or_insert preserves the rehydrated anchor (confirm-window continues, not reset to 0)"
        );

    // Silence drops (1 byte of output) → Hung EXIT → anchor cleared.
    let still = h.check_hang(
        AgentState::Idle,
        Duration::from_secs(0),
        Duration::from_secs(0),
        1_000_000,
        0,
    );
    assert!(!still);
    assert_eq!(h.state, HealthState::Healthy);
    assert!(
        h.hung_since.is_none(),
        "#1744-H2: Hung exit must clear the anchor (no stale persist)"
    );
}

/// #1744-H2 (codex HIGH regression): persisting a Hung anchor then RECOVERING
/// must propagate the CLEAR to the snapshot — so a restart rehydrates `None`,
/// not the stale anchor. Otherwise the next (unrelated) Hung re-entry's
/// `get_or_insert` would keep the stale, already-elapsed anchor and
/// `hung_escalation_due` would fire immediately (a false escalation).
#[test]
fn cleared_hung_anchor_snapshots_as_none_no_false_escalation_1744_h2() {
    // 1) Enter Hung → anchor set; the snapshot carries it.
    let mut h = HealthTracker::new();
    assert!(h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0),
        1_000_000,
        0,
    ));
    assert!(h.escalation_snapshot().hung_since_epoch_ms.is_some());

    // 2) Recover (silence drops) → Hung exit clears the anchor → the snapshot
    //    we would persist now carries None.
    assert!(!h.check_hang(
        AgentState::Idle,
        Duration::from_secs(0),
        Duration::from_secs(0),
        1_000_000,
        0,
    ));
    let cleared = h.escalation_snapshot();
    assert!(
        cleared.hung_since_epoch_ms.is_none(),
        "#1744-H2: the cleared snapshot must carry hung_since=None"
    );

    // 3) Restart: rehydrate the cleared snapshot → no anchor.
    let mut after = HealthTracker::new();
    after.rehydrate_escalation(&cleared);
    assert!(
        after.hung_since.is_none(),
        "rehydrated cleared anchor is None"
    );

    // 4) A later, unrelated Hung episode anchors FRESH (≈now) → not yet due,
    //    so no false immediate escalation (the bug had it fire at once).
    assert!(after.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0),
        1_000_000,
        0,
    ));
    assert!(
            !after.hung_escalation_due(Duration::from_secs(60)),
            "#1744-H2: a fresh post-recovery Hung must NOT escalate immediately (stale anchor would have)"
        );
}

#[test]
fn test_first_crash_silent() {
    let mut h = HealthTracker::new();
    let (respawn, _delay, notify) = h.record_crash();
    assert!(respawn);
    assert!(!notify); // 1st crash = silent
    assert_eq!(h.state, HealthState::Recovering);
}

#[test]
fn test_second_crash_notifies() {
    let mut h = HealthTracker::new();
    h.record_crash();
    let (respawn, _delay, notify) = h.record_crash();
    assert!(respawn);
    assert!(notify); // 2nd crash = notify
}

#[test]
fn test_unstable_after_three() {
    let mut h = HealthTracker::new();
    h.record_crash();
    h.record_crash();
    h.record_crash();
    assert_eq!(h.state, HealthState::Unstable);
}

#[test]
fn test_failed_after_max_retries() {
    let mut h = HealthTracker::new();
    for _ in 0..5 {
        h.record_crash();
    }
    assert_eq!(h.state, HealthState::Failed);
    let (respawn, _, _) = h.record_crash();
    assert!(!respawn); // Failed state = no more respawn
}

#[test]
fn test_backoff_exponential() {
    let mut h = HealthTracker::new();
    h.record_crash();
    assert_eq!(h.backoff_delay(), Duration::from_secs(5));
    h.record_crash();
    assert_eq!(h.backoff_delay(), Duration::from_secs(10));
    h.record_crash();
    assert_eq!(h.backoff_delay(), Duration::from_secs(20));
    h.record_crash();
    assert_eq!(h.backoff_delay(), Duration::from_secs(40));
}

#[test]
fn test_error_loop() {
    let mut h = HealthTracker::new();
    assert!(!h.record_error(AgentState::RateLimit));
    assert!(!h.record_error(AgentState::RateLimit));
    assert!(h.record_error(AgentState::RateLimit)); // 3rd = loop
    assert_eq!(h.state, HealthState::ErrorLoop);
}

#[test]
fn test_hang_idle_exempt() {
    let mut h = HealthTracker::new();
    assert!(!h.check_hang(
        AgentState::Idle,
        Duration::from_secs(300),
        Duration::from_secs(0),
        1_000_000,
        0
    ));
    // Idle never hangs
}

#[test]
fn test_hang_thinking_long_timeout() {
    let mut h = HealthTracker::new();
    assert!(!h.check_hang(
        AgentState::Active,
        Duration::from_secs(100),
        Duration::from_secs(0),
        1_000_000,
        0
    )); // 100s < 600s
    assert!(h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0),
        1_000_000,
        0
    ));
    // 700s > 600s
}

#[test]
fn test_awaiting_operator_starting_silence() {
    let h = HealthTracker::new();
    // Starting + 29s silence → under threshold (slow splash/token load)
    assert!(!h.check_awaiting_operator(AgentState::Starting, Duration::from_secs(29)));
    // Starting + 31s silence → flagged
    assert!(h.check_awaiting_operator(AgentState::Starting, Duration::from_secs(31)));
}

#[test]
fn test_awaiting_operator_non_starting_exempt() {
    // #1552: Starting + runtime prompt states (PermissionPrompt /
    // InteractivePrompt) trigger; every OTHER state is exempt regardless of
    // silence — generic Idle/tool silence is handled by `check_hang` with
    // much higher thresholds so legitimate pauses don't produce FPs.
    let h = HealthTracker::new();
    for s in [
        AgentState::Idle,
        AgentState::Idle,
        AgentState::Active,
        AgentState::Hang,
        AgentState::AwaitingOperator,
        AgentState::Crashed,
    ] {
        assert!(
            !h.check_awaiting_operator(s, Duration::from_secs(60)),
            "state {:?} should not trigger awaiting_operator",
            s
        );
    }
    // #1552: the runtime prompt states DO trigger past the threshold (the
    // supervisor adds the position/stability/engagement FP-gates on top).
    for s in [AgentState::PermissionPrompt, AgentState::InteractivePrompt] {
        assert!(
            h.check_awaiting_operator(s, Duration::from_secs(60)),
            "runtime prompt state {s:?} must trigger awaiting_operator past threshold"
        );
        assert!(
            !h.check_awaiting_operator(s, Duration::from_secs(10)),
            "runtime prompt state {s:?} under threshold must not trigger"
        );
    }
}

#[test]
fn test_notification_rate_limit() {
    let mut h = HealthTracker::new();
    h.record_crash();
    let (_, _, notify1) = h.record_crash();
    assert!(notify1); // First notification

    let (_, _, notify2) = h.record_crash();
    assert!(!notify2); // Rate limited (< 5 min)
}

#[test]
fn test_respawn_ok_recovers() {
    let mut h = HealthTracker::new();
    h.record_crash();
    assert_eq!(h.state, HealthState::Recovering);
    h.respawn_ok(true);
    assert_eq!(h.state, HealthState::Healthy);
}

#[test]
fn test_clone_preserves_crash_history() {
    let mut h = HealthTracker::new();
    h.record_crash();
    h.record_crash();
    assert_eq!(h.total_crashes, 2);

    // Simulate respawn: clone old tracker, call respawn_ok
    let mut h2 = h.clone();
    h2.respawn_ok(true);
    assert_eq!(h2.total_crashes, 2); // History preserved

    // 3rd crash on cloned tracker should see recent=3
    let (_, _, notify) = h2.record_crash();
    // notify is false: 2nd crash already set last_crash_notification and cooldown (5 min) hasn't elapsed
    assert!(!notify);
    assert_eq!(h2.state, HealthState::Unstable);
}

#[test]
fn test_maybe_decay() {
    let mut h = HealthTracker::new();
    h.record_crash();
    h.record_crash();
    assert_eq!(h.total_crashes, 2);
    // Decay won't trigger immediately (need 30 min)
    h.maybe_decay(true);
    assert_eq!(h.total_crashes, 2);
}

#[test]
fn test_check_hang_skipped_when_rate_limited() {
    let mut h = HealthTracker::new();
    h.set_blocked_reason(BlockedReason::RateLimit {
        retry_after_secs: Some(60),
    });
    // Thinking + 700s silence would normally trigger hang
    assert!(!h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0),
        1_000_000,
        0
    ));
    assert_ne!(h.state, HealthState::Hung);

    // Also test QuotaExceeded and AwaitingOperator
    h.clear_blocked_reason();
    h.set_blocked_reason(BlockedReason::QuotaExceeded);
    assert!(!h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0),
        1_000_000,
        0
    ));

    h.clear_blocked_reason();
    h.set_blocked_reason(BlockedReason::AwaitingOperator);
    assert!(!h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0),
        1_000_000,
        0
    ));

    // PermissionPrompt does NOT suppress hang check
    h.clear_blocked_reason();
    h.set_blocked_reason(BlockedReason::PermissionPrompt);
    assert!(h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0),
        1_000_000,
        0
    ));
}

// ── #1553: prompt states must never reach the Hung threshold ──
#[test]
fn test_1553_prompt_states_never_hang() {
    // PermissionPrompt / InteractivePrompt / AwaitingOperator are correctly
    // blocked on a human — silence is expected, so they must NEVER exceed the
    // Hung threshold (else Stage-1 ESC cancels the live prompt / Stage-2
    // restart kills the agent mid-approval).
    let long = Duration::from_secs(600); // well past every per-state threshold
    for state in [
        AgentState::PermissionPrompt,
        AgentState::InteractivePrompt,
        AgentState::AwaitingOperator,
    ] {
        assert!(
            !productive_silence_exceeds(state, long),
            "{state:?} must never exceed the Hung threshold"
        );
    }
}

#[test]
fn test_1553_check_hang_skips_prompt_state_without_reason() {
    // The exact gap: a real PermissionPrompt agent whose #1552 AwaitingOperator
    // escalation has NOT fired (current_reason == None) was classified Hung
    // after 120s → ESC'd/restarted. The AgentState-layer gate must prevent it
    // even without any blocked reason.
    for state in [
        AgentState::PermissionPrompt,
        AgentState::InteractivePrompt,
        AgentState::AwaitingOperator,
    ] {
        let mut h = HealthTracker::new();
        assert_eq!(h.current_reason, None, "no blocked reason (the gap)");
        assert!(
            !h.check_hang(
                state,
                Duration::from_secs(300), // 5 min — past the old 120s threshold
                Duration::from_secs(300),
                1_000_000,
                0
            ),
            "{state:?} with no reason must not be classified Hung"
        );
        assert_ne!(h.state, HealthState::Hung);
    }
}

#[test]
fn test_1553_regression_other_state_thresholds_unchanged() {
    // The fix must NOT relax the existing per-state thresholds.
    assert!(!productive_silence_exceeds(
        AgentState::Idle,
        Duration::from_secs(100_000)
    ));
    assert!(productive_silence_exceeds(
        AgentState::Starting,
        Duration::from_secs(121)
    ));
    assert!(!productive_silence_exceeds(
        AgentState::Starting,
        Duration::from_secs(119)
    ));
    assert!(productive_silence_exceeds(
        AgentState::Active,
        Duration::from_secs(601)
    ));
    assert!(!productive_silence_exceeds(
        AgentState::Active,
        Duration::from_secs(599)
    ));
    // Ready/Idle merge: `Idle` is now EXEMPT from the Hung floor (it absorbed
    // the old `Ready` catch-all path → Idle's `=> false`). An idle agent is
    // legitimately quiet — never silence-Hung. A genuinely-stalling ACTIVE
    // agent still hangs via the Active 600s threshold.
    assert!(!productive_silence_exceeds(
        AgentState::Idle,
        Duration::from_secs(121)
    ));
    assert!(productive_silence_exceeds(
        AgentState::Active,
        Duration::from_secs(601)
    ));
}

#[test]
fn test_clear_blocked_reason_resumes_hang_check() {
    let mut h = HealthTracker::new();
    h.set_blocked_reason(BlockedReason::RateLimit {
        retry_after_secs: None,
    });
    assert!(!h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0),
        1_000_000,
        0
    ));

    h.clear_blocked_reason();
    assert!(h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0),
        1_000_000,
        0
    ));
    assert_eq!(h.state, HealthState::Hung);
}

#[test]
fn test_blocked_reason_serde() {
    let cases = vec![
        BlockedReason::Hang,
        BlockedReason::RateLimit {
            retry_after_secs: Some(60),
        },
        BlockedReason::RateLimit {
            retry_after_secs: None,
        },
        BlockedReason::QuotaExceeded,
        BlockedReason::AwaitingOperator,
        BlockedReason::PermissionPrompt,
        BlockedReason::Crash,
        BlockedReason::ModelUnsupported,
        BlockedReason::TypedInjectContaminated,
    ];
    for reason in cases {
        let json = serde_json::to_string(&reason).expect("serialize");
        let parsed: BlockedReason = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, reason, "round-trip failed for {json}");
    }
}

#[test]
fn test_watchdog_dry_run_logs_but_no_state_change() {
    // Simulate dry-run: classify returns Some(RateLimit), but we only log, don't set.
    let mut h = HealthTracker::new();
    let backend = crate::backend::Backend::ClaudeCode;
    // #1125 M4: updated to use canonical for_backend pattern
    let output = "Server is temporarily limiting requests";
    let reason = crate::state::classify_pty_output(&backend, output);
    assert!(reason.is_some(), "should classify as blocked");

    // Dry-run: do NOT call set_blocked_reason
    // (in production, daemon checks AGEND_WATCHDOG_DRY_RUN)
    assert!(
        h.current_reason.is_none(),
        "dry-run must not mutate health state"
    );

    // check_hang should still fire (no reason set)
    assert!(h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0),
        1_000_000,
        0
    ));
}

#[test]
fn test_watchdog_live_sets_reason() {
    // Simulate live mode: classify returns Some, set_blocked_reason called.
    let mut h = HealthTracker::new();
    let backend = crate::backend::Backend::KiroCli;
    let output = "ThrottlingError: Too Many Requests";
    if let Some(reason) = crate::state::classify_pty_output(&backend, output) {
        h.set_blocked_reason(reason);
    }
    assert!(
        matches!(h.current_reason, Some(BlockedReason::RateLimit { .. })),
        "live mode must set current_reason, got: {:?}",
        h.current_reason
    );
    // check_hang should be suppressed
    assert!(!h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0),
        1_000_000,
        0
    ));
}

#[test]
fn test_watchdog_ignores_classify_none() {
    // Healthy output → classify returns None → no change.
    let mut h = HealthTracker::new();
    let backend = crate::backend::Backend::ClaudeCode;
    let output = "Thinking about your request...";
    let reason = crate::state::classify_pty_output(&backend, output);
    assert!(reason.is_none(), "healthy output should not classify");
    assert!(h.current_reason.is_none());
    // check_hang still works normally
    assert!(h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0),
        1_000_000,
        0
    ));
}

#[test]
fn test_reset_clears_current_reason() {
    let mut h = HealthTracker::new();
    h.set_blocked_reason(BlockedReason::QuotaExceeded);
    assert!(h.current_reason.is_some());
    h.reset();
    assert!(
        h.current_reason.is_none(),
        "reset must clear current_reason"
    );
}

// Sprint 24 P1 (F-NEW-DAEMON-HEALTH-CLASSIFIER-1) — IdleLong vs Hung
// discriminator tests. Closes operator 04:00 UTC false-alarm pattern.
//
// Ready/Idle merge: these mechanism tests carry `AgentState::Active` (an
// active agent that genuinely silence-exceeds at the 600s threshold), NOT
// `Idle`. `Idle` is now EXEMPT from the Hung floor (`productive_silence_exceeds
// => false`), so it short-circuits to Healthy before reaching this
// discriminator — pinned directly by `classifier_idle_agent_exempt_from_hung`
// below. The discriminator (HealthState::IdleLong vs Hung) still governs
// every non-exempt state.

/// Ready/Idle merge (accepted behavior change ③): an `Idle` agent is EXEMPT
/// from Hung classification even with input pending past response + a fresh
/// heartbeat — the very inputs that drive a ToolUse agent to Hung. Pre-merge,
/// `Ready` (agy/opencode idle prompt) was NON-exempt (120s catch-all) and
/// could be flagged Hung here; post-merge it follows Idle's exemption,
/// consistent with claude (which always behaved this way — hang is caught via
/// Thinking/ToolUse, not the idle-prompt path).
#[test]
fn classifier_idle_agent_exempt_from_hung() {
    let mut h = HealthTracker::new();
    let result = h.check_hang(
        AgentState::Idle,
        Duration::from_secs(700), // far past any threshold
        Duration::from_secs(700),
        10_000, // input pending past response (would Hung a ToolUse agent)
        0,      // no heartbeat refresh
    );
    assert!(!result, "idle agent must never be Hung from silence");
    assert_eq!(
        h.state,
        HealthState::Healthy,
        "idle short-circuits to Healthy — never Hung/IdleLong"
    );
}

#[test]
fn classifier_returns_hung_when_input_pending_past_response() {
    // Real hung: input delivered at T+5s, agent has not responded
    // (heartbeat still at T+0). Silence exceeds threshold. Classifier
    // must return true (escalation-worthy) and set state = Hung.
    let mut h = HealthTracker::new();
    // last_input_at_ms past last_heartbeat_at_ms by > 5s grace.
    let result = h.check_hang(
        AgentState::Active,
        Duration::from_secs(700), // > 120s threshold
        Duration::from_secs(0),   // F9: productive-silence — recent
        10_000,                   // input delivered at T+10s
        0,                        // no heartbeat (or T-0)
    );
    assert!(result, "input pending past response → Hung, return true");
    assert_eq!(h.state, HealthState::Hung);
}

#[test]
fn classifier_returns_idle_long_when_no_input_pending() {
    // Operator 04:00 UTC pattern: agent silent past threshold but NO
    // input was delivered (last_input_at_ms == 0). Classifier must
    // mark IdleLong (no escalation) and return false.
    let mut h = HealthTracker::new();
    let result = h.check_hang(
        AgentState::Active,
        Duration::from_secs(700), // > 120s threshold
        Duration::from_secs(0),   // F9: productive-silence — recent
        0,                        // no input ever delivered
        5_000,                    // heartbeat at T+5s (some past activity)
    );
    assert!(!result, "no input pending → IdleLong, no escalation");
    assert_eq!(
        h.state,
        HealthState::IdleLong,
        "must be IdleLong, NOT Hung — operator 04:00 UTC false-alarm pattern"
    );
}

#[test]
fn classifier_returns_idle_long_when_input_already_responded_to() {
    // Input delivered at T+0, agent responded at T+8s (heartbeat
    // refreshed). Silence then accrues. Last_input < last_heartbeat
    // → no input pending → IdleLong (NOT Hung).
    let mut h = HealthTracker::new();
    let result = h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0), // F9: productive-silence — recent
        0,                      // input at T+0
        8_000,                  // heartbeat at T+8s (already responded)
    );
    assert!(!result, "input already responded → IdleLong");
    assert_eq!(h.state, HealthState::IdleLong);
}

#[test]
fn classifier_returns_healthy_when_silence_below_threshold() {
    // Fresh agent: silent < threshold. Classifier returns Healthy
    // regardless of input/heartbeat data.
    let mut h = HealthTracker::new();
    let result = h.check_hang(
        AgentState::Idle,
        Duration::from_secs(60), // < 120s threshold
        Duration::from_secs(0),  // F9: productive-silence — recent
        10_000,
        0,
    );
    assert!(!result);
    assert_eq!(h.state, HealthState::Healthy);
}

#[test]
fn classifier_idle_long_recovers_to_healthy_when_activity_resumes() {
    // Agent enters IdleLong at T+180s silent. Then activity resumes
    // (silent drops below threshold). State must transition back to
    // Healthy so future cron consumers don't see stale IdleLong.
    let mut h = HealthTracker::new();
    h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0),
        0,
        5_000,
    );
    assert_eq!(h.state, HealthState::IdleLong);
    // Activity resumes → silence < threshold.
    let result = h.check_hang(
        AgentState::Active,
        Duration::from_secs(30),
        Duration::from_secs(0),
        0,
        5_000,
    );
    assert!(!result);
    assert_eq!(
        h.state,
        HealthState::Healthy,
        "IdleLong must recover to Healthy when silence drops"
    );
}

#[test]
fn classifier_grace_window_prevents_flap() {
    // last_input at T+5s, last_heartbeat at T+0. Delta = 5_000ms = exactly
    // the grace window. Must NOT flag Hung — within grace.
    let mut h = HealthTracker::new();
    let result = h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0), // F9: productive-silence — recent
        5_000,                  // input
        0,                      // heartbeat — delta exactly 5_000ms
    );
    assert!(
        !result,
        "delta == grace window → not yet Hung (boundary inclusive)"
    );
    // delta = 5_001 > grace → Hung
    let result2 = h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0),
        5_001,
        0,
    );
    assert!(result2, "delta > grace → Hung");
}

#[test]
fn classifier_hung_state_returns_false_on_subsequent_calls() {
    // First Hung detection returns true (caller escalates). Second
    // call with same state must return false (already escalated;
    // avoid duplicate escalation).
    let mut h = HealthTracker::new();
    assert!(h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0),
        10_000,
        0
    ));
    assert_eq!(h.state, HealthState::Hung);
    // Same conditions next tick → no re-escalation.
    assert!(!h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0),
        10_000,
        0
    ));
    assert_eq!(h.state, HealthState::Hung);
}

// Sprint 24 P2 F1 — fresh-heartbeat-PTY-silent classifier tests.
// Catches stuck agents in tight MCP loops (heartbeat refreshing but
// PTY producing no output) that would otherwise misclassify as IdleLong.

#[test]
fn classifier_returns_hung_on_fresh_heartbeat_pty_silent() {
    // Stuck-agent scenario: agent calling MCP tools (heartbeat fresh)
    // but producing no PTY output (silent past threshold). Classifier
    // must return Hung (escalation-worthy), NOT IdleLong.
    let mut h = HealthTracker::new();
    let now = crate::daemon::heartbeat_pair::now_ms();
    // Heartbeat very recent (1s ago), no input pending, PTY silent 180s.
    let result = h.check_hang(
        AgentState::Active,
        Duration::from_secs(700), // > 120s threshold
        Duration::from_secs(0),   // F9: productive-silence — recent
        0,                        // no input pending
        now - 1_000,              // heartbeat 1s ago (fresh)
    );
    assert!(
        result,
        "heartbeat fresh + PTY silent → Hung (F1 cross-check)"
    );
    assert_eq!(h.state, HealthState::Hung);
}

#[test]
fn classifier_returns_idle_long_on_normal_idle_stale_heartbeat() {
    // Regression: pure idle with stale heartbeat (older than silence
    // window). Must still classify as IdleLong, not Hung.
    let mut h = HealthTracker::new();
    // Heartbeat 800s ago (stale — older than the 700s silence window, so
    // `heartbeat_fresh` (age < silent) is false → no F1 Hung cross-check).
    let now = crate::daemon::heartbeat_pair::now_ms();
    let result = h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0), // F9: productive-silence — recent
        0,                      // no input pending
        now - 800_000,          // heartbeat 800s ago (stale vs 700s window)
    );
    assert!(
        !result,
        "stale heartbeat + no input → IdleLong (no escalation)"
    );
    assert_eq!(h.state, HealthState::IdleLong);
}

// -----------------------------------------------------------------------
// F9 productive-output gate tests (#685 sub-task 4, decision
// d-20260513235514013631-0). Pins the dual-path contract:
//   - Default (env var unset): productive-silence path produces telemetry
//     but does NOT change Hung classification.
//   - Activated (AGEND_PRODUCTIVE_GATE=1): productive-silence-exceeded +
//     silent-NOT-exceeded triggers Hung.
//   - Existing silent path unchanged in both modes (no regression on
//     #659 silent-stuck detection).
// Tests must serialise on the env var because Rust tests share process
// env. Use a single Once-style mutex to avoid flake.
// -----------------------------------------------------------------------

/// Run `f` with `AGEND_PRODUCTIVE_GATE` env var set per `active`,
/// restoring the prior value on return.
///
/// **Mirror copy** of `tests/common/env_gate.rs::with_f9_gate`. Unit
/// tests cannot directly import from `tests/common/`; the helper is
/// duplicated to enable both unit and integration test reuse. Sub-task
/// 5 decision `d-20260514015214320625-1` §1.D accepted the ~15 LOC
/// duplication over exposing `pub mod test_util` in production code.
/// Keep in lock-step with the integration-test copy.
fn with_f9_gate<R>(active: bool, f: impl FnOnce() -> R) -> R {
    // Tests touch a shared process-wide env var — serialise via a
    // function-scoped mutex so parallel test threads don't race.
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let lock = LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let prior = std::env::var("AGEND_PRODUCTIVE_GATE").ok();
    // SAFETY: this is a test-only helper; set_var/remove_var are
    // process-global mutations and we serialise with the mutex above.
    // The unsafe block annotation matches Rust 1.84+ semantics where
    // env mutations are wrapped in unsafe. On older toolchains the
    // wrapping is a no-op syntactically.
    unsafe {
        if active {
            std::env::set_var("AGEND_PRODUCTIVE_GATE", "1");
        } else {
            std::env::remove_var("AGEND_PRODUCTIVE_GATE");
        }
    }
    let result = f();
    unsafe {
        match prior {
            Some(v) => std::env::set_var("AGEND_PRODUCTIVE_GATE", v),
            None => std::env::remove_var("AGEND_PRODUCTIVE_GATE"),
        }
    }
    result
}

#[test]
fn f9_default_shadow_does_not_classify_hung_on_productive_silence_alone() {
    // Default mode (env var unset): productive-silence above threshold
    // but silent below threshold → NO Hung classification (shadow only).
    // Pins the "additive, no regression" contract.
    with_f9_gate(false, || {
        let mut h = HealthTracker::new();
        let result = h.check_hang(
            AgentState::Active,
            Duration::from_secs(60),  // silent < 600s threshold
            Duration::from_secs(700), // silent_productive > 600s
            0,
            0,
        );
        assert!(
            !result,
            "shadow-mode must not flag Hung on productive-only path"
        );
        assert_ne!(
            h.state,
            HealthState::Hung,
            "shadow-mode must not mutate state to Hung"
        );
    });
}

#[test]
fn f9_activated_classifies_hung_on_productive_silence_exceeded() {
    // Activated mode: productive-silence above threshold + silent
    // below threshold → Hung. This is the F9 grey-failure capture:
    // 1-byte spinner output keeps silent low while no real work
    // happens (silent_productive grows).
    with_f9_gate(true, || {
        let mut h = HealthTracker::new();
        let result = h.check_hang(
            AgentState::Active,
            Duration::from_secs(60),  // silent < 600s threshold
            Duration::from_secs(700), // silent_productive > 600s
            10_000,                   // input pending past heartbeat
            0,
        );
        assert!(
            result,
            "activated F9 gate flags Hung when productive-silence exceeds"
        );
        assert_eq!(h.state, HealthState::Hung);
    });
}

#[test]
fn f9_does_not_regress_silent_path() {
    // Regression guard: when silent_productive is recent (any-output
    // path triggers Hung) the existing silent-side classification
    // path must still fire identically. F9 is strictly additive.
    with_f9_gate(true, || {
        let mut h = HealthTracker::new();
        // silent path exceeds; productive-silence is fresh.
        let result = h.check_hang(
            AgentState::Active,
            Duration::from_secs(700), // silent > 600s threshold
            Duration::from_secs(0),   // silent_productive recent
            10_000,
            0,
        );
        assert!(result, "silent path must still trigger Hung");
        assert_eq!(h.state, HealthState::Hung);
    });
}

/// #1638: the BlockedReason clear-policy + hang-suppress-policy table. This
/// is the behavior-preserving spec — every variant×signal cell must match
/// the pre-#1638 hardcoded `matches!` behavior (watchdog #1621 rate-limit
/// axis, supervisor #1552 operator-resolution axis, check_hang suppression).
/// A new variant added to the enum fails to compile in `auto_clears_on` /
/// `suppresses_hang_check` (wildcard-free), forcing a deliberate row here.
#[test]
fn blocked_reason_policy_table_1638() {
    use BlockedReason::*;
    use RecoverySignal::*;
    let rl = RateLimit {
        retry_after_secs: Some(30),
    };
    let rl_none = RateLimit {
        retry_after_secs: None,
    };

    // (reason, clears_on RateLimitLifted, clears_on OperatorResolved, suppresses_hang)
    let table: &[(BlockedReason, bool, bool, bool)] = &[
        (rl, true, false, true),
        (rl_none, true, false, true),
        (QuotaExceeded, true, false, true),
        (AwaitingOperator, false, true, true),
        (PermissionPrompt, false, false, false),
        (Hang, false, false, false),
        (Crash, false, false, false),
        // #1634: the novel never-auto-clears (false/false) + suppresses-hang
        // (true) combination — manual-clear-only like Crash, but suppresses
        // hang-check like RateLimit (stuck-but-not-hung).
        (ModelUnsupported, false, false, true),
        (TypedInjectContaminated, false, false, true),
    ];
    for (reason, on_rl, on_op, suppress) in table {
        assert_eq!(
            reason.auto_clears_on(RateLimitLifted),
            *on_rl,
            "{reason:?} auto_clears_on(RateLimitLifted)"
        );
        assert_eq!(
            reason.auto_clears_on(OperatorResolved),
            *on_op,
            "{reason:?} auto_clears_on(OperatorResolved)"
        );
        assert_eq!(
            reason.suppresses_hang_check(),
            *suppress,
            "{reason:?} suppresses_hang_check"
        );
    }

    // #1564/#1621 guard restated explicitly: an operator-action reason must
    // NOT clear on a rate-limit lift, and a throttle reason must NOT clear on
    // operator resolution.
    assert!(
        !AwaitingOperator.auto_clears_on(RateLimitLifted),
        "#1564: AwaitingOperator must survive an unrelated rate-limit lift"
    );
    assert!(
        !QuotaExceeded.auto_clears_on(OperatorResolved),
        "throttle reason must not be cleared by operator-resolution"
    );
}

#[test]
fn test_failed_with_dead_pid_does_not_decay_to_healthy() {
    let mut h = HealthTracker::new();
    h.state = HealthState::Failed;
    h.total_crashes = 4;
    let base = Instant::now();
    h.crash_times.push_back(base);
    h.crash_decay_at = Some(base);

    // Decay window elapsed, but process is dead (process_alive = false)
    h.maybe_decay_at(base + Duration::from_secs(31 * 60), false);

    // State must NOT be Healthy or Recovering; it transitions to Absent
    assert_eq!(h.state, HealthState::Absent);
}

#[test]
fn test_check_hang_suppressed_by_recent_mcp_activity() {
    let mut h = HealthTracker::new();
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;

    h.last_mcp_activity_at_epoch_ms = Some(now_ms - 5_000);
    h.state = HealthState::Hung;

    let is_hung = h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0),
        1_000_000,
        0,
    );

    assert!(
        !is_hung,
        "check_hang must return false when recent MCP activity exists"
    );
    assert_eq!(
        h.state,
        HealthState::Healthy,
        "state must transition to Healthy on MCP activity"
    );
    assert!(h.hung_since.is_none(), "hung_since must be cleared");

    h.last_mcp_activity_at_epoch_ms = Some(now_ms - 700_000);
    h.state = HealthState::Healthy;

    let is_hung = h.check_hang(
        AgentState::Active,
        Duration::from_secs(700),
        Duration::from_secs(0),
        1_000_000,
        0,
    );
    assert!(
        is_hung,
        "check_hang must return true when MCP activity is stale"
    );
    assert_eq!(h.state, HealthState::Hung, "state must transition to Hung");
}
