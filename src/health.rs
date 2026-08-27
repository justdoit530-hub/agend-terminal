//! Health monitoring: auto-respawn, backoff, hang detection, error loop.
//!
//! Two-layer state:
//! - AgentState: instant PTY output detection (Thinking, Idle, RateLimit...)
//! - HealthState: cumulative lifecycle (Healthy, Recovering, Unstable, Failed...)

use crate::state::AgentState;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const CRASH_WINDOW: Duration = Duration::from_secs(600); // 10 minutes
const NOTIFY_COOLDOWN: Duration = Duration::from_secs(300); // 5 min between same notifications
const DEFAULT_MAX_RETRIES: u32 = 5;
const BACKOFF_BASE: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
const STABILITY_WINDOW: Duration = Duration::from_secs(1800); // 30 min stable → decay
/// Silence threshold for AwaitingOperator detection. Agent in `Starting`
/// with no stdout for this long is likely blocked on an interactive startup
/// prompt (trust dialog, codex update menu before banner, auth confirmation).
///
/// Only `Starting` is considered: once the agent transitions to `Idle` we
/// trust the detection and any further silence is either legitimate idle or
/// a real hang — the latter is handled by `check_hang` with much higher
/// thresholds (120s+). Flagging Idle-with-short-silence produced false
/// positives for agents in the middle of tool execution that simply had a
/// few seconds of quiet between bursts of output.
///
/// Threshold chosen to be a true last-resort fallback: structurally
/// recognizable prompts (y/n, press enter, etc. — see
/// `state::is_generic_startup_prompt`, plus the backend-specific
/// `InteractivePrompt` patterns) fire immediately on detection, so the
/// silence window only matters for prompts whose text we can't pattern
/// match. 30s is long enough that CLIs with slow splash screens or token
/// loading don't falsely trip, and still well under the 120s
/// `check_hang` threshold.
const AWAITING_OP_SILENCE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // ErrorLoop constructed by record_error() in health monitoring
pub enum HealthState {
    Healthy,
    Recovering,
    Unstable,
    Failed,
    Hung,
    ErrorLoop,
    /// Sprint 24 P1 (F-NEW-DAEMON-HEALTH-CLASSIFIER-1): agent is silent
    /// past the hang threshold but **no input is pending past last
    /// response** — typically `Idle` state waiting for next dispatch.
    /// Cron escalation chains (`interrupt` / `replace`)
    /// MUST NOT trigger on this state; only `Hung` is escalation-worthy.
    /// Closes the operator 04:00 UTC false-alarm pattern where impl-1's
    /// 30-min idle-waiting was mis-classified as `Hung`.
    IdleLong,
    /// `#685` sub-task 7a Stage 1 recovery (decision
    /// `d-20260514030404021793-1`): agent escalated through Stage 3 of
    /// the auto-recovery dispatcher — Stage 1 ESC failed, Stage 2
    /// auto-restart failed N times. Operator action required to
    /// unpause (separate sub-task). Distinct from `Failed` (which =
    /// crash counter exhausted): same "operator must intervene"
    /// terminal status but different trigger.
    ///
    /// Guards: `check_hang` short-circuits on `Paused` (returns
    /// `false` — no auto-recovery dispatcher work); `maybe_decay` does
    /// NOT touch `Paused`; entered ONLY via Stage 3 dispatcher.
    /// See `docs/RECOVERY-STAGES.md` §RS for the lifecycle.
    Paused,
    Absent,
}

impl HealthState {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Recovering => "recovering",
            Self::Unstable => "unstable",
            Self::Failed => "failed",
            Self::Hung => "hung",
            Self::ErrorLoop => "error_loop",
            Self::IdleLong => "idle_long",
            Self::Paused => "paused",
            Self::Absent => "absent",
        }
    }
}

/// `#685` sub-task 7a: per-agent recovery dispatcher state machine for
/// the auto-recovery ladder. Carried inside `HealthTracker` so the
/// dispatcher can read both `HealthState` and stage progression in one
/// per-tick lock acquisition. Compile-time exhaustive match in the
/// dispatcher catches missing-state bugs.
///
/// **Spontaneous recovery reset** (decision §5 Refinement): when
/// `health.state` transitions back to `Healthy` (either Stage success
/// or external recovery), the dispatcher resets `recovery_stage_state`
/// to `None`. Subsequent `Hung` re-entry begins a fresh sequence —
/// linear escalation rule restarts from Stage 1.
///
/// **Future variant**: a 7th `Disabling { until_operator_unpauses }`
/// variant is intentionally NOT added in Phase 1 — Stage 3 + Paused
/// HealthState already covers the operator-action-required terminal
/// case. Recorded here as a comment-only note for future sub-tasks
/// that add operator-unpause command surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Stage2Pending / Stage3Eligible / Stage3Pending: emitted by sub-tasks 7b / 7c
pub enum RecoveryStageState {
    /// No recovery in progress. Default.
    None,
    /// Stage 1 ESC dispatched (or shadow-logged); monitoring for recovery.
    /// `entered_at` drives the Stage 1 timeout window.
    Stage1Pending { entered_at: Instant },
    /// Stage 1 timed out (or skipped via dead-likely branch). Dispatcher
    /// will fire Stage 2 on the next tick once 7b ships. Phase 1 logs
    /// the eligibility transition and stops here.
    Stage2Eligible,
    /// Stage 2 restart in progress; monitoring for recovery. (Phase 1
    /// stub — emitted by 7b when Stage 2 ships.)
    Stage2Pending { entered_at: Instant },
    /// Stage 2 timed out. Dispatcher will fire Stage 3 on next tick.
    Stage3Eligible,
    /// Stage 3 dispatched (HealthState transitioned to Paused).
    /// Awaiting operator unpause action. `entered_at` is the
    /// `Instant` passed to [`HealthTracker::enter_paused`] and is the
    /// same value stamped into [`HealthTracker::last_stage3_fired_at`]
    /// — kept on the variant so dispatcher tick-time debug logs can
    /// report Paused-since duration without reaching back into
    /// `HealthTracker` (parallel `Stage1Pending` / `Stage2Pending`).
    Stage3Pending { entered_at: Instant },
}

/// Stage 1 default timeout — ESC dispatched, dispatcher waits this long
/// before declaring failure and transitioning to `Stage2Eligible`.
/// Decision §1.4 Delta 1: 10s default (reviewer recommendation: ESC
/// delivery latency = PTY write + agent process scheduling + Ink TUI
/// state reset; under load, 5s false-positives Stage 2 escalation).
/// Fixed const (#env-cleanup: was env-overridable; demoted to YAGNI).
pub const STAGE1_TIMEOUT_DEFAULT_MS: u64 = 10_000;

/// Stage 1 default cooldown — if agent re-enters Hung within this window
/// after a recent Stage 1 fire, dispatcher skips Stage 1 (goes directly
/// to `Stage2Eligible`). Prevents rapid-fire ESC sending that masks
/// underlying issues. Decision §1.4 Refinement B.
/// Fixed const (#env-cleanup: was env-overridable; demoted to YAGNI).
pub const STAGE1_COOLDOWN_DEFAULT_MS: u64 = 60_000;

/// Stage 2 default backoff — sleep before `spawn_agent` re-runs in the
/// respawn worker's Stage 2 arm. Decision §1.4 Delta 2: 1s default
/// (defensive padding against tight-loop on transient spawn errors —
/// transient filesystem / network / PTY allocation failures).
/// Fixed const (#env-cleanup: was env-overridable; demoted to YAGNI).
pub const STAGE2_BACKOFF_DEFAULT_MS: u64 = 1_000;

/// Stage 2 default monitoring window — how long the dispatcher waits in
/// `Stage2Pending` for the agent to settle on `Healthy` before
/// classifying Stage 2 as failed and escalating to `Stage3Eligible`.
/// Mirrors the 30s window already documented in `docs/RECOVERY-STAGES.md`.
/// Fixed const (#env-cleanup: was env-overridable; demoted to YAGNI).
pub const STAGE2_TIMEOUT_DEFAULT_MS: u64 = 30_000;

/// Stage 2 cumulative retry cap — when `recovery_restart_count` reaches
/// this number across Hung cycles, the dispatcher skips Stages 1/2 on
/// the next Hung and escalates directly to `Stage3Eligible`. Decision
/// §Q1/Q2: N=3 default mirrors the issue body's "fails N times → Stage 3"
/// language with a conservative restart budget before operator intervention.
/// Operator override via env var `AGEND_AUTO_RECOVERY_STAGE2_MAX_RESTARTS`.
pub const STAGE2_MAX_RESTARTS_DEFAULT: u32 = 3;

/// Returns `true` when `silent_productive` (silence since last productive
/// output) exceeds the per-`AgentState` threshold. Mirrors the
/// `silence_exceeds_threshold` pattern at the top of `check_hang`.
/// Extracted per decision §1.4 Delta 2 (Option a: DRY, single source of
/// truth — recovery dispatcher reads this directly without
/// re-implementing the threshold mapping).
pub fn productive_silence_exceeds(agent_state: AgentState, silent_productive: Duration) -> bool {
    match agent_state {
        AgentState::Idle => false,
        // #1553: an agent parked on a permission/approval/interactive prompt is
        // CORRECTLY blocked on a human, not hung — silence is expected and
        // open-ended. It must never reach the Hung threshold, otherwise the
        // recovery pipeline acts on it: Stage-1 ESC cancels the live prompt and
        // Stage-2 restart kills the agent mid-approval. Gating here (the single
        // shared Hung-threshold fn, read by `check_hang`) covers every downstream
        // recovery path uniformly — Stage 1/2/3 + the cron interrupt/replace
        // consumers — not just the Stage-1 ESC the issue named.
        AgentState::PermissionPrompt
        | AgentState::InteractivePrompt
        | AgentState::AwaitingOperator => false,
        AgentState::Starting => silent_productive > Duration::from_secs(120),
        AgentState::Active => silent_productive > Duration::from_secs(600),
        _ => silent_productive > Duration::from_secs(120),
    }
}

/// Why an agent is blocked. Used to prevent `check_hang` from
/// misdiagnosing expected waits as hangs (race mutex).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum BlockedReason {
    Hang,
    RateLimit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_secs: Option<u64>,
    },
    QuotaExceeded,
    AwaitingOperator,
    PermissionPrompt,
    Crash,
    /// #1634: the backend's configured model is rejected by the provider
    /// (discontinued/unsupported model id). Set from the red-anchored
    /// `AgentState::ModelUnsupported`. Manual-clear-only and hang-suppressing —
    /// see `auto_clears_on` / `suppresses_hang_check`.
    ModelUnsupported,
    ContextHigh,
    /// #3175: a LegacyPty typed injection left a draft whose delivery or submit
    /// could not be confirmed. The per-process injection fence remains closed
    /// until the agent handle is replaced; surface that fail-closed state instead
    /// of silently dropping every later nudge behind the same fence.
    TypedInjectContaminated,
}

/// #1638: which recovery signal a [`BlockedReason`] is being tested against.
/// There are two independent recovery axes, each clearing a different set of
/// reasons (a single "recovered?" boolean would conflate them and break one):
/// - [`RateLimitLifted`](Self::RateLimitLifted): the watchdog observed the agent
///   actively working/ready again after a throttle (#1621/bughunt2 auto-clear).
/// - [`OperatorResolved`](Self::OperatorResolved): the supervisor consumed a
///   recovery notice on the transition out of AwaitingOperator/InteractivePrompt
///   (#1552 "ready again" clear).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverySignal {
    RateLimitLifted,
    OperatorResolved,
}

impl BlockedReason {
    pub fn parse_kind(kind: &str, params: &serde_json::Value) -> Option<Self> {
        match kind {
            "rate_limit" => Some(Self::RateLimit {
                retry_after_secs: params["retry_after_secs"].as_u64(),
            }),
            "quota_exceeded" => Some(Self::QuotaExceeded),
            "awaiting_operator" => Some(Self::AwaitingOperator),
            "permission_prompt" => Some(Self::PermissionPrompt),
            "hang" => Some(Self::Hang),
            "crash" => Some(Self::Crash),
            "model_unsupported" => Some(Self::ModelUnsupported),
            "context_high" => Some(Self::ContextHigh),
            "typed_inject_contaminated" => Some(Self::TypedInjectContaminated),
            _ => None,
        }
    }

    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Hang => "hang",
            Self::RateLimit { .. } => "rate_limit",
            Self::QuotaExceeded => "quota_exceeded",
            Self::AwaitingOperator => "awaiting_operator",
            Self::PermissionPrompt => "permission_prompt",
            Self::Crash => "crash",
            Self::ModelUnsupported => "model_unsupported",
            Self::ContextHigh => "context_high",
            Self::TypedInjectContaminated => "typed_inject_contaminated",
        }
    }

    /// #1638: does this reason auto-clear when the given [`RecoverySignal`]
    /// fires? Type-encodes the clear-policy that was previously two hardcoded
    /// `matches!` (watchdog #1621 + supervisor #1552), so a NEW variant is
    /// compile-forced to declare its clear behavior for BOTH axes instead of
    /// silently latching forever (the #1621 set-without-clear class).
    ///
    /// Preserves the existing guards exactly:
    /// - RateLimit/QuotaExceeded auto-clear on `RateLimitLifted` only (#1621),
    ///   never on operator-resolution.
    /// - AwaitingOperator auto-clears on `OperatorResolved` only (#1552), never
    ///   on rate-limit recovery (#1564 — operator-action reasons must persist
    ///   through an unrelated throttle lift).
    /// - PermissionPrompt/Hang/Crash never auto-clear (operator/crash action
    ///   required; today they are MCP-set + MCP/explicit-cleared only). See the
    ///   PR note on whether InteractivePrompt resolution SHOULD clear
    ///   PermissionPrompt — left as current behavior here.
    ///
    /// The `match self` is intentionally wildcard-free: that is the forcing
    /// function.
    pub fn auto_clears_on(&self, signal: RecoverySignal) -> bool {
        match self {
            Self::RateLimit { .. } | Self::QuotaExceeded => {
                matches!(signal, RecoverySignal::RateLimitLifted)
            }
            Self::AwaitingOperator => matches!(signal, RecoverySignal::OperatorResolved),
            // #1634: ModelUnsupported never auto-clears on EITHER axis. A rate-
            // limit lift doesn't fix a bad model id, and there is no "ready
            // again" operator-resolution transition (the agent keeps erroring) —
            // the operator must change the configured model, then the agent
            // restarts and `HealthTracker::reset()` clears it on respawn. So it
            // joins the manual-clear-only class with PermissionPrompt/Hang/Crash.
            Self::PermissionPrompt
            | Self::Hang
            | Self::Crash
            | Self::ModelUnsupported
            | Self::ContextHigh
            | Self::TypedInjectContaminated => false,
        }
    }

    /// #1638: does this reason legitimately suppress `check_hang` (the agent is
    /// blocked for a known reason that explains the output silence)? Type-encodes
    /// what was a hardcoded `matches!` in `check_hang`, so a new variant is
    /// compile-forced to declare its hang-suppression too. Wildcard-free by
    /// design. Behavior-preserving: RateLimit/QuotaExceeded/AwaitingOperator
    /// suppress; PermissionPrompt/Hang/Crash do not.
    pub fn suppresses_hang_check(&self) -> bool {
        match self {
            // #1634: ModelUnsupported suppresses hang-check (stuck-but-not-hung —
            // the model rejection explains the output silence; we don't want
            // hang-detection ALSO firing/escalating on top of the model-unsupported
            // notify). NOTE the deliberate novel combination: it NEVER auto-clears
            // (above, like Crash) yet DOES suppress hang (here, like RateLimit) —
            // no prior variant pairs those, which is exactly why #1638's
            // wildcard-free axes force this to be declared explicitly.
            Self::RateLimit { .. }
            | Self::QuotaExceeded
            | Self::AwaitingOperator
            | Self::ModelUnsupported
            | Self::TypedInjectContaminated => true,
            Self::PermissionPrompt | Self::Hang | Self::Crash | Self::ContextHigh => false,
        }
    }
}

impl std::fmt::Display for BlockedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.kind_str())
    }
}

/// Tracks health for one agent.
#[derive(Clone)]
#[allow(dead_code)] // error_events, last_output, record_error, reset: reserved for daemon health monitoring
pub struct HealthTracker {
    pub state: HealthState,
    pub crash_times: VecDeque<Instant>,
    pub total_crashes: u32,
    /// [M1] decay clock for `total_crashes`. Advanced to `now` on every
    /// `maybe_decay` decrement so decay is genuinely 1 unit per `STABILITY_WINDOW`
    /// — NOT keyed on `crash_times.back()` alone (which never moves on decrement,
    /// so once one window elapsed the whole budget drained in consecutive ticks).
    /// In-memory only (re-derives from `crash_times` on restart; not persisted).
    crash_decay_at: Option<Instant>,
    max_retries: u32,
    /// #1744-H3: per-class notify cooldown — crash escalations stamp/read THIS,
    /// hung escalations use `last_hung_notification`. Split from the former shared
    /// `last_notification` so a crash page and a hung page no longer suppress each
    /// other inside `NOTIFY_COOLDOWN` (they are distinct failure modes).
    pub last_crash_notification: Option<Instant>,
    /// #1744-H3: per-class notify cooldown for the self-orch Hung escalation.
    /// See `last_crash_notification`.
    pub last_hung_notification: Option<Instant>,
    /// #1701: when the agent most recently transitioned INTO `HealthState::Hung`
    /// (set in `check_hang`'s two Hung-entry branches). Drives the self-orch Hung
    /// escalation confirm-window.
    ///
    /// #1744-H2: set via `get_or_insert` (not unconditional `= now`) so a value
    /// rehydrated across a daemon restart SURVIVES the first post-restart Hung
    /// re-entry — otherwise the confirm-window would reset to 0 and the P0 be
    /// delayed/missed for a still-hung resumed session. Paired with an explicit
    /// clear on Hung EXIT (so a recovered episode's anchor never lingers / is
    /// never persisted stale).
    hung_since: Option<Instant>,
    /// #1744-H4: once-off latch — set when the terminal-Failed self-orchestrator
    /// P0 fires, so the permanent leaderless-death page is sent exactly once
    /// (persisted across restart via [`PersistedEscalation`]).
    pub failed_escalated: bool,
    error_events: VecDeque<(Instant, AgentState)>,
    pub last_output: Instant,
    pub current_reason: Option<BlockedReason>,
    /// #1933: free-text operator-readable annotation accompanying `current_reason`
    /// (the `note` arg of `health action=report`). Orthogonal to the reason KIND,
    /// so it lives alongside `current_reason` rather than inside the enum. Reset
    /// when a new reason is set and cleared with the reason.
    pub current_note: Option<String>,
    /// #2232: ground-truth rate-limit recovery latch. Set true ONLY when the
    /// AGENT itself clears a `RateLimit`/`QuotaExceeded` block via the MCP
    /// `clear_blocked_reason` action (proof it is awake and read the inject) —
    /// the watchdog's heuristic auto-clear does NOT set it. The supervisor's
    /// `process_error_recovery` reads it (under the lock it already holds) as a
    /// reliable recovery signal to drop the `ServerRateLimit` retry track when
    /// the narrow `recovered_within` heuristic misses a pure-text fast reply, and
    /// resets it on a genuine `ServerRateLimit` exit so a future episode re-arms.
    /// In-memory only (resets to false on restart, like the retry tracks).
    /// (`RecoverySignal::RateLimitLifted` is the would-be typed equivalent but
    /// stays unwired — a bool is sufficient here, #2232 D2.)
    pub rate_limit_self_cleared: bool,
    /// Last MCP activity timestamp as epoch milliseconds.
    pub last_mcp_activity_at_epoch_ms: Option<u64>,
    /// `#685` sub-task 7a: per-agent recovery dispatcher state machine.
    /// Mutated only by `src/daemon/per_tick/recovery_dispatcher.rs`;
    /// `health.rs` ships it as a field to keep all per-agent state in
    /// one struct (single lock acquisition for dispatcher tick).
    pub recovery_stage_state: RecoveryStageState,
    /// `#685` sub-task 7a: timestamp of last Stage 1 ESC fire (or
    /// shadow-log emission). Used by the cooldown guard — if agent
    /// re-enters `Hung` within `STAGE1_COOLDOWN_DEFAULT_MS` of this
    /// timestamp, dispatcher skips Stage 1 and escalates directly to
    /// `Stage2Eligible`. `None` means Stage 1 has never fired for this
    /// agent in the current daemon run.
    pub last_stage1_fired_at: Option<Instant>,
    /// `#685` sub-task 7b: cumulative Stage 2 auto-restart count across
    /// Hung cycles. Increments on each Stage 2 fire that the respawn
    /// worker successfully consumes (channel `try_send` `Ok`). When
    /// count reaches `STAGE2_MAX_RESTARTS_DEFAULT`, the dispatcher
    /// skips Stages 1/2 on the next Hung cycle and escalates directly
    /// to `Stage3Eligible` — operator must intervene rather than the
    /// daemon repeatedly thrashing the agent.
    ///
    /// Decays via `maybe_decay`: 1 unit per `STABILITY_WINDOW` (30 min)
    /// of last-crash stability. Mirrors `total_crashes` decay
    /// discipline so an agent that recovers and stays stable does not
    /// carry recovery-restart attribution forever.
    ///
    /// Preserved selectively across the Stage 2 respawn boundary in
    /// `daemon/mod.rs` Stage 2 variant arm — fresh `HealthTracker` on
    /// spawn re-applies this counter (plus crash_times + total_crashes,
    /// plus the per-class notify cooldowns) so the cap survives the restart
    /// that the counter itself drove.
    pub recovery_restart_count: u32,
    /// `#685` sub-task 7b: timestamp of last Stage 2 auto-restart fire.
    /// Used to drive `recovery_restart_count` decay alongside
    /// `crash_times` — `maybe_decay` checks the most recent of the two
    /// when deciding whether the agent has been stable long enough to
    /// decrement the recovery counter. `None` means Stage 2 has never
    /// fired for this agent in the current daemon run.
    pub last_stage2_fired_at: Option<Instant>,
    /// `#685` sub-task 7c: timestamp of last Stage 3 escalation (the
    /// transition into `HealthState::Paused`). Reserved for the future
    /// operator-unpause sub-task — it will read this to display "Paused
    /// since {duration}" in the unpause UI and may extend
    /// [`Self::maybe_decay_at`] to honour a Paused decay window. Stage 3
    /// is terminal in Phase 2, so nothing in 7c reads the field; carry
    /// `#[allow(dead_code)]` until the unpause sub-task lands.
    #[allow(dead_code)] // reserved for unpause sub-task (Stage 3 decay)
    pub(crate) last_stage3_fired_at: Option<Instant>,
    /// F9 productive-silence gate active state (backend-specific)
    pub productive_gate: bool,
}

/// #1744-H3: shared rate-limit predicate for a per-class notify cooldown stamp.
/// `None` (never notified) → due; otherwise due once `NOTIFY_COOLDOWN` elapsed.
fn cooldown_elapsed(last: Option<Instant>) -> bool {
    match last {
        // #1744-H2: saturating — a coarse-clock (windows) read where the stamp
        // sits in the same tick as `now` (t >= now) underflows a raw `elapsed()`.
        // b>=a → 0 elapsed, which is the correct "just stamped, not yet due".
        Some(t) => Instant::now().saturating_duration_since(t) >= NOTIFY_COOLDOWN,
        None => true,
    }
}

/// #1744-H2: cap on how far back a rehydrated wall-clock timestamp may be
/// projected onto the monotonic clock. Well past every health window
/// (`CRASH_WINDOW` 600s, `NOTIFY_COOLDOWN` 300s), so a capped value still reads
/// as "long ago" (cooldown elapsed / crash pruned / hang sustained) — the cap
/// only guards `Instant::now() - gap` against underflow on an absurd/corrupt
/// stored value.
const REHYDRATE_MAX_LOOKBACK_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// #1744-H2: persist an `Instant` as a wall-clock epoch-ms (monotonic `Instant`
/// is meaningless across a process restart). `now_epoch - instant.elapsed()`.
fn instant_to_epoch_ms(t: Instant) -> u64 {
    // #1744-H2: saturating elapsed — a coarse-clock (windows) `t` that reads
    // in the same tick as `now` (t >= now) underflows a raw `t.elapsed()`.
    let elapsed_ms = Instant::now().saturating_duration_since(t).as_millis() as u64;
    crate::daemon::heartbeat_pair::now_ms().saturating_sub(elapsed_ms)
}

/// #1744-H2: project a persisted wall-clock epoch-ms back onto the monotonic
/// clock. A future timestamp (clock skew) clamps to now; an absurdly old one
/// clamps to [`REHYDRATE_MAX_LOOKBACK_MS`] (still "long ago" for every window).
fn epoch_ms_to_instant(ms: u64) -> Instant {
    let gap_ms = crate::daemon::heartbeat_pair::now_ms()
        .saturating_sub(ms)
        .min(REHYDRATE_MAX_LOOKBACK_MS);
    Instant::now()
        .checked_sub(Duration::from_millis(gap_ms))
        .unwrap_or_else(Instant::now)
}

/// #1744-H2: the subset of [`HealthTracker`] escalation state that must survive a
/// daemon restart, stored as wall-clock epoch-ms (NOT `Instant`). Persisted via
/// `daemon::escalation_persist` and re-applied on boot/agent-register so a
/// restart no longer (a) resets the crash budget → infinite respawn that never
/// reaches `Failed`, (b) resets the Hung confirm-window → delayed/missed self-orch
/// P0, or (c) clears the notify cooldowns → duplicate P0 for an already-paged agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedEscalation {
    #[serde(default)]
    pub total_crashes: u32,
    #[serde(default)]
    pub crash_times_epoch_ms: Vec<u64>,
    #[serde(default)]
    pub last_crash_notification_epoch_ms: Option<u64>,
    #[serde(default)]
    pub last_hung_notification_epoch_ms: Option<u64>,
    #[serde(default)]
    pub hung_since_epoch_ms: Option<u64>,
    /// #1744-H4: once-off latch for the terminal-Failed self-orchestrator P0.
    /// Persisted so a daemon restart (which does NOT rehydrate `state`, only the
    /// crash budget) doesn't re-page the operator about the same permanent death.
    #[serde(default)]
    pub failed_escalated: bool,
}

impl HealthTracker {
    pub fn new() -> Self {
        Self {
            state: HealthState::Healthy,
            crash_times: VecDeque::new(),
            total_crashes: 0,
            crash_decay_at: None,
            max_retries: DEFAULT_MAX_RETRIES,
            last_crash_notification: None,
            last_hung_notification: None,
            hung_since: None,
            failed_escalated: false,
            error_events: VecDeque::new(),
            last_output: Instant::now(),
            current_reason: None,
            current_note: None,
            rate_limit_self_cleared: false,
            last_mcp_activity_at_epoch_ms: None,
            recovery_stage_state: RecoveryStageState::None,
            last_stage1_fired_at: None,
            recovery_restart_count: 0,
            last_stage2_fired_at: None,
            last_stage3_fired_at: None,
            productive_gate: false,
        }
    }

    /// `#685` sub-task 7c: atomic transition into `HealthState::Paused`
    /// for Stage 3 escalation. Encapsulates the three invariants that
    /// the dispatcher's Stage 3 arm must apply together so the §F39.5
    /// rule "Paused entered ONLY via Stage 3 dispatcher" has a single
    /// grep target — `enter_paused` is the sole writer of
    /// `HealthState::Paused` in the codebase.
    ///
    /// Invariants written in one logical step (caller holds the per-
    /// agent lock for the duration):
    /// 1. `state` → `HealthState::Paused` (terminal — no further
    ///    auto-recovery; check_hang short-circuits and maybe_decay
    ///    no-touches Paused per 7a guards).
    /// 2. `recovery_stage_state` → `Stage3Pending { entered_at: now }`
    ///    so the dispatcher's next tick lands on the idempotent no-op
    ///    arm rather than re-firing Stage 3.
    /// 3. `last_stage3_fired_at` → `Some(now)` for the future operator-
    ///    unpause sub-task's UX (Paused-since-{duration}).
    ///
    /// **`recovery_restart_count` is NOT reset** — preserves the
    /// operator-must-fix-root-cause signal across a future manual
    /// unpause. If the post-unpause agent Hungs again without the root
    /// cause being addressed, the cap check in
    /// [`crate::daemon::per_tick::recovery_dispatcher`] immediately
    /// re-escalates to Stage3Eligible rather than burning further
    /// auto-restart budget.
    ///
    /// DI-friendly signature (parallels [`Self::maybe_decay_at`]) so
    /// tests can supply a deterministic `now`. Production callers
    /// always pass `Instant::now()`.
    pub fn enter_paused(&mut self, now: Instant) {
        self.state = HealthState::Paused;
        self.recovery_stage_state = RecoveryStageState::Stage3Pending { entered_at: now };
        self.last_stage3_fired_at = Some(now);
    }

    /// Record a crash event. Returns (should_respawn, respawn_delay, should_notify).
    pub fn record_crash(&mut self) -> (bool, Duration, bool) {
        let now = Instant::now();
        self.crash_times.push_back(now);
        self.total_crashes += 1;

        // Clean old crashes outside window
        while let Some(front) = self.crash_times.front() {
            // #1744-H2: saturating — coarse-clock same-tick (front >= now) → 0.
            if now.saturating_duration_since(*front) > CRASH_WINDOW {
                self.crash_times.pop_front();
            } else {
                break;
            }
        }

        let recent = self.crash_times.len();
        let delay = self.backoff_delay();

        // Check max retries
        if self.total_crashes >= self.max_retries {
            self.state = HealthState::Failed;
            return (false, Duration::ZERO, true); // Don't respawn, do notify
        }

        let should_notify = recent >= 2 && cooldown_elapsed(self.last_crash_notification);

        if recent >= 3 {
            self.state = HealthState::Unstable;
        } else if recent >= 1 {
            self.state = HealthState::Recovering;
        }

        if should_notify {
            self.last_crash_notification = Some(now);
        }

        (true, delay, should_notify)
    }

    /// Mark successful respawn.
    pub fn respawn_ok(&mut self, has_live_handle: bool) {
        if has_live_handle
            && (self.state == HealthState::Recovering || self.state == HealthState::Absent)
        {
            self.state = HealthState::Healthy;
        }
        // Unstable stays until crash window clears
    }

    /// Mark a failed respawn attempt — the agent could not be restarted.
    pub fn respawn_failed(&mut self) {
        self.state = HealthState::Failed;
    }

    /// Calculate exponential backoff delay.
    fn backoff_delay(&self) -> Duration {
        if self.total_crashes == 0 {
            return BACKOFF_BASE;
        }
        let exp = (self.total_crashes - 1).min(10);
        let delay = BACKOFF_BASE.mul_f64(2.0_f64.powi(exp as i32));
        delay.min(BACKOFF_MAX)
    }

    /// #1701: self-orchestrator crash P0 gate. Unlike [`record_crash`]'s
    /// recent>=2 gate (which only fires on the SECOND crash in the window),
    /// this fires on EVERY orchestrator crash that clears `NOTIFY_COOLDOWN` —
    /// one crash already lost the orchestrator's in-memory context and no peer
    /// can relay an inbox P0, so the operator must be told. Uses the crash-class
    /// cooldown (#1744-H3: distinct from the hung cooldown) to avoid crash-loop
    /// spam, and stamps it on fire. Caller must already have established the agent
    /// is its own team orchestrator.
    pub fn self_orch_crash_should_notify(&mut self) -> bool {
        if cooldown_elapsed(self.last_crash_notification) {
            self.last_crash_notification = Some(Instant::now());
            true
        } else {
            false
        }
    }

    /// #1701 (Hung half): should a self-orchestrator's sustained Hung escalate to
    /// the operator? True only when the agent is CURRENTLY Hung AND has been so
    /// for at least `confirm_window` (the FP-filter — the Hung/IdleLong split in
    /// `check_hang` already excludes the 04:00 idle false-alarm; this window
    /// additionally rides out the documented transient residual FPs F39/F10/
    /// keystroke-draining so a real page is not fired on a 1-tick blip) AND the
    /// hung-class notify cooldown (#1744-H3: distinct from the crash cooldown, so
    /// a recent crash page no longer suppresses this) has elapsed. Stamps the
    /// cooldown on fire so a persisting Hung pages at most once per
    /// `NOTIFY_COOLDOWN`. Caller must already have established the agent is its
    /// own team orchestrator. Independent of `hang_auto_recovery_enabled` — a
    /// leaderless team with no peer to relay must page even in recovery shadow.
    pub fn hung_escalation_due(&mut self, confirm_window: Duration) -> bool {
        if self.state != HealthState::Hung {
            return false;
        }
        let sustained = self
            .hung_since
            // #1744-H2: saturating — coarse-clock same-tick (t >= now) → 0.
            .is_some_and(|t| Instant::now().saturating_duration_since(t) >= confirm_window);
        if sustained && cooldown_elapsed(self.last_hung_notification) {
            self.last_hung_notification = Some(Instant::now());
            true
        } else {
            false
        }
    }

    /// #1744-H2: snapshot the restart-critical escalation state as wall-clock
    /// epoch-ms for persistence. Pure read; the daemon calls this at the crash /
    /// hung chokepoints and hands the result to `escalation_persist`.
    pub fn escalation_snapshot(&self) -> PersistedEscalation {
        PersistedEscalation {
            total_crashes: self.total_crashes,
            crash_times_epoch_ms: self
                .crash_times
                .iter()
                .map(|t| instant_to_epoch_ms(*t))
                .collect(),
            last_crash_notification_epoch_ms: self.last_crash_notification.map(instant_to_epoch_ms),
            last_hung_notification_epoch_ms: self.last_hung_notification.map(instant_to_epoch_ms),
            hung_since_epoch_ms: self.hung_since.map(instant_to_epoch_ms),
            failed_escalated: self.failed_escalated,
        }
    }

    /// #1744-H2: re-apply persisted escalation state onto a freshly-spawned
    /// tracker (boot / agent-register). Crash times older than `CRASH_WINDOW` are
    /// dropped on the way in (they would be pruned on the next `record_crash`
    /// anyway), so the rehydrated `crash_times` reflects the live window. The
    /// cumulative `total_crashes` (max-retries budget) is restored verbatim, and
    /// the cooldowns / `hung_since` are projected back onto the monotonic clock —
    /// a stale value reads as "elapsed" via [`cooldown_elapsed`] / self-heals via
    /// the Hung-exit clear, so no `state` is rehydrated (avoids a premature Hung
    /// that would wrongly trip the recovery dispatcher on boot).
    pub fn rehydrate_escalation(&mut self, p: &PersistedEscalation) {
        self.total_crashes = p.total_crashes;
        let now = crate::daemon::heartbeat_pair::now_ms();
        self.crash_times = p
            .crash_times_epoch_ms
            .iter()
            .filter(|ms| now.saturating_sub(**ms) <= CRASH_WINDOW.as_millis() as u64)
            .map(|ms| epoch_ms_to_instant(*ms))
            .collect();
        self.last_crash_notification = p.last_crash_notification_epoch_ms.map(epoch_ms_to_instant);
        self.last_hung_notification = p.last_hung_notification_epoch_ms.map(epoch_ms_to_instant);
        self.hung_since = p.hung_since_epoch_ms.map(epoch_ms_to_instant);
        self.failed_escalated = p.failed_escalated;
    }

    /// Check whether agent is stalled on an interactive startup prompt.
    /// Pure predicate — no state mutation.
    ///
    /// Fires when the agent has been silent past the threshold AND is in a
    /// state that legitimately awaits operator input: `Starting` (the original
    /// startup-stall fallback) or, post-#1552, a runtime `PermissionPrompt` /
    /// `InteractivePrompt` (a mid-task stall on an approval the operator must
    /// answer). Other states (`Idle` mid-tool-use, etc.) are left to
    /// `check_hang` — flagging short generic silences here produced FPs.
    ///
    /// This is the *necessary* condition only; the supervisor adds escalation
    /// FP-gates (live-bottom-N position, state-stability, operator-engagement
    /// cooldown) before it actually notifies — see `daemon::supervisor`.
    pub fn check_awaiting_operator(&self, agent_state: AgentState, silent: Duration) -> bool {
        silent > AWAITING_OP_SILENCE
            && matches!(
                agent_state,
                AgentState::Starting | AgentState::PermissionPrompt | AgentState::InteractivePrompt
            )
    }

    /// Check for hang based on agent state and output timeout.
    ///
    /// Takes `silent` as a plain `Duration` (rather than `Instant::elapsed()`
    /// internally) so tests can construct arbitrary durations without
    /// overflowing on platforms where `Instant` is boot-anchored (Windows).
    ///
    /// **Sprint 24 P1 (F-NEW-DAEMON-HEALTH-CLASSIFIER-1)** added the
    /// `last_input_at_ms` + `last_heartbeat_at_ms` parameters from the
    /// per-instance [`crate::daemon::heartbeat_pair::HeartbeatPair`]
    /// snapshot to discriminate "idle waiting (no input pending)" from
    /// "hung unresponsive (input pending past last response)". Pass `0`
    /// for both in test contexts where the caller only wants legacy
    /// silence-based hang detection (back-compat with existing tests).
    ///
    /// Returns `true` ONLY when transitioning **into** [`HealthState::Hung`]
    /// (the escalation-worthy state). [`HealthState::IdleLong`] transitions
    /// return `false` so cron escalation consumers (interrupt
    /// / replace) keep their existing semantics — they only act on `Hung`.
    ///
    /// Mutator monopoly: [`Self::maybe_decay`] does NOT touch
    /// [`HealthState::Hung`]; all Hung mutations are inside this function
    /// (entries below, exit at the silence-drops branch). See
    /// `docs/HUNG-STATE-TRANSITIONS.md §Invariants`.
    ///
    /// **F9 (#685 sub-task 4)**: `silent_productive` is the dual-path
    /// supplement signal (silence-since-last-productive-output vs the
    /// existing `silent` = silence-since-any-output). When the env var
    /// `AGEND_PRODUCTIVE_GATE=1` is set, the productive-silence path can
    /// trigger Hung classification independently of the silent path —
    /// catching the F9 grey failure where 1-byte spinner output keeps
    /// `silent` below threshold while no productive work happens. Default
    /// (env var unset) behavior is shadow-mode: telemetry collected, no
    /// classification change. See `docs/F9-PRODUCTIVE-OUTPUT-GATE.md` §F9.5.
    pub fn check_hang(
        &mut self,
        agent_state: AgentState,
        silent: Duration,
        silent_productive: Duration,
        last_input_at_ms: u64,
        last_heartbeat_at_ms: u64,
    ) -> bool {
        // `#685` sub-task 7a guard: `Paused` is operator-action-required
        // terminal state — auto-recovery dispatcher already escalated
        // through Stage 3 and stopped. `check_hang` must NOT mutate state
        // back to `Hung` or trigger further dispatcher work. Return false
        // immediately so the upstream `tracing::warn!` at the hang-detection
        // tick site is suppressed too (operator already alerted via Stage 3
        // telegram notify; further warns would be noise).
        if self.state == HealthState::Paused {
            return false;
        }

        // Race mutex: skip hang check when agent is blocked for a known
        // reason that legitimately suppresses output (#1638: policy on the type).
        if let Some(ref reason) = self.current_reason {
            if reason.suppresses_hang_check() {
                return false;
            }
        }

        // MCP activity within the window keeps the agent active/healthy
        let mcp_active = if let Some(last_mcp) = self.last_mcp_activity_at_epoch_ms {
            let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
            let elapsed = Duration::from_millis(now_ms.saturating_sub(last_mcp));
            !productive_silence_exceeds(agent_state, elapsed)
        } else {
            false
        };

        if mcp_active {
            if matches!(self.state, HealthState::Hung | HealthState::IdleLong) {
                self.state = HealthState::Healthy;
                self.hung_since = None;
            }
            return false;
        }

        let silence_exceeds_threshold = productive_silence_exceeds(agent_state, silent);

        // F9 (#685 sub-task 4): productive-silence threshold mirrors silent
        // thresholds. Active only when `AGEND_PRODUCTIVE_GATE=1` is set;
        // telemetry fires regardless for fixture-corpus measurement.
        let productive_exceeds = productive_silence_exceeds(agent_state, silent_productive);
        let f9_gate_active = self.productive_gate
            || std::env::var("AGEND_PRODUCTIVE_GATE")
                .map(|v| v == "1")
                .unwrap_or(false);
        // Shadow-mode telemetry: fires when the productive path would
        // independently flag Hung but the silent path does not. Lets the
        // fixture corpus measure F9 FP rate without affecting prod behavior
        // until the env var flips to active.
        if productive_exceeds && !silence_exceeds_threshold {
            tracing::debug!(
                target: "behavioral_shadow",
                silent_secs = silent.as_secs(),
                silent_productive_secs = silent_productive.as_secs(),
                agent_state = ?agent_state,
                active = f9_gate_active,
                "F9 dual-path candidate: silent_productive exceeded without silent"
            );
        }
        // Dual-path: Hung classification fires if either path exceeded,
        // gated on env var for productive path until promoted to default.
        let any_path_exceeds = silence_exceeds_threshold || (f9_gate_active && productive_exceeds);

        if !any_path_exceeds {
            // Hung Exit (X1) / IdleLong Exit (X1): silence dropped below threshold
            // PRE: state in {Hung, IdleLong}, !silence_exceeds_threshold
            // POST: state = Healthy, check_hang returns false
            // FP vector: F10 — any 1 byte of output (spinner tick, log line) flips
            //   Hung → Healthy without productive-work evidence.
            // FN vector: indirect via FP (stale "Healthy" hides genuine stuck agent).
            // See docs/HUNG-STATE-TRANSITIONS.md §Exit.X1
            if matches!(self.state, HealthState::Hung | HealthState::IdleLong) {
                self.state = HealthState::Healthy;
                // #1744-H2: explicit clear on Hung EXIT. Pre-#1744 this was unneeded
                // (every entry overwrote `hung_since`), but now that entry uses
                // `get_or_insert` so a rehydrated anchor survives a restart, a
                // recovered episode MUST drop its anchor here — else a stale value
                // would be persisted and a later unrelated Hung would inherit a
                // bogus (already-elapsed) confirm-window.
                self.hung_since = None;
            }
            return false;
        }

        // Sprint 24 P1 discriminator: input pending past last response
        // (heartbeat) → real Hung. Otherwise (no input pending OR input
        // already responded to) → IdleLong (no escalation).
        //
        // Grace window prevents flapping when input arrives 1-tick before
        // the heartbeat write completes. 5s mirrors typical MCP roundtrip
        // upper-bound for a non-busy agent.
        const INPUT_RESPONSE_GRACE_MS: u64 = 5_000;
        let input_pending_past_response = last_input_at_ms > 0
            && last_input_at_ms > last_heartbeat_at_ms.saturating_add(INPUT_RESPONSE_GRACE_MS);

        if input_pending_past_response {
            // Real hung: input was delivered but agent has not responded
            // (no MCP call to refresh heartbeat). Operator-facing log
            // includes delta diagnostic per self-diagnostic pattern (PR #241
            // praise pattern transfer).
            let delta_ms = last_input_at_ms.saturating_sub(last_heartbeat_at_ms);
            tracing::warn!(
                last_input_at_ms,
                last_heartbeat_at_ms,
                input_response_delta_ms = delta_ms,
                silent_secs = silent.as_secs(),
                agent_state = ?agent_state,
                "agent classified Hung — input pending {delta_ms}ms past last heartbeat (escalation-worthy)"
            );
            // Hung Entry (E1): input pending past heartbeat_deadline
            // PRE: !blocked-reason race mutex, silence > threshold,
            //   last_input_at_ms > last_heartbeat_at_ms + 5s grace,
            //   state != Hung
            // POST: state = Hung, check_hang returns true (first detection only)
            // FP vector: operator typed input but agent is genuinely producing
            //   keystrokes draining through MCP; bounded by heartbeat refresh.
            // FN vector: F9 grey failure — 1-byte spinner output resets silent
            //   timer in StateTracker; never crosses threshold.
            // See docs/HUNG-STATE-TRANSITIONS.md §Entry.E1
            if self.state != HealthState::Hung {
                self.state = HealthState::Hung;
                // #1744-H2: `get_or_insert` (not `= now`) so a rehydrated cross-restart
                // anchor survives this first post-restart re-entry → the confirm-window
                // continues rather than resetting to 0.
                self.hung_since.get_or_insert_with(Instant::now);
                return true; // First hang detection — caller escalates
            }
            return false;
        }

        // No input pending past response → idle waiting (operator 04:00
        // UTC false-alarm pattern). Mark IdleLong so consumers can
        // distinguish from Hung but cron escalation MUST NOT trigger.
        //
        // Sprint 24 P2 F1 cross-check: heartbeat fresh but PTY silent →
        // agent is calling MCP tools (refreshing heartbeat) without
        // producing PTY output. Indicates a stuck agent in a tight MCP
        // loop — operator should be notified for diagnosis.
        let heartbeat_age_ms =
            crate::daemon::heartbeat_pair::now_ms().saturating_sub(last_heartbeat_at_ms);
        let heartbeat_fresh =
            last_heartbeat_at_ms > 0 && heartbeat_age_ms < silent.as_millis() as u64;
        if heartbeat_fresh {
            let delta_ms = silent.as_millis() as u64;
            tracing::warn!(
                last_heartbeat_at_ms,
                heartbeat_age_ms,
                silent_ms = delta_ms,
                agent_state = ?agent_state,
                "agent classified Hung — heartbeat fresh but PTY silent (F1 cross-check)"
            );
            // Hung Entry (E2): heartbeat fresh but PTY silent (F1 cross-check)
            // PRE: !blocked-reason race mutex, silence > threshold,
            //   !input_pending_past_response (E1 did not fire),
            //   last_heartbeat_at_ms > 0 AND heartbeat_age_ms < silent.as_millis(),
            //   state != Hung
            // POST: state = Hung, check_hang returns true (first detection only)
            // FP vector: F39 — stale AgentState::Active pattern in vterm scrollback;
            //   bounded by LATCHED_STATE_EXPIRY (30s) but not perfectly.
            // FN vector: F9 grey failure — same shape as §Entry.E1.
            // See docs/HUNG-STATE-TRANSITIONS.md §Entry.E2
            if self.state != HealthState::Hung {
                self.state = HealthState::Hung;
                // #1744-H2: see §Entry.E1 — `get_or_insert` preserves a rehydrated anchor.
                self.hung_since.get_or_insert_with(Instant::now);
                return true;
            }
            return false;
        }

        // IdleLong Entry (E1): silent past threshold, no input pending
        // PRE: !blocked-reason race mutex, silence > threshold,
        //   !input_pending_past_response, !heartbeat_fresh, state != IdleLong
        // POST: state = IdleLong, check_hang returns false (escalation
        //   consumers act only on Hung per rustdoc contract above)
        // FP vector: genuinely idle agent waiting for next operator prompt
        //   (04:00 UTC false-alarm pattern that motivated splitting Hung/IdleLong)
        // FN vector: F9 grey failure — same shape as §Entry.E1.
        // See docs/HUNG-STATE-TRANSITIONS.md §IdleLong.Entry.E1
        if self.state != HealthState::IdleLong {
            tracing::debug!(
                last_input_at_ms,
                last_heartbeat_at_ms,
                silent_secs = silent.as_secs(),
                agent_state = ?agent_state,
                "agent classified IdleLong — silent past threshold but no input pending (no escalation)"
            );
            self.state = HealthState::IdleLong;
        }
        false
    }

    /// Set the current blocked reason. Prevents `check_hang` from
    /// misdiagnosing expected waits as hangs.
    pub fn set_blocked_reason(&mut self, reason: BlockedReason) {
        self.current_reason = Some(reason);
        // #1933: a fresh reason resets any prior annotation (an auto-detected
        // reason from the supervisor carries no note). The report path sets the
        // note right after via `set_blocked_note`.
        self.current_note = None;
    }

    /// #1933: attach the operator-readable annotation to the current blocked reason.
    pub fn set_blocked_note(&mut self, note: Option<String>) {
        self.current_note = note;
    }

    /// Clear the current blocked reason, resuming normal hang detection.
    pub fn clear_blocked_reason(&mut self) {
        self.current_reason = None;
        self.current_note = None;
    }

    /// Record an error state. Returns true if error loop detected (3x in 10min).
    #[allow(dead_code)] // wired by daemon health monitoring; used in tests
    pub fn record_error(&mut self, state: AgentState) -> bool {
        let now = Instant::now();
        self.error_events.push_back((now, state));

        // Clean old events
        while let Some((t, _)) = self.error_events.front() {
            // #1744-H2: saturating — coarse-clock same-tick (t >= now) → 0.
            if now.saturating_duration_since(*t) > CRASH_WINDOW {
                self.error_events.pop_front();
            } else {
                break;
            }
        }

        let count = self
            .error_events
            .iter()
            .filter(|(_, s)| *s == state)
            .count();

        if count >= 3 {
            self.state = HealthState::ErrorLoop;
            true
        } else {
            false
        }
    }

    /// Get crash reason string for inject hint.
    pub fn crash_reason(&self) -> &'static str {
        match self.state {
            HealthState::Recovering => "crash",
            HealthState::Unstable => "repeated crashes",
            HealthState::Failed => "too many crashes",
            HealthState::ErrorLoop => "error loop",
            HealthState::Absent => "absent",
            _ => "unknown",
        }
    }

    /// Decay total_crashes if stable for STABILITY_WINDOW.
    /// Call periodically from daemon main loop.
    ///
    /// `#685` sub-task 7a guard: `Paused` is operator-action-required —
    /// crash decay must NOT exit `Paused` (only operator unpause can).
    /// `Paused` is reachable only via Stage 3 dispatcher, never via
    /// crash counter or decay paths.
    pub fn maybe_decay(&mut self, process_alive: bool) {
        self.maybe_decay_at(Instant::now(), process_alive);
    }

    /// Test-injection variant of [`Self::maybe_decay`] — accepts a
    /// caller-supplied `now` so tests can simulate elapsed time without
    /// constructing backdated `Instant` values via subtraction.
    ///
    /// **Why this exists**: Windows `Instant::now()` is anchored to system
    /// uptime via `QueryPerformanceCounter`. On a fresh CI VM with low
    /// uptime, `Instant::now() - Duration::from_secs(30 * 60)` underflows
    /// and panics. Tests instead use `base + offset` (cross-platform safe;
    /// `Instant::add` saturates to `Instant::MAX` on all platforms) and
    /// pass the resulting future-Instant as the `now` argument. Internal
    /// elapsed checks use `now.saturating_duration_since(t)` defensively
    /// against clock skew.
    ///
    /// Production callers should always use [`Self::maybe_decay`] which
    /// passes `Instant::now()` — zero behaviour change. Sub-task 7b PR
    /// #775 v2 hot-fix.
    pub(crate) fn maybe_decay_at(&mut self, now: Instant, process_alive: bool) {
        if self.state == HealthState::Paused {
            return;
        }
        // `#685` sub-task 7b: decay recovery_restart_count via the same
        // STABILITY_WINDOW discipline as crash decay. Independent of
        // crash counter — if agent went through Stage 2 restart without
        // crashing, last_stage2_fired_at drives the decay clock alone.
        // Mirrors decision §Delta 3: long-stability decay (NOT
        // reset-on-Healthy, which oscillates too aggressively).
        if self.recovery_restart_count > 0 {
            let last_stage2_idle = self
                .last_stage2_fired_at
                .map(|t| now.saturating_duration_since(t) >= STABILITY_WINDOW)
                .unwrap_or(false);
            if last_stage2_idle {
                self.recovery_restart_count = self.recovery_restart_count.saturating_sub(1);
                // [M1] advance the decay anchor so the NEXT unit needs another full
                // STABILITY_WINDOW — without this, once one window elapsed every
                // subsequent tick (~10s) decremented, draining the count in seconds.
                self.last_stage2_fired_at = Some(now);
            }
        }
        if self.total_crashes == 0 {
            return;
        }
        let last_crash = match self.crash_times.back() {
            Some(t) => *t,
            None => return,
        };
        // [M1] decay clock = the LATER of the most-recent crash and the last decay.
        // `crash_times.back()` never moves on decrement, so keying on it alone let
        // the whole budget drain in consecutive ticks once the first 30-min window
        // passed; `crash_decay_at` (advanced below) makes it 1 unit per window.
        let crash_anchor = match self.crash_decay_at {
            Some(d) if d > last_crash => d,
            _ => last_crash,
        };
        if now.saturating_duration_since(crash_anchor) >= STABILITY_WINDOW {
            self.total_crashes = self.total_crashes.saturating_sub(1);
            self.crash_decay_at = Some(now); // advance the decay anchor
            if self.total_crashes == 0 {
                self.crash_times.clear();
                self.crash_decay_at = None; // budget cleared → reset decay clock
            }
            // Recover from Failed/Unstable if crashes decayed enough.
            // Known limitation: Failed → Recovering with a dead process
            // leaves the agent in Recovering until operator manual restart
            // or #685 Stage 2 auto-restart fires. record_crash returned
            // (false, _, _) when entering Failed, so the process is gone.
            if self.total_crashes < DEFAULT_MAX_RETRIES && self.state == HealthState::Failed {
                if process_alive {
                    self.state = HealthState::Recovering;
                } else {
                    self.state = HealthState::Absent;
                    tracing::debug!(
                        "maybe_decay_at: process not alive, transitioning Failed to Absent"
                    );
                }
            }
            if self.total_crashes < 3
                && matches!(self.state, HealthState::Unstable | HealthState::Recovering)
            {
                if process_alive {
                    self.state = HealthState::Healthy;
                } else {
                    self.state = HealthState::Absent;
                    tracing::debug!("maybe_decay_at: process not alive, transitioning state to Absent instead of Healthy");
                }
            }
        }
    }

    /// Reset health state (e.g., after manual restart).
    #[allow(dead_code)] // used in tests; available for manual restart path
    pub fn reset(&mut self) {
        self.state = HealthState::Healthy;
        self.crash_times.clear();
        self.total_crashes = 0;
        self.error_events.clear();
        self.current_reason = None;
    }
}

#[cfg(test)]
mod tests;
