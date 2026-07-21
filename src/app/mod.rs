//! Terminal application — multi-tab/pane TUI for agent management.
//!
//! Uses agent::spawn_agent() for all panes (agents and shells), sharing the
//! same PTY lifecycle as the daemon: auto-dismiss, state tracking, broadcast.

mod api_server;
// #t-5: `pub(crate)` so `render::overlay` can read the completion specs
// (`CommandSpec` / `COMMAND_SPECS` / `matching_specs`). `execute` stays
// `pub(super)` = app-only, so command EXECUTION is not widened.
pub(crate) mod commands;
mod dispatch;
mod frame_timing;
mod mouse;
mod overlay;
mod pane_factory;
mod session;
mod telegram_hooks;
mod app_state;
mod tui_events;
mod tui_spawn;
mod ui_state;

use app_state::{AppDeps, AppState, LoopFlow};
use ui_state::{UiDeps, UiState};

pub use overlay::{BoardView, DecisionMode, MenuItem, MenuItemKind, TaskBoardMode};
pub(crate) use tui_events::{TuiEvent, TuiEventSender, TuiNotifier};

use crate::agent::{self, AgentRegistry};
use crate::backend::Backend;
use crate::channel::TelegramStatus;
use crate::keybinds::KeyHandler;
use crate::layout::{Layout, Pane};
use crate::notification_queue;
use crate::render;
use frame_timing::{
    should_draw, should_sync_notifications, trace_tty_size, BOOT_FRAME_TIME_CAP, FRAME_INTERVAL,
    MAX_BOOT_CATCHUP, NOTIF_SYNC_INTERVAL,
};
use overlay::{CloseTarget, Overlay, OverlayCtx};

use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use parking_lot::Mutex;
use ratatui::DefaultTerminal;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Run the terminal application.
pub fn run(fleet_path_override: Option<&str>) -> Result<()> {
    // Redirect tracing to log file BEFORE ratatui takes over stderr.
    // Must happen before main.rs's tracing init — caller should skip init for App.
    let home = crate::home_dir();

    // Extract embedded fleet protocol to AGEND_HOME/protocol/.default/
    crate::protocol::extract_default(&home);

    // #927 PR-A: was a raw `OpenOptions::truncate(true)` write on
    // `app.log` with hardcoded `debug` filter — long sessions hit
    // unbounded growth (operator-observed). Now uses the parameterized
    // rolling-appender shared with the daemon path:
    //   - DAILY rotation, retain N days (env: AGEND_LOG_RETAIN_DAYS).
    //   - Default filter `agend_terminal=info` (was `debug`); opt into
    //     verbose via `AGEND_LOG=agend_terminal=debug`.
    //   - First-boot pre-rotation `app.log` is dropped (synthesis policy:
    //     tiny file, no rescue value).
    //
    // Guard lifetime: the `WorkerGuard` returned by setup_rolling_tracing
    // must outlive the entire app session; drop = flush + close the
    // worker thread. Bound here in `app::run`'s scope so it lives until
    // the fn returns (the entire TUI loop lifetime).
    let _app_log_guard = crate::logging::setup_rolling_tracing(
        &home,
        "app",
        "agend_terminal=info",
        crate::logging::MigrationPolicy::Drop,
    )
    .ok();

    let fleet_path = fleet_path_override.map(PathBuf::from);

    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste,
    )
    .ok();

    let mut terminal = ratatui::init();

    // Push keyboard enhancement AFTER entering alternate screen — Kitty
    // protocol push/pop stack is per-screen, so pushing on the main screen
    // is lost when ratatui::init() switches to the alternate screen.
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PushKeyboardEnhancementFlags(
            crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        ),
    )
    .ok();

    // Panic hook: restore terminal on panic so the user doesn't get stuck
    // in raw mode with mouse capture enabled. Chains the original hook so
    // panic messages still print.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(
            std::io::stderr(),
            crossterm::event::PopKeyboardEnhancementFlags,
        );
        ratatui::restore();
        let _ = crossterm::execute!(
            std::io::stderr(),
            crossterm::event::DisableMouseCapture,
            crossterm::event::DisableBracketedPaste,
        );
        original_hook(info);
    }));

    let result = run_app(&mut terminal, fleet_path.as_deref());

    // Restore default panic hook before normal cleanup (avoid double-restore).
    drop(std::panic::take_hook());

    // Pop before leaving alternate screen (symmetric with push).
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PopKeyboardEnhancementFlags,
    )
    .ok();

    ratatui::restore();

    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
    )
    .ok();
    result
}

/// #1726: per-tick handlers that app-standalone INTENTIONALLY does not run,
/// excluded from the otherwise-shared `build_default_handlers` set. Each is a
/// deliberate, justified omission; the completeness invariant
/// (`app_tick_handlers_cover_every_non_allowlisted_daemon_handler`) fails CI if a
/// NEW handler is added to `build_default_handlers` but is neither run in app nor
/// listed here — so additions are a conscious decision, not silent drift.
///
/// - `snapshot_rotation`: app owns session persistence via `session::save_session_if_changed`.
/// - `thread_dump`: env-gated diagnostic, not needed in the interactive TUI.
///
/// #1694(a): `recovery_dispatcher` was REMOVED from this allowlist — the live
/// daemon runs in app mode (`app::run_app`, never `run_core`), so allowlisting it
/// out meant the #685 recovery ladder was silently dead in the live daemon (the
/// #1720 class). It now runs in app mode: Stage1 (ESC-nudge to the PTY) needs no
/// `crash_rx`; Stage2 (restart) has no app-mode consumer, so it escalates to
/// Stage3 instead of silent-dropping (see `build_default_handlers`'
/// `stage2_dispatch_available = false` below). All stages stay shadow-gated-off by
/// default — zero behavior change unless an operator opts in.
///
/// #2413 Phase B (live-fix): `shadow_observe` was REMOVED from this allowlist (same
/// #1720/#685 class as `recovery_dispatcher` above). The original #2433 allowlisted it
/// out, reasoning the plane was "run_core-only" — but the LIVE fleet daemon runs
/// `agend-terminal app` (`run_app`), NEVER `run_core`, so that gated the whole Shadow
/// Observer DEAD in production: `observed_status` stayed null on every agent even with
/// the flag on. The fix is symmetric to #1694(a): the reducer driver now runs in app mode
/// (un-allowlisted), and `run_app` starts the hook-event socket server (`shadow::start`)
/// alongside the api-activity probe. Flag-OFF by default ⇒ zero behaviour change.
/// Per-tick handlers ALWAYS allowlisted out of app-standalone (unconditional skips).
/// `thread_dump` is an env-gated diagnostic not needed in the interactive TUI.
///
/// #2413 PR-B (#1720-class fix): `snapshot_rotation` was REMOVED from this list — see
/// [`app_snapshot_rotation_enabled`]. The live daemon runs `run_app`, never `run_core`, so
/// allowlisting `snapshot_rotation` out meant the live daemon NEVER wrote `<home>/snapshot.json`
/// (it was weeks-stale), and `dispatch_idle` / inbox / handoff / reply — which read it — all
/// operated on stale state. Same #1720 class already fixed for `recovery_dispatcher` (#1694a)
/// and `shadow_observe` (#2413 Phase B). It now runs in app mode by default, reversible via the
/// `AGEND_APP_SNAPSHOT=0` kill-switch.
const APP_TICK_ALLOWLIST: &[&str] = &["thread_dump"];

/// #2413 PR-B: whether app-standalone runs `snapshot_rotation` (writes `snapshot.json` every
/// tick). **Default ON** — the #1720-class fix so the live daemon's snapshot-reading deciders
/// (dispatch_idle / inbox / handoff / reply) see CURRENT state. The `AGEND_APP_SNAPSHOT=0`
/// kill-switch restores the pre-PR-B behaviour (allowlisted out — no app-mode snapshot write),
/// a reversible escape hatch since flipping the whole snapshot plane stale→live is a behaviour
/// change with a broad blast radius.
fn app_snapshot_rotation_enabled() -> bool {
    std::env::var("AGEND_APP_SNAPSHOT").as_deref() != Ok("0")
}

/// Build the per-tick handler set app-standalone runs: the shared
/// `build_default_handlers` minus `APP_TICK_ALLOWLIST` (and minus `snapshot_rotation` only when
/// the `AGEND_APP_SNAPSHOT=0` kill-switch is set). Extracted so the completeness invariant can
/// compare it against the full daemon set.
///
/// #1694(a): `crash_tx` here is a throwaway sender (its receiver is dropped), so
/// `stage2_dispatch_available = false` — `RecoveryDispatcherHandler` now RUNS
/// (no longer allowlisted out) and its Stage2 path escalates to Stage3 rather
/// than emit onto the consumerless channel.
fn app_tick_handlers(
    daemon_binary_stale: crate::daemon::mcp_registry_watcher::DaemonBinaryStale,
) -> Vec<Box<dyn crate::daemon::per_tick::PerTickHandler>> {
    let (crash_tx, _crash_rx) = crossbeam_channel::bounded(1);
    let mut handlers = crate::daemon::build_default_handlers(crash_tx, false, daemon_binary_stale);
    let skip_snapshot = !app_snapshot_rotation_enabled();
    handlers.retain(|h| {
        let name = h.name();
        if APP_TICK_ALLOWLIST.contains(&name) {
            return false;
        }
        // #2413 PR-B: snapshot_rotation runs by default; the kill-switch skips it.
        if skip_snapshot && name == "snapshot_rotation" {
            return false;
        }
        true
    });
    handlers
}

/// Main event loop for the TUI app.
///
/// M5 note: this function is 550+ lines with 15+ locals. Extraction to
/// `app/event_loop.rs` deferred — the function is a single coherent event
/// loop with no natural split point that wouldn't increase coupling.
/// Locals are all loop-scoped state (layout, registry, overlay, etc.)
/// that the event loop needs in every iteration. Splitting would require
/// passing all state as a context struct, adding complexity without
/// reducing cognitive load. Revisit if the function grows further.
/// #2057 instrument (gated on `AGEND_TUI_SIZE_DEBUG=1`): log the controlling
/// TTY's kernel winsize (crossterm reads fd 1) at a named STARTUP milestone.
/// The operator A/B showed fd-1 rows drop 56→53 only in the default home (12
/// agents / 7 tabs) — somewhere in startup a phase shrinks the TUI's OWN
/// terminal. Bracketing the phases (baseline → post-fleet-spawn → pre-loop)
/// pins which one; the per-frame loop probe (`#2057-size`) shows the loop only
/// ever observes the post-shrink value, so the culprit is pre-loop.
/// #2050 simplify PR-C (②): render the active overlay on top of the main frame.
/// Extracted verbatim from the two byte-identical blocks in `run_app` — the normal
/// draw path and the screenshot (TestBackend) path — so they can't drift. Takes
/// `&mut Overlay` because `ScratchShell` drains/resizes its pane during render.
fn render_active_overlay(
    frame: &mut ratatui::Frame,
    overlay: &mut Overlay,
    layout: &Layout,
    registry: &AgentRegistry,
    home: &Path,
) {
    match overlay {
        Overlay::NewTabMenu { items, selected }
        | Overlay::SplitMenu {
            items, selected, ..
        } => {
            render::render_menu(frame, items, *selected);
        }
        Overlay::RenameTab { input } | Overlay::RenamePane { input } => {
            render::render_rename(frame, input);
        }
        Overlay::ConfirmClose { target } => {
            let msg = match target {
                CloseTarget::Pane => "Close pane? (y/n)",
                CloseTarget::Tab => "Close tab and kill all agents? (y/n)",
            };
            render::render_confirm(frame, msg);
        }
        Overlay::TabList { selected } => {
            render::render_tab_list(frame, layout, *selected);
        }
        Overlay::MovePaneTarget {
            selected,
            source_tab_idx,
            split_dir,
            ..
        } => {
            render::render_move_pane_target(frame, layout, *selected, *source_tab_idx, *split_dir);
        }
        Overlay::Help => {
            render::render_help(frame);
        }
        Overlay::Scroll => {
            let so = layout
                .active_tab()
                .and_then(|t| t.focused_pane())
                .map(|p| p.scroll_offset)
                .unwrap_or(0);
            render::render_scroll_indicator(frame, so);
        }
        Overlay::Command {
            ref input,
            selected,
        } => {
            // Compute the completion once (same `palette_completion` the key
            // handler uses) and hand it to the renderer, so the highlighted
            // candidate always matches what Tab completes. Registry is touched
            // only for agent-argument completion — off the per-pane render path.
            let completion = commands::palette_completion(input, registry);
            render::render_command_palette(frame, input, *selected, &completion);
        }
        Overlay::Decisions {
            ref items,
            selected,
            ref mode,
        } => {
            render::render_decisions(frame, items, *selected, mode);
        }
        Overlay::Tasks {
            ref items,
            col,
            row,
            ref mode,
            ref view,
        } => {
            render::render_tasks(frame, items, *col, *row, mode, *view, home);
        }
        Overlay::ScratchShell { pane } => {
            render::render_scratch_shell(frame, pane, registry);
        }
        Overlay::None => {}
    }
}

fn app_boot_preflight(home: &Path) {
    crate::runtime_config::reload(home);
}

fn start_owned_services(
    home: &Path,
    registry: &AgentRegistry,
    telegram_state: &Option<std::sync::Arc<dyn crate::channel::Channel>>,
    attached_mode: bool,
) {
    if !attached_mode {
        crate::daemon::supervisor::spawn(home.to_path_buf(), Arc::clone(registry));
        crate::instance_monitor::spawn_monitor_tick(home.to_path_buf(), Arc::clone(registry));
        crate::api_activity_probe::spawn(Arc::clone(registry));
        crate::daemon::shadow::start(home);
        crate::daemon::shadow::rollout::spawn(Arc::clone(registry), home.to_path_buf());
        crate::daemon::shadow::opencode::spawn(Arc::clone(registry), home.to_path_buf());
        crate::daemon::shadow::kiro::spawn(Arc::clone(registry), home.to_path_buf());
        crate::daemon::shadow::grok::spawn(Arc::clone(registry), home.to_path_buf());
        crate::daemon::shadow::agy::spawn(Arc::clone(registry), home.to_path_buf());
        crate::agent::set_pending_registry(Arc::clone(registry));
        if let Some(tg) = telegram_state.as_ref() {
            tg.attach_registry(Arc::clone(registry));
        } else if let Some(tg) = crate::channel::active_channel() {
            tg.attach_registry(Arc::clone(registry));
        }
    }
}

fn spawn_crossterm_event_reader() -> crossbeam_channel::Receiver<Event> {
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<Event>();
    std::thread::Builder::new()
        .name("crossterm_events".into())
        .spawn(move || loop {
            if let Ok(ev) = event::read() {
                if event_tx.send(ev).is_err() {
                    break;
                }
            }
        })
        .ok();
    event_rx
}

fn register_app_event_bus(attached_mode: bool, registry: &AgentRegistry) {
    if !attached_mode {
        crate::daemon::register_event_subscribers(registry);
    }
}

fn spawn_app_tick(
    attached_mode: bool,
) -> (
    Option<crossbeam_channel::Receiver<()>>,
    crossbeam_channel::Receiver<()>,
) {
    let tick_rx = if !attached_mode {
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name("app_tick".into())
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(10));
                if tx.send(()).is_err() {
                    break;
                }
            })
            .ok();
        Some(rx)
    } else {
        None
    };
    let never_rx = crossbeam_channel::never::<()>();
    (tick_rx, never_rx)
}

fn build_app_maintenance(
    attached_mode: bool,
    daemon_binary_stale: &crate::daemon::mcp_registry_watcher::DaemonBinaryStale,
) -> (
    crate::agent::ExternalRegistry,
    crate::api::ConfigRegistry,
    Vec<Box<dyn crate::daemon::per_tick::TickHandler>>,
) {
    let app_externals: crate::agent::ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
    let app_configs: crate::api::ConfigRegistry = Arc::new(Mutex::new(HashMap::new()));
    let app_handlers = if !attached_mode {
        app_tick_handlers(Arc::clone(daemon_binary_stale))
    } else {
        Vec::new()
    };
    (app_externals, app_configs, app_handlers)
}

fn log_pre_render_milestone(size_debug: bool, restore_start: std::time::Instant, attached_mode: bool) {
    trace_tty_size(size_debug, "pre-render-loop");
    tracing::info!(
        phase = "pre-render-loop",
        elapsed_ms = restore_start.elapsed().as_millis() as u64,
        attached = attached_mode,
        "pre-render-loop: entering render loop (first draw imminent)"
    );
}

fn term_requested_logged() -> bool {
    if crate::bootstrap::signals::term_requested() {
        tracing::info!("app: SIGTERM received, exiting main loop");
        true
    } else {
        false
    }
}

fn run_app(terminal: &mut DefaultTerminal, fleet_override: Option<&Path>) -> Result<()> {
    let home = crate::home_dir();
    app_boot_preflight(&home);
    let fleet_path = fleet_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| crate::fleet::fleet_yaml_path(&home));

    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let daemon_binary_stale: crate::daemon::mcp_registry_watcher::DaemonBinaryStale =
        Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tui_event_tx, tui_event_rx) = crossbeam_channel::bounded::<TuiEvent>(256);
    let _tui_event_tx_keepalive = tui_event_tx.clone();

    let (_api_guard, telegram_state, telegram_status, attached_run_dir) =
        setup_app_bootstrap(&home, &fleet_path, &registry, tui_event_tx);
    let attached_mode = attached_run_dir.is_some();

    start_owned_services(&home, &registry, &telegram_state, attached_mode);
    let mut state = AppState::new();
    let (wakeup_tx, wakeup_rx) = crossbeam_channel::unbounded::<usize>();

    let size_debug = std::env::var("AGEND_TUI_SIZE_DEBUG").as_deref() == Ok("1");
    trace_tty_size(size_debug, "startup-baseline");
    let restore_start = std::time::Instant::now();

    let deps = AppDeps {
        home: &home,
        fleet_path: &fleet_path,
        registry: &registry,
        wakeup_tx: &wakeup_tx,
        daemon_binary_stale: &daemon_binary_stale,
        telegram_status,
        attached_run_dir: &attached_run_dir,
        attached_mode,
        size_debug,
    };

    let (_attach_tx, attach_rx, attach_workers) = state.restore_and_attach(&deps, restore_start)?;
    let event_rx = spawn_crossterm_event_reader();
    register_app_event_bus(attached_mode, &registry);
    let (tick_rx, never_rx) = spawn_app_tick(attached_mode);
    let (app_externals, app_configs, app_handlers) =
        build_app_maintenance(attached_mode, &daemon_binary_stale);
    log_pre_render_milestone(size_debug, restore_start, attached_mode);

    loop {
        if term_requested_logged() || state.poll_restart(&deps) == LoopFlow::Break {
            break;
        }
        state.pre_select(terminal, &deps);
        state.render_frame(terminal, &deps)?;
        crossbeam_channel::select! {
            recv(event_rx) -> ev => {
                if state.handle_crossterm_event(ev, terminal, &deps) == LoopFlow::Break {
                    break;
                }
            }
            recv(wakeup_rx) -> _ => state.handle_wakeup(&wakeup_rx),
            recv(attach_rx) -> outcome => state.handle_attach_outcome(outcome, &deps),
            recv(tui_event_rx) -> ev => state.handle_tui_event(ev, terminal, &deps),
            recv(tick_rx.as_ref().unwrap_or(&never_rx)) -> _ => {
                state.handle_maintenance_tick(&deps, &app_externals, &app_configs, &app_handlers)
            }
            default(state.select_timeout()) => state.handle_idle_tick(&deps),
        }
    }

    app_teardown(&home, &state.ui.layout, &registry, attached_mode, attach_workers);
    Ok(())
}

/// App startup bootstrap: prepare the fleet (issuing `api.cookie` BEFORE any API
/// server thread starts — otherwise Telegram's router `api::call(INJECT)` would
/// silently fail), then either start the in-process API server + the SIGTERM
/// handler (Owned) or note the run dir to connect to (Attached). Extracted
/// verbatim from the head of `run_app` (#14 god-fn split) — byte-identical.
///
/// Returns `(api_guard, telegram_channel, telegram_status, attached_run_dir)`.
/// The RAII `ApiGuard` must outlive the TUI loop, so the caller binds it;
/// `attached_run_dir.is_some()` ⇒ Attached mode.
fn setup_app_bootstrap(
    home: &Path,
    fleet_path: &Path,
    registry: &AgentRegistry,
    tui_event_tx: TuiEventSender,
) -> (
    api_server::ApiGuard,
    Option<Arc<dyn crate::channel::Channel>>,
    TelegramStatus,
    Option<PathBuf>,
) {
    let opts = crate::bootstrap::PrepareOptions {
        resolve_agents: false, // app spawns via pane_factory from tabs
        ..Default::default()
    };
    let mut attached_run_dir: Option<PathBuf> = None;
    let (api_guard, telegram_state, telegram_status) =
        match crate::bootstrap::prepare(home, fleet_path, opts) {
            Ok(crate::bootstrap::BootstrapOutcome::Owned(prepared)) => {
                let telegram = prepared.telegram.clone();
                let status = if telegram.is_some() {
                    TelegramStatus::Connected
                } else {
                    telegram_hooks::telegram_status_from_config(&prepared.config)
                };
                let guard = api_server::start_api_server(prepared, registry, tui_event_tx);
                // SIGTERM-only handler: `agend-terminal stop` can cleanly exit
                // the owned app. SIGINT stays with crossterm so Ctrl+C still
                // reaches the focused pane's PTY as 0x03.
                crate::bootstrap::signals::install_term_only();
                (guard, telegram, status)
            }
            Ok(crate::bootstrap::BootstrapOutcome::Attached(attached)) => {
                tracing::info!(
                    pid = attached.daemon_pid,
                    path = %attached.run_dir.display(),
                    "attached to existing daemon, connecting as remote client"
                );
                attached_run_dir = Some(attached.run_dir.clone());
                (
                    api_server::noop_guard(),
                    None,
                    TelegramStatus::NotConfigured,
                )
            }
            Err(e) => {
                tracing::warn!(error = %e, "bootstrap failed, running TUI without in-process API");
                (
                    api_server::noop_guard(),
                    None,
                    TelegramStatus::NotConfigured,
                )
            }
        };
    (api_guard, telegram_state, telegram_status, attached_run_dir)
}

/// Periodic owned-mode maintenance: run the full daemon per-tick handler
/// pipeline once. Extracted verbatim from `run_app`'s tick arm (#14 god-fn
/// split) — byte-identical, no behaviour change.
pub(super) fn app_maintenance_tick(
    home: &Path,
    registry: &AgentRegistry,
    externals: &crate::agent::ExternalRegistry,
    configs: &crate::api::ConfigRegistry,
    handlers: &[Box<dyn crate::daemon::per_tick::PerTickHandler>],
) {
    let tick_ctx = crate::daemon::per_tick::TickContext {
        home,
        registry,
        externals,
        configs,
    };
    crate::daemon::per_tick::run_handlers_with_panic_guard(handlers, &tick_ctx);
}

/// App exit teardown: persist the on-screen layout, then (Owned mode only) sync
/// fleet.yaml + kill every agent PTY. Extracted verbatim from the tail of
/// `run_app` (#14) — byte-identical.
///
/// `save_session` is UNGATED (#895): tab grouping / splits / ratios are
/// presentation-layer state the app owns even when Attached, so the next attach
/// can restore the custom layout. `sync_fleet_yaml` + agent-kill STAY gated to
/// Owned mode — in Attached mode the daemon owns fleet.yaml and the agent PTYs.
/// Process-global app-mode shutdown flag, cloned into every Owned-mode agent's
/// `SpawnConfig.shutdown` (see `pane_factory::attach_agent_to_pane`). app mode
/// is a singleton process, so one flag covers the whole fleet — this avoids
/// threading an `Arc<AtomicBool>` through the entire restore/pane-factory call
/// chain. `app_teardown` flips it true before killing agents so each agent's
/// PTY-close handler (`agent::handle_pty_close`) takes the fast `is_shutdown`
/// early-return (no per-thread 2 s exit-poll, no crash / shell-fallback events
/// during teardown). It is the app-mode equivalent of run_core's
/// "drain registry first" race guard. Sticky-true — process exits after.
static APP_SHUTDOWN: std::sync::OnceLock<Arc<std::sync::atomic::AtomicBool>> =
    std::sync::OnceLock::new();

pub(crate) fn app_shutdown_flag() -> &'static Arc<std::sync::atomic::AtomicBool> {
    APP_SHUTDOWN.get_or_init(|| Arc::new(std::sync::atomic::AtomicBool::new(false)))
}

/// #render-first phase-(b) F2: join attach workers, but DETACH any that haven't
/// finished by the shared `deadline` — a worker wedged mid-spawn (fork/exec /
/// skills / subscribe) must not hang quit (that would move the restore freeze to
/// quit). A detached worker's child, if it registered one, is reaped by the
/// registry drain + the OS (same stance as #2311's grace→SIGKILL). Returns the
/// number detached (wedged past the deadline).
fn bounded_join_attach_workers(
    handles: Vec<std::thread::JoinHandle<()>>,
    deadline: std::time::Instant,
) -> usize {
    let mut detached = 0usize;
    for h in handles {
        while !h.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if h.is_finished() {
            let _ = h.join();
        } else {
            detached += 1; // past the shared deadline → detach (drop the handle)
        }
    }
    detached
}

fn app_teardown(
    home: &Path,
    layout: &Layout,
    registry: &AgentRegistry,
    attached_mode: bool,
    attach_workers: Vec<std::thread::JoinHandle<()>>,
) {
    session::save_session(home, layout);
    if !attached_mode {
        // Sync fleet.yaml to match current state (Owned-only — daemon owns
        // fleet.yaml in Attached).
        session::sync_fleet_yaml(home, layout);

        // Cleanup: kill all agents (Owned-only — daemon owns PTYs in Attached).
        //
        // restart-freeze 真嫌#1 (t-…55279): this was a SEQUENTIAL per-agent
        // `kill_agent` loop, each blocking on `wait_for_child_exit` (≤5 s),
        // ~0.5 s × N ≈ ~6 s of the operator-visible restart freeze. Now:
        //  1. flip the shutdown flag so PTY-close handlers fast-return (no
        //     crash/shell-fallback events, no redundant per-thread exit poll) —
        //     the app-mode equivalent of run_core's drain-first race guard;
        //  2. drain the registry and kill ALL agents in parallel via the shared
        //     run_core core (`terminate_agents_parallel`: parallel SIGTERM →
        //     single grace → SIGKILL/reap holdouts), wall time ≈ one grace
        //     window regardless of N;
        //  3. run the per-agent cleanup tail (drop active-channel binding +
        //     remove IPC port + event log) — mirrors `delete_transaction`'s
        //     steps 5/7/8 (the registry remove is already done by the drain;
        //     app mode tracks no AgentConfig map, matching its `configs: None`).
        app_shutdown_flag().store(true, std::sync::atomic::Ordering::SeqCst);
        // #render-first phase-(b): join the background attach workers BEFORE
        // draining the registry. The shutdown flag (just set) makes each worker
        // early-abort un-started attaches (run_attach checks it on entry), so a
        // holdout is at most ONE in-flight spawn per worker. Joining first means
        // every child a worker DID register is in the registry → the parallel
        // terminate below reaps it.
        //
        // F2 (r4/r6): the join is BOUNDED — a worker wedged mid-spawn (fork/exec /
        // skills / subscribe) must not move the restore freeze to quit. Poll up to
        // a shared grace deadline (slightly longer than #2311's 2s SHUTDOWN_GRACE);
        // past it, DETACH the holdout (drop its handle). `is_finished` (Rust 1.61+,
        // MSRV 1.88) avoids a blocking `join()` on a wedged thread.
        //
        // Detaching is SAFE w.r.t. the one-shot drain below because we set the
        // shutdown flag FIRST (above): a detached worker that finishes its spawn
        // AFTER the drain sees the flag set and reaps its own child in
        // `pane_factory::finish_attach` — so a late registration never outlives
        // teardown (the r6 child-leak race). Children registered before the drain
        // are reaped by the drain itself.
        let attach_join_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let detached = bounded_join_attach_workers(attach_workers, attach_join_deadline);
        if detached > 0 {
            tracing::warn!(
                detached,
                "render-first: detached attach worker(s) still wedged past the join grace at quit"
            );
        }
        // #t-41673 gap-instrument: clock the parallel teardown so the next
        // operator restart EMPIRICALLY confirms the ~6s sequential freeze is
        // gone (expect `teardown_elapsed_ms` ≈ one grace window). Mirrors
        // shutdown_sequence's `shutdown_elapsed_ms` for the app-mode path, plus
        // per-agent `reap_ms` inside `terminate_agents_parallel`.
        let teardown_started = std::time::Instant::now();
        let agents: Vec<(String, crate::daemon::ChildHandle)> = {
            let mut reg = crate::agent::lock_registry(registry);
            reg.drain()
                .map(|(_id, handle)| (handle.name.to_string(), handle.child))
                .collect()
        };
        let agents_total = agents.len();
        let names: Vec<String> = agents.iter().map(|(n, _)| n.clone()).collect();
        crate::daemon::terminate_agents_parallel(agents);
        let run_dir = crate::daemon::run_dir(home);
        for name in &names {
            if let Some(ch) = crate::channel::active_channel() {
                let _ = ch.take_binding(name);
            }
            crate::ipc::remove_port(&run_dir, name);
            crate::event_log::log(home, "delete", name, "delete: app teardown (parallel)");
        }
        tracing::info!(
            agents_total,
            teardown_elapsed_ms = teardown_started.elapsed().as_millis() as u64,
            "app-mode parallel teardown complete"
        );
    }
}

/// Build menu items for new-tab selection.
/// Fleet instances already running in the registry are excluded.
fn build_menu_items(fleet_path: &Path, registry: &AgentRegistry) -> Vec<MenuItem> {
    let mut items = Vec::new();

    // Collect already-running agent names
    let running: Vec<String> = {
        let reg = agent::lock_registry(registry);
        reg.values().map(|h| h.name.to_string()).collect()
    };

    if let Ok(fleet) = crate::fleet::FleetConfig::load(fleet_path) {
        let mut names = fleet.instance_names();
        names.sort();
        for name in names {
            // Skip if exact name or deduped variant (name-1, name-2...) is running
            let already_open = running
                .iter()
                .any(|r| r == &name || r.starts_with(&format!("{name}-")));
            if already_open {
                continue;
            }
            let label = if let Some(resolved) = fleet.resolve_instance(&name) {
                format!("{name}  ({backend})", backend = resolved.backend_command)
            } else {
                name.clone()
            };
            items.push(MenuItem {
                label: format!("[fleet] {label}"),
                kind: MenuItemKind::FleetInstance(name),
            });
        }
    }

    for backend in Backend::all() {
        if backend.is_installed() {
            items.push(MenuItem {
                label: format!("[backend] {}", backend.name()),
                kind: MenuItemKind::Backend(backend.clone()),
            });
        }
    }

    items.push(MenuItem {
        label: "[shell] bash".to_string(),
        kind: MenuItemKind::Shell,
    });

    items
}

/// Create a pane from a menu item selection (shared by NewTab and Split handlers).
#[allow(clippy::too_many_arguments)]
fn pane_from_menu_item(
    item: MenuItem,
    fleet_path: &Path,
    layout: &mut Layout,
    registry: &AgentRegistry,
    home: &Path,
    cols: u16,
    rows: u16,
    wakeup_tx: &crossbeam_channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
) -> Result<Pane> {
    match item.kind {
        MenuItemKind::Shell => {
            let shell = crate::shell_command();
            pane_factory::create_pane(
                layout,
                registry,
                home,
                "shell",
                &shell,
                &[],
                crate::backend::SpawnMode::Fresh,
                None,
                &HashMap::new(),
                "\r",
                cols,
                rows,
                wakeup_tx,
                name_counter,
                pane_factory::SpawnIdentity::UnmanagedLocalShell,
            )
        }
        MenuItemKind::Backend(backend) => {
            let preset = backend.preset();
            let inst_name = pane_factory::unique_fleet_name(home, preset.command);
            // #966: TUI Backend menu (ctrl+b c) previously called
            // `add_instance_to_yaml` directly, bypassing the topic-creation
            // side effect that `handle_spawn` does. Now routes through
            // `tui_spawn::add_instance_with_topic` so the channel topic is
            // created + topic_id persisted to topics.json at TUI-spawn time.
            if let Err(e) = tui_spawn::add_instance_with_topic(
                home,
                &inst_name,
                &crate::fleet::InstanceYamlEntry {
                    backend: Some(backend.name().to_string()),
                    ..Default::default()
                },
            ) {
                tracing::warn!(error = %e, "failed to write fleet.yaml");
            }
            // Resolve from fleet to get defaults merged
            let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok();
            if let Some(resolved) = fleet.as_ref().and_then(|f| f.resolve_instance(&inst_name)) {
                pane_factory::create_pane_from_resolved(
                    &inst_name,
                    &resolved,
                    layout,
                    registry,
                    home,
                    cols,
                    rows,
                    wakeup_tx,
                    name_counter,
                    crate::backend::SpawnMode::Fresh,
                )
            } else {
                // Preset args are added by spawn_agent; no need to compose here.
                pane_factory::create_pane(
                    layout,
                    registry,
                    home,
                    &inst_name,
                    preset.command,
                    &[],
                    crate::backend::SpawnMode::Fresh,
                    None,
                    &HashMap::new(),
                    preset.submit_key,
                    cols,
                    rows,
                    wakeup_tx,
                    name_counter,
                    pane_factory::SpawnIdentity::Managed,
                )
            }
        }
        MenuItemKind::FleetInstance(inst_name) => {
            let fleet = crate::fleet::FleetConfig::load(fleet_path)?;
            let resolved = fleet
                .resolve_instance(&inst_name)
                .ok_or_else(|| anyhow::anyhow!("fleet instance '{inst_name}' not found"))?;
            pane_factory::create_pane_from_resolved(
                &inst_name,
                &resolved,
                layout,
                registry,
                home,
                cols,
                rows,
                wakeup_tx,
                name_counter,
                crate::backend::SpawnMode::Resume,
            )
        }
    }
}

/// #1762: does this forwarded keystroke ENTER the agent's input buffer
/// (text-composing), as opposed to NAVIGATE / CONTROL (arrows, F-keys, Esc,
/// Ctrl-combos, Tab, Backspace)? Only composing input should mark a draft
/// (#1457/#1675 draft-gating); navigation/control must NOT, or an idle operator
/// who merely browses history (Up/Down) or fat-fingers a non-text key traps every
/// actionable inject (task dispatch / ci-ready) behind the ~5min draft escape
/// window — exactly when away (the #1762 report).
///
/// Composing = at least one byte that enters the buffer: a non-space printable
/// (`> 0x20`, excluding DEL `0x7f`) or any UTF-8 continuation/lead byte
/// (`>= 0x80`). Deliberately NON-composing: ESC-prefixed sequences (arrows /
/// F-keys / Esc / Alt-combos — `key_to_bytes` encodes every nav key with a `0x1b`
/// lead), bare control bytes (Ctrl-combos, `Tab`=`\t`, `Backspace`=`0x7f`, and
/// `Enter`=`\r`/`\n` — Enter is the separately-detected SUBMIT signal), and lone
/// whitespace (the fat-fingered-space case — a real draft always carries a
/// non-space char that marks it, so #1675 protection is preserved). EXCEPTION:
/// bracketed paste (`ESC [ 200 ~`) wraps PASTED TEXT and IS composing.
fn is_text_composing_input(bytes: &[u8]) -> bool {
    if bytes.first() == Some(&0x1b) {
        return bytes.starts_with(b"\x1b[200~");
    }
    bytes.iter().any(|&b| (b > 0x20 && b != 0x7f) || b >= 0x80)
}

/// Write bytes to the focused pane's PTY (Local) or remote bridge (Remote).
fn write_to_focused(home: &Path, layout: &mut Layout, registry: &AgentRegistry, bytes: &[u8]) {
    if let Some(pane) = layout.active_tab_mut().and_then(|t| t.focused_pane_mut()) {
        // #1762: only text-composing input marks a draft — navigation / control
        // keys (and lone whitespace) must not defer actionable injects.
        if is_text_composing_input(bytes) {
            notification_queue::record_input_activity(home, &pane.agent_name);
        }
        // Sprint 54 P2-3: backend-aware submit detection (claude-first
        // allowlist). When the keystroke buffer contains the agent's
        // submit key (`\r` for claude, also matches paste-with-newlines
        // since the underlying CLI submits on any \r), record a
        // separate timestamp so the daemon supervisor can detect
        // "typed but not submitted" against this paired signal. Other
        // backends gracefully no-op — the supervisor tick reads
        // `last_submit_at_ms == 0` for them and skips emission per
        // the explicit backend allowlist there.
        if pane_input_contains_submit(pane.backend.as_ref(), bytes) {
            notification_queue::record_submit_activity(home, &pane.agent_name);
        }
        pane.write_input(registry, bytes);
    }
}

/// #783: write bytes to a SPECIFIC pane by id, bypassing focus. Used by
/// the mouse-forward path so the SGR report reaches the pane under the
/// cursor (e.g. opencode in a non-focused split) instead of the focused
/// pane. Shares the same submit-detection bookkeeping as
/// `write_to_focused` since the byte stream eventually lands at the
/// same `Pane::write_input` sink.
fn write_to_pane(
    home: &Path,
    layout: &mut Layout,
    registry: &AgentRegistry,
    pane_id: usize,
    bytes: &[u8],
) {
    if let Some(pane) = layout
        .active_tab_mut()
        .and_then(|t| t.root_mut().find_pane_mut(pane_id))
    {
        // #1762: only text-composing input marks a draft (see `write_to_focused`).
        if is_text_composing_input(bytes) {
            notification_queue::record_input_activity(home, &pane.agent_name);
        }
        if pane_input_contains_submit(pane.backend.as_ref(), bytes) {
            notification_queue::record_submit_activity(home, &pane.agent_name);
        }
        pane.write_input(registry, bytes);
    }
}

/// Sprint 54 P2-3: backend-aware submit detection. Returns true iff
/// the backend is on the submit-detection allowlist AND the keystroke
/// buffer contains its submit key. Hard-coded claude-only first round
/// per dispatch — extending to other backends just requires adding
/// arms to the match.
fn pane_input_contains_submit(backend: Option<&crate::backend::Backend>, bytes: &[u8]) -> bool {
    let Some(b) = backend else {
        return false;
    };
    // #1457: detect the submit key for ALL backends (was claude-only). Without
    // this, non-claude panes never record a submit timestamp, so `draft_state`
    // would see `submit=0` and treat every keystroke as a permanent unsent
    // draft → notifications would NEVER deliver to them (worse than the bug
    // this fixes). `submit_key` is `\r` for every preset; the empty-key guard
    // below no-ops backends (Shell/Raw) that declare no submit key.
    let submit = b.preset().submit_key.as_bytes();
    if submit.is_empty() || bytes.len() < submit.len() {
        return false;
    }
    bytes.windows(submit.len()).any(|w| w == submit)
}

fn sync_notification_state(home: &Path, layout: &mut Layout) {
    for tab in &mut layout.tabs {
        let pane_ids = tab.root().pane_ids();
        for pane_id in pane_ids {
            if let Some(pane) = tab.root_mut().find_pane_mut(pane_id) {
                let prev = pane.pending_notification_count;
                let now = notification_queue::pending_count(home, &pane.agent_name);
                // #1944 instrument: the pane-title `[N]` badge renders off this
                // count (core_render.rs). Log every CHANGE so the "badge
                // disappeared" report can be located at runtime — the code is
                // intact, so this catches whether the count actually reaches the
                // render with N>0 (or is reset to 0 before the next frame).
                if now != prev {
                    tracing::info!(
                        tag = "#1944-badge-state",
                        agent = %pane.agent_name,
                        prev,
                        now,
                        "pending-notification badge count changed"
                    );
                }
                pane.pending_notification_count = now;
            }
        }
    }
}

fn flush_idle_notifications(home: &Path, layout: &mut Layout) {
    for tab in &mut layout.tabs {
        let pane_ids = tab.root().pane_ids();
        for pane_id in pane_ids {
            let Some(pane) = tab.root_mut().find_pane_mut(pane_id) else {
                continue;
            };
            let agent_name = pane.agent_name.clone();
            flush_notifications_for_pane(home, pane, |text| {
                // #982 RC: queue contents come from compose_aware_*
                // which would have submit-injected on the immediate-
                // idle path. The flush must preserve that contract or
                // queued hints (e.g. `[AGEND-MSG-PENDING]`) land in
                // the prompt buffer without the backend submit_key —
                // codex one-shots silently drop the wake.
                crate::inbox::inject_notification_with_submit(home, &agent_name, text)
            });
        }
    }
}

/// #1944: bottom rows of the rendered screen scanned for the input box (prompt +
/// a few wrapped input rows). Mirrors the #1912 readback `READBACK_TAIL_ROWS`.
const DRAFT_INPUT_TAIL_ROWS: usize = 8;

/// Per-pane wrapper around the shared flush core
/// (`inbox::notify::flush_agent_queue_with_state` — busy/typing holds and
/// MAX_DEFER caps live there so the daemon's per-tick `notification_flush`
/// handler applies the IDENTICAL release policy in headless mode). The
/// TUI-only part kept here: the #1944/#1948 input-box probe that refines a
/// raw `Drafting` against the ACTUAL rendered input box (`pane.vterm` is
/// TUI-owned; the headless flush has no pane and conservatively honors the
/// raw draft state), plus the badge refresh.
fn flush_notifications_for_pane<F>(home: &Path, pane: &mut Pane, injector: F)
where
    F: FnMut(&str) -> anyhow::Result<()>,
{
    if pane.pending_notification_count == 0 {
        return;
    }
    // #1457: gate on draft state (input-vs-submit order), not the 3s idle window.
    // Drafting → defer everything; Abandoned → escape valve releases just the
    // oldest (trickle); None (clean buffer) → drain the whole backlog.
    //
    // #1944: refine `Drafting` with the input box's ACTUAL content. A
    // type-then-clear (typed then deleted to empty, or typed-but-not-submitted)
    // leaves `typed_ms > submit_ms` for up to 5 min while the box is visibly
    // EMPTY — the timestamp-only heuristic mis-read that as a live draft and held
    // messages until the next real submit. `pane.vterm` is the owned, live
    // rendered screen (no lock), so reading the input line here is cheap. When the
    // box is verifiably empty → deliver; a real draft (text in the box) OR an
    // undeterminable read (no marker / agent mid-output) both keep deferring
    // (fail toward draft-protection — never risk clobbering a real draft).
    let raw_state = notification_queue::draft_state(home, &pane.agent_name);
    let buffer_empty = if raw_state == notification_queue::DraftState::Drafting {
        pane.backend.as_ref().and_then(|b| {
            // #1948(b): codex's empty box shows DIM ghost text after `›`, which a
            // plain marker probe mis-reads as typed content — route it through the
            // DIM-aware check (needs the per-char dim mask). Everyone else uses the
            // text-only probe: marker (claude/agy) → placeholder (kiro) → fallback.
            // #t-97931 (F-A): route through the path-aware `Pane::tail_lines*` — off-
            // thread the main-thread `pane.vterm` is idle/blank, so reading it directly
            // mis-reads a real unsent draft as an empty box and the gate clobbers it.
            if let Some(marker) = b.input_dim_ghost_marker() {
                let (text, dim) = pane.tail_lines_with_dim(DRAFT_INPUT_TAIL_ROWS);
                notification_queue::input_box_dim_aware_empty(&text, &dim, marker)
            } else {
                notification_queue::input_box_empty_probe(
                    &pane.tail_lines(DRAFT_INPUT_TAIL_ROWS),
                    b.input_prompt_marker(),
                    b.input_empty_placeholder(),
                )
            }
        })
    } else {
        None
    };
    let effective_state = if buffer_empty == Some(true) {
        notification_queue::DraftState::None
    } else {
        raw_state
    };
    // #1944 instrument: the RCA had ZERO logs on this path. Surface every DEFER
    // (or buffer-override) decision so the next stranded-message report is
    // diagnosable. Clean immediate deliveries (None, no draft) are not logged.
    if effective_state != notification_queue::DraftState::None || buffer_empty.is_some() {
        let (typed_ms, submit_ms) =
            notification_queue::read_input_submit_timestamps(home, &pane.agent_name);
        tracing::info!(
            tag = "#1944-draftgate-decision",
            agent = %pane.agent_name,
            raw_state = ?raw_state,
            effective_state = ?effective_state,
            buffer_empty = ?buffer_empty,
            typed_ms,
            submit_ms,
            pending = pane.pending_notification_count,
            "draft-gate delivery decision"
        );
    }
    crate::inbox::notify::flush_agent_queue_with_state(
        home,
        &pane.agent_name,
        effective_state,
        injector,
    );
    pane.pending_notification_count = notification_queue::pending_count(home, &pane.agent_name);
}

/// Adjust scroll offset of the focused pane by `delta` lines (positive = up, negative = down).
fn scroll_focused(layout: &mut Layout, delta: i32) {
    if let Some(tab) = layout.active_tab_mut() {
        let fid = tab.focus_id;
        if let Some(pane) = tab.root_mut().find_pane_mut(fid) {
            // #offthread-scroll: off-thread mode leaves `pane.vterm` idle, so use the
            // path-aware max (snapshot history when off-thread, else live vterm).
            let max = pane.scroll_max();
            if delta > 0 {
                pane.scroll_offset = (pane.scroll_offset + delta as usize).min(max);
            } else {
                pane.scroll_offset = pane.scroll_offset.saturating_sub((-delta) as usize);
            }
        }
    }
}

/// Kill an agent and remove from registry. Delegates to
/// [`crate::daemon::lifecycle::delete_transaction`] so app-mode and
/// daemon-mode share one tear-down path.
///
/// Sprint 20 F3 fix: previously called only `child.kill()` (leader-only,
/// leaving subprocess trees alive on backends like kiro-cli) and skipped
/// event_log + Telegram binding rollback. The shared transaction now does
/// `kill_process_tree` + synchronous wait-for-exit + `take_binding` + event
/// log, matching the API delete path.
fn kill_agent(home: &Path, registry: &AgentRegistry, name: &str) {
    // #1915: app-mode teardown entry — mark "deleting" so a concurrent spawn
    // (e.g. a crash-respawn triggered by the kill below) cannot resurrect the
    // instance. Guard held to fn return; Drop un-marks on every path.
    let _delete_guard = crate::agent::deleting::mark_deleting(home, name);
    crate::daemon::lifecycle::delete_transaction(home, name, registry, None, false);
}

/// Whether the agent's child process is still running.
///
/// Used by the scratch shell overlay to self-close when the user exits the
/// shell naturally (`exit`, Ctrl+D) or the process crashes. Returns `false`
/// if the name is no longer registered (already reaped) or `try_wait`
/// reports the child has exited. `AgentHandle.child` is a `parking_lot::Mutex`
/// (which never poisons), so a CONTENDED lock is read via `try_lock()` and
/// treated as alive: this runs on the TUI main loop, and a blocking `.lock()`
/// would wedge the whole UI if another thread panicked while holding the child
/// lock (parking_lot leaves it locked). Transient contention just keeps the
/// overlay open for that tick — Esc still works.
fn agent_is_alive(registry: &AgentRegistry, name: &str) -> bool {
    let reg = agent::lock_registry(registry);
    // #1441: registry is UUID-keyed; the overlay only knows the display name,
    // so locate the handle by name (no fleet.yaml on the scratch-shell path).
    let Some(handle) = reg.values().find(|h| h.name.as_str() == name) else {
        return false;
    };
    // Bind to a local so the child-lock's temporary MutexGuard drops
    // before `reg` does — returning the match expression directly trips
    // the borrow checker because temporaries outlive the registry lock.
    let alive = match handle.child.try_lock() {
        Some(mut child) => !matches!(child.try_wait(), Ok(Some(_))),
        // Contended → cannot prove the child exited without blocking the main
        // loop; treat as alive and re-check next tick.
        None => true,
    };
    alive
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::PaneSource;
    use crate::vterm::VTerm;

    /// #render-first phase-(b) F2 (r4): `bounded_join_attach_workers` must DETACH a
    /// worker wedged past the deadline (so quit can't hang) while still joining the
    /// finished ones. Deterministic via a parked thread held by a channel.
    #[test]
    fn bounded_join_detaches_wedged_worker_without_hanging() {
        let (keep_tx, keep_rx) = std::sync::mpsc::channel::<()>();
        // Wedged: blocks until keep_tx drops (held past the join below).
        let wedged = std::thread::spawn(move || {
            let _ = keep_rx.recv();
        });
        let quick = std::thread::spawn(|| {}); // finishes immediately
        let start = std::time::Instant::now();
        let detached = bounded_join_attach_workers(
            vec![quick, wedged],
            start + std::time::Duration::from_millis(150),
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "bounded join must not hang on a wedged worker (took {:?})",
            start.elapsed()
        );
        assert_eq!(
            detached, 1,
            "the wedged worker is detached; the quick one is joined"
        );
        drop(keep_tx); // release the parked thread (cleanup)
    }

    /// restart-freeze 真嫌#1 (t-…55279) source-scan invariant: `app_teardown`'s
    /// Owned-mode cleanup must (1) flip the shutdown flag so PTY-close handlers
    /// fast-return, then (2) tear agents down through the shared parallel core
    /// `terminate_agents_parallel` — NOT the old SEQUENTIAL per-tab `kill_agent`
    /// loop (each blocking ≤5 s on `wait_for_child_exit`, ~6 s of the restart
    /// freeze). Regression-proof: revert app_teardown to a `kill_agent` loop and
    /// this fails.
    #[test]
    fn app_teardown_uses_parallel_core_not_sequential_kill_loop() {
        let src = include_str!("mod.rs");
        let start = src.find("fn app_teardown(").expect("app_teardown present");
        let after = &src[start..];
        let end = after.find("fn build_menu_items(").unwrap_or(after.len());
        let body = &after[..end];

        assert!(
            body.contains("app_shutdown_flag().store(true"),
            "app_teardown must flip the shutdown flag before killing agents \
             (fast PTY-close early-return, no crash events during teardown)"
        );
        assert!(
            body.contains("terminate_agents_parallel("),
            "app_teardown must route the kill through the shared parallel core"
        );
        assert!(
            !body.contains("kill_agent("),
            "#真嫌1: app_teardown must NOT use the sequential per-agent kill_agent \
             loop (that is the ~6s restart-freeze regression)"
        );
    }

    /// #1457 regression guard: submit detection must fire for ALL backends, not
    /// just claude. If this regresses to claude-only, non-claude panes never
    /// record a submit timestamp → `draft_state` sees `submit=0` → every
    /// keystroke looks like a permanent unsent draft → notifications NEVER
    /// deliver to them (strictly worse than the bug #1457 fixes).
    #[test]
    fn submit_detection_fires_for_all_backends() {
        use crate::backend::Backend;
        for b in [
            Backend::ClaudeCode,
            Backend::Codex,
            Backend::KiroCli,
            Backend::OpenCode,
            Backend::Agy,
        ] {
            assert!(
                pane_input_contains_submit(Some(&b), b"hello\r"),
                "submit key must be detected for {b:?}"
            );
            assert!(
                !pane_input_contains_submit(Some(&b), b"hello"),
                "no submit key in plain text for {b:?}"
            );
        }
        // No backend → never a submit (anonymous/unknown pane).
        assert!(!pane_input_contains_submit(None, b"hello\r"));
    }

    fn tmp_home(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agend-app-phase2-{}-{}",
            suffix,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// #1726 completeness invariant — the guard that closes the recurring
    /// #1002 / #982 / #1719 "app silently drops a handler" class. Compares two
    /// independently-built `name()` sets (the full daemon pipeline vs app's actual
    /// run set), so it has teeth and is not a tautology: a NEW handler added to
    /// `build_default_handlers` lands in `all`, and unless app runs it OR it is
    /// allowlisted, `missing` is non-empty → CI red.
    #[test]
    fn app_tick_handlers_cover_every_non_allowlisted_daemon_handler() {
        use std::collections::HashSet;
        let (crash_tx, _rx) = crossbeam_channel::bounded(1);
        let stale: crate::daemon::mcp_registry_watcher::DaemonBinaryStale =
            Arc::new(std::sync::atomic::AtomicBool::new(false));
        let all: HashSet<&str> = crate::daemon::build_default_handlers(crash_tx, true, stale)
            .iter()
            .map(|h| h.name())
            .collect();
        let app: HashSet<&str> =
            app_tick_handlers(Arc::new(std::sync::atomic::AtomicBool::new(false)))
                .iter()
                .map(|h| h.name())
                .collect();

        // Positive: every non-allowlisted daemon handler must run in app.
        // #2413 PR-B: `snapshot_rotation` is CONDITIONALLY run (default-ON, kill-switched by
        // `AGEND_APP_SNAPSHOT=0`), so it is exempt from the "must always run" check regardless
        // of the ambient env — its default-on + reversibility is pinned separately by
        // `snapshot_rotation_runs_in_app_mode_by_default_1720`.
        let missing: Vec<&str> = all
            .difference(&app)
            .filter(|n| !APP_TICK_ALLOWLIST.contains(n))
            .filter(|n| **n != "snapshot_rotation")
            .copied()
            .collect();
        assert!(
            missing.is_empty(),
            "app-standalone must run these per_tick handlers (or add to APP_TICK_ALLOWLIST \
             with a justification): {missing:?}"
        );
        // Negative probe: no stale allowlist entry — every allowlisted name must
        // still exist in the daemon set (catches a renamed/removed handler).
        for a in APP_TICK_ALLOWLIST {
            assert!(
                all.contains(a),
                "stale APP_TICK_ALLOWLIST entry '{a}' — handler renamed or removed?"
            );
            assert!(
                !app.contains(a),
                "allowlisted handler '{a}' must NOT run in app-standalone"
            );
        }
    }

    /// #1694(a): the #685 recovery ladder must RUN in app mode — the live daemon
    /// is app-standalone (`run_app`), never `run_core`, so allowlisting
    /// `recovery_dispatcher` out left the whole ladder silently dead in production
    /// (the #1720 / #1002 class). This pins it back IN the app run set.
    #[test]
    fn recovery_dispatcher_runs_in_app_mode_1694a() {
        let names: Vec<&str> =
            app_tick_handlers(Arc::new(std::sync::atomic::AtomicBool::new(false)))
                .iter()
                .map(|h| h.name())
                .collect();
        assert!(
            names.contains(&"recovery_dispatcher"),
            "recovery_dispatcher must RUN in app mode (#1694a) — got {names:?}"
        );
        assert!(
            !APP_TICK_ALLOWLIST.contains(&"recovery_dispatcher"),
            "recovery_dispatcher must NOT be allowlisted out of app mode (#1694a)"
        );
    }

    /// #2413 Phase B live-fix: the Shadow Observer reducer driver must RUN in app mode —
    /// the live fleet daemon is app-standalone (`run_app`), never `run_core`, so
    /// allowlisting `shadow_observe` out (as #2433 did) left `observed_status` null on
    /// every agent in production even with the flag on (the #1720/#685 class, same as
    /// `recovery_dispatcher` #1694a). This pins it IN the app run set so a future edit
    /// can't silently re-gate it to run_core-only. Goes through the REAL app-mode entry
    /// (`app_tick_handlers` = the set `run_app` actually executes), not a direct
    /// `handler.run()` — the unit-call seam is exactly what let DUAL/CI miss the gap.
    #[test]
    fn shadow_observe_runs_in_app_mode_2413() {
        let names: Vec<&str> =
            app_tick_handlers(Arc::new(std::sync::atomic::AtomicBool::new(false)))
                .iter()
                .map(|h| h.name())
                .collect();
        assert!(
            names.contains(&"shadow_observe"),
            "shadow_observe (the reducer driver) must RUN in app mode (#2413 live-fix) — \
             got {names:?}"
        );
        assert!(
            !APP_TICK_ALLOWLIST.contains(&"shadow_observe"),
            "shadow_observe must NOT be allowlisted out of app mode (#2413 live-fix)"
        );
    }

    /// #2413 PR-B (#1720-class fix): `snapshot_rotation` must RUN in app mode BY DEFAULT so
    /// the live daemon writes `snapshot.json` every tick (it was allowlisted out → weeks-stale
    /// → dispatch_idle/inbox/handoff/reply read stale state). Reversible: `AGEND_APP_SNAPSHOT=0`
    /// restores the allowlisted-out behaviour. Pins both the default-on and the kill-switch.
    #[test]
    #[serial_test::serial(app_snapshot_killswitch)]
    fn snapshot_rotation_runs_in_app_mode_by_default_1720() {
        struct EnvGuard(Option<String>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match &self.0 {
                    Some(v) => std::env::set_var("AGEND_APP_SNAPSHOT", v),
                    None => std::env::remove_var("AGEND_APP_SNAPSHOT"),
                }
            }
        }
        let _g = EnvGuard(std::env::var("AGEND_APP_SNAPSHOT").ok());
        let names = |stale| {
            app_tick_handlers(stale)
                .iter()
                .map(|h| h.name())
                .collect::<Vec<_>>()
        };

        // Default (unset) → snapshot_rotation RUNS in app mode.
        std::env::remove_var("AGEND_APP_SNAPSHOT");
        assert!(
            names(Arc::new(std::sync::atomic::AtomicBool::new(false)))
                .contains(&"snapshot_rotation"),
            "snapshot_rotation must RUN in app mode by default (#1720 live-fix)"
        );
        assert!(
            !APP_TICK_ALLOWLIST.contains(&"snapshot_rotation"),
            "snapshot_rotation must NOT be unconditionally allowlisted out (#1720 live-fix)"
        );

        // Kill-switch `=0` → reverts to the old allowlisted-out behaviour.
        std::env::set_var("AGEND_APP_SNAPSHOT", "0");
        assert!(
            !names(Arc::new(std::sync::atomic::AtomicBool::new(false)))
                .contains(&"snapshot_rotation"),
            "AGEND_APP_SNAPSHOT=0 must restore the allowlisted-out behaviour (no app snapshot write)"
        );
    }

    /// #2413 PR-B (#1720 live-fix) — END-TO-END verification on a /tmp home, driving the REAL
    /// app-mode handler set + real `snapshot.json` + real `dispatch_idle` gate. The
    /// operator-evidence proof of (a)(b)(c) with the `AGEND_APP_SNAPSHOT` on/off contrast:
    ///   (a) app mode WRITES `snapshot.json` with the PROMOTED operated state (it was
    ///       allowlisted out → weeks-stale → all snapshot readers saw stale state);
    ///   (b) at a REAL false-idle (raw screen Idle + a high-confidence Active
    ///       `observed_status`), `dispatch_idle` does NOT mis-fire — whereas a stale/raw
    ///       `idle` snapshot WOULD — and the shared busy-gate `agent_is_busy`
    ///       (inbox/handoff/reply) reads the agent as BUSY;
    ///   (c) `AGEND_APP_SNAPSHOT=0` reverts (app does not write) — the reversible escape hatch.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(shadow_observer)]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn pr_b_end_to_end_operated_state_app_mode_1720() {
        use crate::daemon::dispatch_idle;
        use crate::daemon::shadow::evidence::{Authority, Confidence};
        use crate::daemon::shadow::reducer::{ObservedState, ObservedStatus};
        use crate::snapshot::{agent_is_busy, agent_state_of, AgentSnapshot};

        struct G(&'static str, Option<String>);
        impl Drop for G {
            fn drop(&mut self) {
                match &self.1 {
                    Some(v) => std::env::set_var(self.0, v),
                    None => std::env::remove_var(self.0),
                }
            }
        }
        let _g = (
            G(
                "AGEND_SHADOW_OBSERVER",
                std::env::var("AGEND_SHADOW_OBSERVER").ok(),
            ),
            G(
                "AGEND_OBSERVED_DISPATCH",
                std::env::var("AGEND_OBSERVED_DISPATCH").ok(),
            ),
            G(
                "AGEND_APP_SNAPSHOT",
                std::env::var("AGEND_APP_SNAPSHOT").ok(),
            ),
        );
        std::env::set_var("AGEND_SHADOW_OBSERVER", "1");
        std::env::remove_var("AGEND_OBSERVED_DISPATCH"); // default-ON
        std::env::remove_var("AGEND_APP_SNAPSHOT"); // default-ON

        let home = std::env::temp_dir().join(format!("agend-pr-b-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();

        // A false-idle agent: raw screen Idle (mk_test_handle default) + a high-confidence
        // Active observed_status (the mid-API false-idle the reducer produces live).
        let id = crate::types::InstanceId::default();
        let handle = crate::agent::mk_test_handle("victim", id);
        handle.core.lock().observed_status = Some(ObservedStatus {
            state: ObservedState::Active,
            authority: Authority::Hook,
            confidence: Confidence::Strong,
            evidence: vec![],
            since_ms: 0,
        });
        let registry: crate::agent::AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::from([
                (id, handle),
            ])));
        let externals: crate::agent::ExternalRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let configs = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let stale = || Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Run ONLY the app-set's snapshot_rotation handler (proves it IS in the real app set
        // AND that running it writes snapshot.json). Returns whether the app set contained it.
        let run_app_snapshot = || {
            let ctx = crate::daemon::per_tick::TickContext {
                home: &home,
                registry: &registry,
                externals: &externals,
                configs: &configs,
            };
            let mut ran = false;
            for h in app_tick_handlers(stale()) {
                if h.name() == "snapshot_rotation" {
                    h.run(&ctx);
                    ran = true;
                }
            }
            ran
        };

        // ══ (a)+(b): ON (default) — app writes snapshot.json with the PROMOTED operated state ══
        assert!(
            run_app_snapshot(),
            "(a) app mode runs snapshot_rotation by default (un-allowlisted)"
        );
        assert_eq!(
            agent_state_of(&home, "victim").as_deref(),
            Some("active"),
            "(a)+(b) app mode wrote snapshot.json with the PROMOTED operated state (false-idle → active)"
        );
        assert!(
            agent_is_busy(&home, "victim"),
            "(b) the shared busy-gate (inbox/handoff/reply) reads the false-idle agent as BUSY"
        );

        // (b) dispatch_idle: drive the real scan_and_emit on a past-threshold pending dispatch.
        // Isolate the agent_state gate from the silence gate by holding silence > threshold, so
        // ONLY agent_state decides suppress-vs-fire.
        let save_snap = |state: &str| {
            crate::snapshot::save(
                &home,
                &[AgentSnapshot {
                    name: "victim".to_string(),
                    backend_command: "claude".to_string(),
                    args: vec![],
                    working_dir: None,
                    submit_key: "\r".to_string(),
                    health_state: "healthy".to_string(),
                    agent_state: state.to_string(),
                    silent_secs: 9_999,
                    output_silent_secs: 9_999,
                }],
            );
        };
        let make_overdue = |corr: &str| -> String {
            let did =
                dispatch_idle::record_dispatch(&home, "lead", "victim", Some(corr), "task", 60)
                    .expect("recorded pending dispatch");
            let p = dispatch_idle::pending_path(&home, &did);
            let mut v: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
            v["issued_at"] =
                serde_json::json!((chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339());
            v["status"] = serde_json::json!("pending");
            v["not_working_streak"] = serde_json::json!(0);
            crate::store::atomic_write(&p, serde_json::to_string(&v).unwrap().as_bytes()).unwrap();
            did
        };
        let status_of = |did: &str| -> String {
            dispatch_idle::list_pending(&home)
                .into_iter()
                .find(|d| d.dispatch_id == *did)
                .map(|d| format!("{:?}", d.status))
                .unwrap_or_else(|| "DELETED".into())
        };

        // Promoted "active" → SUPPRESSED across DEBOUNCE+margin scans (never mis-fires).
        save_snap("active");
        let did_ok = make_overdue("corr-suppress");
        for _ in 0..5 {
            dispatch_idle::scan_and_emit(&home);
        }
        assert_eq!(
            status_of(&did_ok),
            "Pending",
            "(b) dispatch_idle SUPPRESSED on the promoted false-idle — did NOT mis-fire"
        );

        // Contrast: a stale/raw "idle" snapshot (the pre-#1720-fix behaviour) → FIRES (Exceeded).
        save_snap("idle");
        let did_fire = make_overdue("corr-fire");
        for _ in 0..5 {
            dispatch_idle::scan_and_emit(&home);
        }
        assert_eq!(
            status_of(&did_fire),
            "Exceeded",
            "(b-baseline) a stale 'idle' snapshot MIS-FIRES (the bug PR-B fixes for the live daemon)"
        );

        // ══ (c): AGEND_APP_SNAPSHOT=0 reverts — app does NOT write the snapshot ══
        std::env::set_var("AGEND_APP_SNAPSHOT", "0");
        assert!(
            !run_app_snapshot(),
            "(c) AGEND_APP_SNAPSHOT=0 reverts: app mode does NOT run snapshot_rotation"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// #2413 Phase B live-fix companion: the reducer driver is useless without the
    /// hook-event SOCKET SERVER feeding its buffer, and `shadow::start` is the only thing
    /// that binds it. `run_core` already calls it; this source-pins that `run_app` does
    /// too, so the plane can't be half-wired in app mode again (driver runs, but folds an
    /// always-empty buffer). The behavioural end-to-end (flag-on → observed_status
    /// populated) is the operator's live dogfood; this is the cheap cross-platform guard.
    ///
    /// Scans ONLY the production region (before the `#[cfg(test)]` cutoff). This
    /// assertion's own needle literal lives in the test module below, so a WHOLE-FILE
    /// substring check would self-match and stay green even if the real `run_app` call
    /// were deleted — the #2433 vacuous-pin class this very PR fixes (DUAL round-1 caught
    /// it here). Mirrors `run_app_registers_event_bus_subscribers`. REVERSE-MUTATION
    /// verified: deleting the real `shadow::start(&home)` call from run_app turns this RED.
    #[test]
    fn run_app_wires_shadow_socket_server_2413() {
        let source = std::fs::read_to_string("src/app/mod.rs")
            .or_else(|_| std::fs::read_to_string("agend-terminal/src/app/mod.rs"))
            .expect("source file must be readable from test cwd");
        let prod = &source[..source.find("#[cfg(test)]").unwrap_or(source.len())];
        assert!(
            prod.contains("crate::daemon::shadow::start(&home)"),
            "run_app must start the Shadow Observer hook-socket server in the PRODUCTION \
             region (#2413 live-fix) — gating it to run_core left the whole plane dead in \
             the app-mode live daemon. No 'crate::daemon::shadow::start(&home)' before the \
             #[cfg(test)] cutoff"
        );
    }

    /// #2413 Phase D: the codex rollout-tail observer source (Stream plane) must be started
    /// in app mode too — same #1720/#685 reasoning as the hook socket above. The live fleet
    /// daemon is `run_app`, so gating `rollout::spawn` to run_core-only would leave codex
    /// agents' observer source dead in production (their `observed_status` would never get
    /// Stream evidence). Production-region scan only (the assert literal lives in the test
    /// module below — #2434's vacuous-pin lesson). Production-region scan, **comments
    /// stripped** (#2447 r6 helper) so a commented-out call does NOT satisfy the pin.
    /// REVERSE-MUTATION verified: both DELETING and COMMENTING-OUT the real
    /// `rollout::spawn(...)` call in run_app turn this RED.
    #[test]
    fn run_app_wires_codex_rollout_tailer_2413() {
        let source = std::fs::read_to_string("src/app/mod.rs")
            .or_else(|_| std::fs::read_to_string("agend-terminal/src/app/mod.rs"))
            .expect("source file must be readable from test cwd");
        let prod = &source[..source.find("#[cfg(test)]").unwrap_or(source.len())];
        // Strip comments FIRST: a commented-out call must not pass the pin (#2447 r6 vacuity fix).
        let prod_code = strip_comments_and_blank_strings(prod);
        assert!(
            prod_code.contains("crate::daemon::shadow::rollout::spawn("),
            "run_app must spawn the codex rollout-tail observer in the PRODUCTION region \
             (#2413 Phase D) — gating it run_core-only (or commenting it out) would leave codex \
             agents' Stream observer dead in the app-mode live daemon. No ACTIVE \
             'crate::daemon::shadow::rollout::spawn(' (comments + string-literal contents masked) before the \
             #[cfg(test)] cutoff"
        );
    }

    /// #2413 opencode plane: the opencode SSE `/event` observer source (Stream plane) must
    /// be started in app mode too — SAME #2434 reasoning as the rollout tailer above. The
    /// live fleet daemon is `run_app`, so gating `opencode::spawn` to run_core-only would
    /// leave opencode agents' observer source dead in production. Production-region scan,
    /// **comments stripped** (#2447 r6 helper) so a commented-out call does NOT satisfy the
    /// pin. REVERSE-MUTATION verified: both DELETING and COMMENTING-OUT the real
    /// `opencode::spawn(...)` call in run_app turn this RED.
    #[test]
    fn run_app_wires_opencode_sse_observer_2413() {
        let source = std::fs::read_to_string("src/app/mod.rs")
            .or_else(|_| std::fs::read_to_string("agend-terminal/src/app/mod.rs"))
            .expect("source file must be readable from test cwd");
        let prod = &source[..source.find("#[cfg(test)]").unwrap_or(source.len())];
        // Strip comments FIRST: a commented-out call must not pass the pin (#2447 r6 vacuity fix).
        let prod_code = strip_comments_and_blank_strings(prod);
        assert!(
            prod_code.contains("crate::daemon::shadow::opencode::spawn("),
            "run_app must spawn the opencode SSE observer in the PRODUCTION region \
             (#2413 opencode plane) — gating it run_core-only (or commenting it out) would leave \
             opencode agents' Stream observer dead in the app-mode live daemon. No ACTIVE \
             'crate::daemon::shadow::opencode::spawn(' (comments + string-literal contents masked) before the \
             #[cfg(test)] cutoff"
        );
    }

    /// Strip `//` line + `/* */` block comments from Rust source so a wiring-pin
    /// contains-check can't be satisfied by a COMMENTED-OUT call (#2447 r6: the raw
    /// contains() was vacuous — commenting the production call left the text present and the
    /// pin still passed, the exact #2434 dead-wiring class it must catch).
    ///
    /// **String-literal-aware** (#2447 r6 round-2): a `//` or `/*` INSIDE a string / char /
    /// raw-string literal is NOT a comment and is preserved verbatim — so e.g. a
    /// `"http://x"` URL or a `"/* x */"` payload on a line cannot make the stripper eat the
    /// rest of that line (a false-RED for the pin). Handles `"..."` (with `\` escapes),
    /// `'...'` char literals (escape + simple; lifetimes like `'a` are left as-is), and raw
    /// strings `r"..."` / `r#"..."#` / `br"..."` (hash-count matched, no escapes). Unicode-
    /// correct. Shared so the codex/opencode app-pins can adopt it (follow-up).
    fn strip_rust_comments(src: &str) -> String {
        let s: Vec<char> = src.chars().collect();
        let n = s.len();
        let mut out = String::with_capacity(src.len());
        let mut i = 0;
        while i < n {
            let c = s[i];
            // line comment → skip to (but keep) the newline.
            if c == '/' && i + 1 < n && s[i + 1] == '/' {
                i += 2;
                while i < n && s[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            // block comment → skip to `*/`.
            if c == '/' && i + 1 < n && s[i + 1] == '*' {
                i += 2;
                while i + 1 < n && !(s[i] == '*' && s[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(n);
                continue;
            }
            // raw string `r"..."` / `r#"..."#` / `br"..."` — copy verbatim (no escapes;
            // close on `"` + the same number of `#`).
            if c == 'r' || (c == 'b' && i + 1 < n && s[i + 1] == 'r') {
                let r_pos = if c == 'b' { i + 1 } else { i };
                let mut k = r_pos + 1;
                let mut hashes = 0;
                while k < n && s[k] == '#' {
                    hashes += 1;
                    k += 1;
                }
                if k < n && s[k] == '"' {
                    for ch in &s[i..=k] {
                        out.push(*ch);
                    }
                    i = k + 1;
                    loop {
                        if i >= n {
                            break;
                        }
                        if s[i] == '"' {
                            let mut h = 0;
                            while i + 1 + h < n && h < hashes && s[i + 1 + h] == '#' {
                                h += 1;
                            }
                            if h == hashes {
                                for ch in &s[i..i + 1 + hashes] {
                                    out.push(*ch);
                                }
                                i += 1 + hashes;
                                break;
                            }
                        }
                        out.push(s[i]);
                        i += 1;
                    }
                    continue;
                }
                // not a raw string (plain identifier starting with r/b) → fall through.
            }
            // normal / byte string `"..."` — copy verbatim, honoring `\` escapes.
            if c == '"' {
                out.push(c);
                i += 1;
                while i < n {
                    if s[i] == '\\' && i + 1 < n {
                        out.push(s[i]);
                        out.push(s[i + 1]);
                        i += 2;
                        continue;
                    }
                    out.push(s[i]);
                    let closing = s[i] == '"';
                    i += 1;
                    if closing {
                        break;
                    }
                }
                continue;
            }
            // char literal `'x'` / `'\n'` (vs a lifetime `'a`, left as a normal char).
            if c == '\'' {
                if i + 1 < n && s[i + 1] == '\\' {
                    // escape char literal: `'`, `\`, escaped-char, …, `'`.
                    out.push(s[i]);
                    out.push(s[i + 1]);
                    i += 2;
                    if i < n {
                        out.push(s[i]); // the escaped char (covers `'\''`)
                        i += 1;
                    }
                    while i < n && s[i] != '\'' {
                        out.push(s[i]);
                        i += 1;
                    }
                    if i < n {
                        out.push(s[i]); // closing `'`
                        i += 1;
                    }
                    continue;
                }
                if i + 2 < n && s[i + 2] == '\'' {
                    // simple char literal `'x'` (x may be `"` / `/`, must not start a string).
                    out.push(s[i]);
                    out.push(s[i + 1]);
                    out.push(s[i + 2]);
                    i += 3;
                    continue;
                }
                // lifetime / stray `'` → normal char.
            }
            out.push(c);
            i += 1;
        }
        out
    }

    /// Replace the CONTENTS of every string / raw-string literal with spaces (delimiters
    /// kept) in ALREADY-comment-free Rust source; char literals are passed through verbatim
    /// (they can't hold a multi-char needle, but must be consumed so a `"` inside `'"'`
    /// doesn't mis-start a string scan). Sibling of [`strip_rust_comments`] used only by the
    /// wiring pins via [`strip_comments_and_blank_strings`].
    ///
    /// #2450 reviewer-6: comment-stripping alone left the pins **string-literal-blind** — a
    /// needle hidden in a string (`let _ = "…::spawn(";`) survived the strip, so the pin's
    /// `contains()` still falsely passed (the exact #2434 dead-wiring vacuity, just relocated
    /// from a comment into a string). Blanking string interiors closes that: the only place
    /// the needle survives is an ACTIVE code call.
    fn blank_string_contents(src: &str) -> String {
        let s: Vec<char> = src.chars().collect();
        let n = s.len();
        let mut out = String::with_capacity(src.len());
        let mut i = 0;
        while i < n {
            let c = s[i];
            // raw string `r"..."` / `r#"..."#` / `br"..."` — blank interior, keep delimiters.
            if c == 'r' || (c == 'b' && i + 1 < n && s[i + 1] == 'r') {
                let r_pos = if c == 'b' { i + 1 } else { i };
                let mut k = r_pos + 1;
                let mut hashes = 0;
                while k < n && s[k] == '#' {
                    hashes += 1;
                    k += 1;
                }
                if k < n && s[k] == '"' {
                    for ch in &s[i..=k] {
                        out.push(*ch); // opening `r#"` delimiter, verbatim
                    }
                    i = k + 1;
                    loop {
                        if i >= n {
                            break;
                        }
                        if s[i] == '"' {
                            let mut h = 0;
                            while i + 1 + h < n && h < hashes && s[i + 1 + h] == '#' {
                                h += 1;
                            }
                            if h == hashes {
                                for ch in &s[i..i + 1 + hashes] {
                                    out.push(*ch); // closing `"#`, verbatim
                                }
                                i += 1 + hashes;
                                break;
                            }
                        }
                        out.push(' '); // blank one interior char
                        i += 1;
                    }
                    continue;
                }
                // not a raw string (identifier starting with r/b) → fall through.
            }
            // normal / byte string `"..."` — blank interior (honor `\` escapes), keep quotes.
            if c == '"' {
                out.push('"');
                i += 1;
                while i < n {
                    if s[i] == '\\' && i + 1 < n {
                        out.push(' ');
                        out.push(' ');
                        i += 2;
                        continue;
                    }
                    if s[i] == '"' {
                        out.push('"');
                        i += 1;
                        break;
                    }
                    out.push(' ');
                    i += 1;
                }
                continue;
            }
            // char literal `'x'` / `'\n'` — pass through verbatim (consume so a `"` inside a
            // char literal can't start a string scan); a lifetime `'a` is a normal char.
            if c == '\'' {
                if i + 1 < n && s[i + 1] == '\\' {
                    out.push(s[i]);
                    out.push(s[i + 1]);
                    i += 2;
                    if i < n {
                        out.push(s[i]);
                        i += 1;
                    }
                    while i < n && s[i] != '\'' {
                        out.push(s[i]);
                        i += 1;
                    }
                    if i < n {
                        out.push(s[i]);
                        i += 1;
                    }
                    continue;
                }
                if i + 2 < n && s[i + 2] == '\'' {
                    out.push(s[i]);
                    out.push(s[i + 1]);
                    out.push(s[i + 2]);
                    i += 3;
                    continue;
                }
                // lifetime / stray `'` → normal char.
            }
            out.push(c);
            i += 1;
        }
        out
    }

    /// The wiring-pin matcher: strip comments AND blank string-literal contents, so a
    /// `…::spawn(` needle counts ONLY when it is an ACTIVE code call — not commented out
    /// (#2447) and not hidden in a string literal (#2450 reviewer-6). Composition of the two
    /// proven passes; `strip_rust_comments` body is left untouched.
    fn strip_comments_and_blank_strings(src: &str) -> String {
        blank_string_contents(&strip_rust_comments(src))
    }

    /// #2450 reviewer-6 regression: the wiring-pin matcher must treat a `…::spawn(` needle as
    /// PRESENT only when it is ACTIVE code — not in a comment (#2447) and not hidden inside a
    /// string / raw-string literal (#2450). Pins reviewer-6's exact false-green break-probe.
    #[test]
    fn strip_comments_and_blank_strings_masks_string_and_comment_needles() {
        let needle = "crate::daemon::shadow::rollout::spawn(";
        // ACTIVE code call → still present (must NOT false-kill the real wiring).
        assert!(strip_comments_and_blank_strings(&format!("    {needle}x);")).contains(needle));
        // reviewer-6 break-probe: needle hidden in a NORMAL string literal → masked.
        assert!(
            !strip_comments_and_blank_strings(&format!("let _p = \"{needle}\";")).contains(needle)
        );
        // needle hidden in a RAW string literal → masked.
        assert!(
            !strip_comments_and_blank_strings(&format!("let _p = r#\"{needle}\"#;"))
                .contains(needle)
        );
        // needle in a comment → masked (the comment-strip pass still applies).
        assert!(!strip_comments_and_blank_strings(&format!("// {needle}\n")).contains(needle));
        // string-literal-AWARE preserved: a `"http://x"` URL must not eat the rest of its
        // line, so real code after it survives (no false-RED).
        assert!(
            strip_comments_and_blank_strings("let u = \"http://x\"; keep_me();")
                .contains("keep_me()")
        );
    }

    /// #2447 r6 round-2: `strip_rust_comments` must strip REAL comments yet preserve
    /// `//` / `/*` that live inside string / char / raw-string literals (else a `"http://x"`
    /// URL would false-strip its line → a vacuity-fix that introduces a false-RED). Covers
    /// r6's requested cases.
    #[test]
    fn strip_rust_comments_is_string_literal_aware() {
        // Real comments ARE stripped.
        assert!(!strip_rust_comments("keep // dropme\n").contains("dropme"));
        let blk = strip_rust_comments("alpha /* dropme */ omega");
        assert!(!blk.contains("dropme") && blk.contains("alpha") && blk.contains("omega"));
        // `//` inside a normal string is NOT a comment.
        assert!(strip_rust_comments(r#"let u = "http://example.com/a";"#)
            .contains("http://example.com/a"));
        // `/* */` inside a string is preserved.
        assert!(strip_rust_comments(r#"let s = "/* not a comment */";"#)
            .contains("/* not a comment */"));
        // raw string content (incl. `//`) is preserved.
        assert!(strip_rust_comments(r##"let r = r#"// not comment"#;"##).contains("// not comment"));
        // a quote/slash inside a CHAR literal must not start a string / a comment.
        assert!(strip_rust_comments(r#"let c = '"'; live_after_quote();"#)
            .contains("live_after_quote()"));
        assert!(
            strip_rust_comments("let c = '/'; live_after_slash();").contains("live_after_slash()")
        );
        // escaped-quote char literal `'\''` must not desync the scanner.
        assert!(
            strip_rust_comments(r"let c = '\''; live_after_esc();").contains("live_after_esc()")
        );
        // a real trailing line comment AFTER an in-string `//` is still stripped.
        let mixed = strip_rust_comments("let u = \"a//b\"; keep(); // dropme\n");
        assert!(mixed.contains("a//b") && mixed.contains("keep()") && !mixed.contains("dropme"));
        // the pin's own case: active call survives; commented-out does not.
        assert!(
            strip_rust_comments("    crate::daemon::shadow::kiro::spawn(x);")
                .contains("crate::daemon::shadow::kiro::spawn(")
        );
        assert!(
            !strip_rust_comments("    // crate::daemon::shadow::kiro::spawn(x);")
                .contains("crate::daemon::shadow::kiro::spawn(")
        );
    }

    /// #2413 kiro plane: the kiro session-tail observer source (Stream plane) must be
    /// started in app mode too — SAME #2434 reasoning as rollout/opencode above. The live
    /// fleet daemon is `run_app`, so gating `kiro::spawn` to run_core-only would leave kiro
    /// agents' observer source dead in production. Production-region scan, **comments
    /// stripped** so a commented-out call does NOT satisfy the pin. REVERSE-MUTATION
    /// verified (#2447 r6): both DELETING and COMMENTING-OUT the real `kiro::spawn(...)`
    /// call in run_app turn this RED.
    #[test]
    fn run_app_wires_kiro_session_tailer_2413() {
        let source = std::fs::read_to_string("src/app/mod.rs")
            .or_else(|_| std::fs::read_to_string("agend-terminal/src/app/mod.rs"))
            .expect("source file must be readable from test cwd");
        let prod = &source[..source.find("#[cfg(test)]").unwrap_or(source.len())];
        // Strip comments FIRST: a commented-out call must not pass the pin (r6 vacuity fix).
        let prod_code = strip_comments_and_blank_strings(prod);
        assert!(
            prod_code.contains("crate::daemon::shadow::kiro::spawn("),
            "run_app must spawn the kiro session-tail observer in the PRODUCTION region \
             (#2413 kiro plane) — gating it run_core-only (or commenting it out) would leave \
             kiro agents' Stream observer dead in the app-mode live daemon. No ACTIVE \
             'crate::daemon::shadow::kiro::spawn(' (comments + string-literal contents masked) before the \
             #[cfg(test)] cutoff"
        );
    }

    #[test]
    fn run_app_wires_agy_session_tailer_2413() {
        let source = std::fs::read_to_string("src/app/mod.rs")
            .or_else(|_| std::fs::read_to_string("agend-terminal/src/app/mod.rs"))
            .expect("source file must be readable from test cwd");
        let prod = &source[..source.find("#[cfg(test)]").unwrap_or(source.len())];
        let prod_code = strip_comments_and_blank_strings(prod);
        assert!(
            prod_code.contains("crate::daemon::shadow::agy::spawn("),
            "run_app must spawn the agy session-tail observer in the PRODUCTION region"
        );
    }

    /// #1726 must-verify: app-standalone runs these handlers with EMPTY
    /// externals/configs (it has no external-agent / AgentConfig registry) and a
    /// possibly-empty registry. None may panic — `run_handlers` has catch_unwind,
    /// but we want a clean no-op degrade, so we call each `run()` directly (no
    /// catch) and a panic fails the test.
    #[test]
    fn app_tick_handlers_no_panic_on_empty_context() {
        let home = tmp_home("tick-empty-ctx");
        let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let externals: crate::agent::ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs: crate::api::ConfigRegistry = Arc::new(Mutex::new(HashMap::new()));
        let ctx = crate::daemon::per_tick::TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        for h in app_tick_handlers(Arc::new(std::sync::atomic::AtomicBool::new(false))) {
            h.run(&ctx); // panic here = test failure
        }
        std::fs::remove_dir_all(&home).ok();
    }

    fn pane(name: &str) -> Pane {
        Pane {
            agent_name: name.into(),
            instance_id: crate::types::InstanceId::default(),
            vterm: VTerm::new(10, 10),
            rx: crossbeam_channel::bounded(1).1,
            id: 1,
            backend: None,
            working_dir: None,
            display_name: None,
            scroll_offset: 0,
            has_notification: false,
            fleet_instance_name: None,
            last_input_at: None,
            pending_notification_count: 0,
            selection: None,
            source: PaneSource::Local,
            offthread: None,
            _fwd_cancel: None,
        }
    }

    /// #982 RC wiring-pin: assert `flush_idle_notifications` invokes
    /// the submit-aware injector (`inject_notification_with_submit`)
    /// so queued hints get the backend `submit_key` applied on flush.
    ///
    /// Implemented as a file-level source pin — the raw
    /// `inject_notification` was deleted in this PR, so the negative
    /// half of the invariant is compile-time enforced. The positive
    /// half (this assertion) is platform-agnostic and survives
    /// rustfmt re-wrapping. Companion test:
    /// `inbox::tests::t15_composing_flush_uses_submit_aware_inject`
    /// pins the JSON payload contract end-to-end.
    #[test]
    fn flush_idle_notifications_wired_to_submit_aware_inject() {
        let source = std::fs::read_to_string("src/app/mod.rs")
            .or_else(|_| std::fs::read_to_string("agend-terminal/src/app/mod.rs"))
            .expect("source file must be readable from test cwd");
        // Search only the production region. This assertion's own literal
        // lives in the #[cfg(test)] module below, so a whole-file substring
        // check self-matches and would stay green even if the real call were
        // deleted. Require the call form (with `(`) before the test cutoff.
        let prod = &source[..source.find("#[cfg(test)]").unwrap_or(source.len())];
        assert!(
            prod.contains("inject_notification_with_submit("),
            "flush_idle_notifications must wire the submit-aware injector \
             (#982 reviewer #999 verdict) — no call to \
             'inject_notification_with_submit(' found in the production region \
             of src/app/mod.rs"
        );
    }

    // ── #1944: buffer-aware draft gate (the operator-facing fix) ──

    /// Build a pane whose live `vterm` renders `screen` with `backend`. The term
    /// is `VTerm::new(cols, rows)` — wide enough that input lines don't wrap, and
    /// few enough rows that the content stays within `DRAFT_INPUT_TAIL_ROWS`.
    fn pane_with_screen(name: &str, backend: Option<Backend>, screen: &str) -> Pane {
        let mut p = pane(name);
        p.backend = backend;
        p.vterm = crate::vterm::VTerm::new(80, 6);
        p.vterm.process(screen.as_bytes());
        p
    }

    /// Set up a recent unsent draft (typed_ms > submit_ms → `Drafting`) and one
    /// queued notification for `agent` under `home`.
    fn seed_drafting_with_queued(home: &Path, agent: &str) {
        let now = chrono::Utc::now().timestamp_millis();
        crate::agent_ops::save_metadata(
            home,
            agent,
            "last_input_epoch_ms",
            serde_json::json!(now - 30_000),
        );
        crate::agent_ops::save_metadata(
            home,
            agent,
            "last_submit_epoch_ms",
            serde_json::json!(now - 60_000),
        );
        notification_queue::enqueue(home, agent, "[AGEND-MSG-PENDING] peer report")
            .expect("enqueue test notification");
    }

    /// #1944 §3.9: a stale type-then-clear draft (typed_ms > submit_ms but the
    /// input box is EMPTY) must DELIVER — the old timestamp-only gate held it.
    #[test]
    fn draft_gate_delivers_when_input_box_empty() {
        let home = tmp_home("draftgate-empty");
        seed_drafting_with_queued(&home, "lead");
        // claude pane, input box empty (`❯ ` with nothing typed).
        let mut p = pane_with_screen("lead", Some(Backend::ClaudeCode), "❯ ");
        p.pending_notification_count = notification_queue::pending_count(&home, "lead");

        let mut injected: Vec<String> = Vec::new();
        flush_notifications_for_pane(&home, &mut p, |t| {
            injected.push(t.to_string());
            Ok(())
        });
        assert_eq!(
            injected.len(),
            1,
            "empty input box → the stale-draft message must be delivered, not held"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1944 §3.9: a REAL live draft (text in the input box) must still DEFER —
    /// the draft-protection invariant is unchanged.
    #[test]
    fn draft_gate_defers_when_input_box_has_text() {
        let home = tmp_home("draftgate-typed");
        seed_drafting_with_queued(&home, "lead");
        let mut p = pane_with_screen("lead", Some(Backend::ClaudeCode), "❯ half-typed reply");
        p.pending_notification_count = notification_queue::pending_count(&home, "lead");

        let mut injected: Vec<String> = Vec::new();
        flush_notifications_for_pane(&home, &mut p, |t| {
            injected.push(t.to_string());
            Ok(())
        });
        assert!(
            injected.is_empty(),
            "a real draft (text in the box) must keep deferring (protection unchanged)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1944 §3.9: a backend with no prompt marker (Shell) → buffer-emptiness is
    /// undeterminable → fall back to the timestamp behavior (defer), fail toward
    /// draft-protection. Same outcome for a claude pane mid-output (no prompt in
    /// the tail) — covered by `input_box_none_when_marker_absent`.
    #[test]
    fn draft_gate_falls_back_to_timestamp_for_markerless_backend() {
        let home = tmp_home("draftgate-shell");
        seed_drafting_with_queued(&home, "lead");
        // Shell has no input_prompt_marker → None → keep the raw Drafting defer.
        let mut p = pane_with_screen("lead", Some(Backend::Shell), "$ ");
        p.pending_notification_count = notification_queue::pending_count(&home, "lead");

        let mut injected: Vec<String> = Vec::new();
        flush_notifications_for_pane(&home, &mut p, |t| {
            injected.push(t.to_string());
            Ok(())
        });
        assert!(
            injected.is_empty(),
            "markerless backend → fail toward protection (timestamp-only defer)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1948 v2 §3.9: kiro has no prompt marker but its empty box shows a
    /// placeholder — a cleared kiro pane (placeholder visible) must DELIVER.
    #[test]
    fn draft_gate_delivers_for_kiro_when_placeholder_visible() {
        let home = tmp_home("draftgate-kiro-empty");
        seed_drafting_with_queued(&home, "lead");
        // cleared kiro box: the real placeholder is visible (no typed content).
        let mut p = pane_with_screen(
            "lead",
            Some(Backend::KiroCli),
            "Kiro auto\n\n ask a question or describe a task ↵\n /copy",
        );
        p.pending_notification_count = notification_queue::pending_count(&home, "lead");

        let mut injected: Vec<String> = Vec::new();
        flush_notifications_for_pane(&home, &mut p, |t| {
            injected.push(t.to_string());
            Ok(())
        });
        assert_eq!(
            injected.len(),
            1,
            "kiro cleared (placeholder visible) → stale draft delivered, not held"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1948 v2 §3.9: a kiro pane with a real draft (placeholder replaced by typed
    /// text) must still DEFER — protection unchanged.
    #[test]
    fn draft_gate_defers_for_kiro_when_typed() {
        let home = tmp_home("draftgate-kiro-typed");
        seed_drafting_with_queued(&home, "lead");
        let mut p = pane_with_screen("lead", Some(Backend::KiroCli), "Kiro auto\n\n half typed\n");
        p.pending_notification_count = notification_queue::pending_count(&home, "lead");

        let mut injected: Vec<String> = Vec::new();
        flush_notifications_for_pane(&home, &mut p, |t| {
            injected.push(t.to_string());
            Ok(())
        });
        assert!(
            injected.is_empty(),
            "kiro with text (placeholder gone) → keep deferring (protection unchanged)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1948(b) §3.9: codex's empty box shows DIM ghost text after `›` (SGR 2) —
    /// the dim-aware path must DELIVER (the v1 plain-marker path mis-read the ghost
    /// as typed content and held). The vterm processes the real SGR so the dim
    /// flag is set exactly as codex emits it.
    #[test]
    fn draft_gate_delivers_for_codex_when_ghost_is_dim() {
        let home = tmp_home("draftgate-codex-ghost");
        seed_drafting_with_queued(&home, "lead");
        // `ESC[1m›` (bold prompt) + `ESC[2m…` (dim ghost) — codex's real encoding.
        let screen = "\u{1b}[1m›\u{1b}[22m\u{1b}[2m Use /skills to list available skills\u{1b}[0m";
        let mut p = pane_with_screen("lead", Some(Backend::Codex), screen);
        p.pending_notification_count = notification_queue::pending_count(&home, "lead");

        let mut injected: Vec<String> = Vec::new();
        flush_notifications_for_pane(&home, &mut p, |t| {
            injected.push(t.to_string());
            Ok(())
        });
        assert_eq!(
            injected.len(),
            1,
            "codex empty box (dim ghost after ›) → stale draft delivered, not held"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1948(b) §3.9: a real codex draft (normal-intensity text after `›`) must
    /// still DEFER — the dim signal must not false-deliver on a real draft.
    #[test]
    fn draft_gate_defers_for_codex_when_input_normal_intensity() {
        let home = tmp_home("draftgate-codex-typed");
        seed_drafting_with_queued(&home, "lead");
        // `ESC[1m›` then NORMAL intensity input (no SGR 2).
        let screen = "\u{1b}[1m›\u{1b}[22m my actual draft reply\u{1b}[0m";
        let mut p = pane_with_screen("lead", Some(Backend::Codex), screen);
        p.pending_notification_count = notification_queue::pending_count(&home, "lead");

        let mut injected: Vec<String> = Vec::new();
        flush_notifications_for_pane(&home, &mut p, |t| {
            injected.push(t.to_string());
            Ok(())
        });
        assert!(
            injected.is_empty(),
            "codex with normal-intensity input → keep deferring (protection unchanged)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn input_prompt_marker_only_for_verified_backends() {
        assert_eq!(Backend::ClaudeCode.input_prompt_marker(), Some("❯"));
        assert_eq!(Backend::Agy.input_prompt_marker(), Some(">"));
        // #1948 codex follow-up: codex is NOT marker-covered — its empty box shows
        // a rotating ghost phrase after `›`, which the PLAIN marker probe mis-reads
        // as typed content. #1948(b): codex is instead covered via the DIM-aware
        // path (`input_dim_ghost_marker`) — the ghost is dim, real input is normal.
        assert_eq!(Backend::Codex.input_prompt_marker(), None);
        assert_eq!(Backend::Codex.input_empty_placeholder(), None);
        assert_eq!(Backend::Codex.input_dim_ghost_marker(), Some("›"));
        assert_eq!(Backend::Shell.input_prompt_marker(), None);
        assert_eq!(Backend::OpenCode.input_prompt_marker(), None);
        // #1948 v2: kiro covered via placeholder, NOT a marker; opencode stays
        // fully fallback (no marker, no placeholder).
        assert_eq!(Backend::KiroCli.input_prompt_marker(), None);
        assert_eq!(
            Backend::KiroCli.input_empty_placeholder(),
            Some("ask a question or describe a task")
        );
        assert_eq!(Backend::OpenCode.input_empty_placeholder(), None);
        assert_eq!(Backend::ClaudeCode.input_empty_placeholder(), None);
        // dim-ghost is codex-only: the marker-backends and kiro are NOT dim-aware.
        assert_eq!(Backend::ClaudeCode.input_dim_ghost_marker(), None);
        assert_eq!(Backend::Agy.input_dim_ghost_marker(), None);
        assert_eq!(Backend::KiroCli.input_dim_ghost_marker(), None);
    }

    /// app-mode subscriber-wiring source pin. Owned `agend-terminal app` mode
    /// never calls `daemon::run_core`, so `run_app` MUST itself register the
    /// event-bus subscribers — otherwise the maintenance tick emits `CronFire` /
    /// `CiReady` / idle nudges into an empty bus and every delivery silently
    /// drops (the live #1720 cron silent-drop; regression class #1002 / #982).
    ///
    /// File-level positive pin (cross-platform-safe; survives rustfmt re-wrap),
    /// same pattern as `flush_idle_notifications_wired_to_submit_aware_inject`.
    /// The functional counterpart —
    /// `cron_tick::tests::global_bus_cron_subscriber_delivers` — proves the
    /// registered set actually delivers a CronFire on the process-global bus.
    #[test]
    fn run_app_registers_event_bus_subscribers() {
        let source = std::fs::read_to_string("src/app/mod.rs")
            .or_else(|_| std::fs::read_to_string("agend-terminal/src/app/mod.rs"))
            .expect("source file must be readable from test cwd");
        // Search only the production region. This assertion's own literal
        // lives in the #[cfg(test)] module below, so a whole-file substring
        // check self-matches and would stay green even if the real call were
        // deleted. Require the call form (with `(`) before the test cutoff.
        let prod = &source[..source.find("#[cfg(test)]").unwrap_or(source.len())];
        assert!(
            prod.contains("register_event_subscribers("),
            "run_app must call daemon::register_event_subscribers in owned mode \
             (app mode never reaches run_core's registration — #1720 app-mode \
             silent-drop root fix). No call to 'register_event_subscribers(' \
             found in the production region of src/app/mod.rs"
        );
    }

    #[test]
    fn flush_drains_queue_on_idle() {
        let home = tmp_home("flush");
        let mut pane = pane("agent1");
        notification_queue::enqueue(&home, "agent1", "queued").expect("queue notification");
        pane.pending_notification_count = notification_queue::pending_count(&home, "agent1");
        let mut flushed = Vec::new();
        flush_notifications_for_pane(&home, &mut pane, |text| {
            flushed.push(text.to_string());
            Ok(())
        });
        assert_eq!(flushed, vec!["queued".to_string()]);
        assert_eq!(pane.pending_notification_count, 0);
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn flush_respects_disk_compose_state_for_fresh_pane() {
        let home = tmp_home("flush-compose-disk");
        let mut pane = pane("agent1");
        notification_queue::record_input_activity(&home, "agent1");
        notification_queue::enqueue(&home, "agent1", "queued").expect("queue notification");
        pane.pending_notification_count = notification_queue::pending_count(&home, "agent1");

        let mut flushed = Vec::new();
        flush_notifications_for_pane(&home, &mut pane, |text| {
            flushed.push(text.to_string());
            Ok(())
        });

        assert!(
            flushed.is_empty(),
            "fresh pane must respect disk compose state"
        );
        assert_eq!(pane.pending_notification_count, 1);
        std::fs::remove_dir_all(home).ok();
    }

    // -----------------------------------------------------------------------
    // #1762: draft detection — only text-composing input marks a draft
    // -----------------------------------------------------------------------

    /// #1762: navigation / control keys + lone whitespace are NOT text-composing
    /// (they must not defer actionable injects), while real character input,
    /// UTF-8, and bracketed paste ARE (so #1675 still protects a live draft).
    /// Byte forms mirror `tui::key_to_bytes`.
    #[test]
    fn is_text_composing_input_excludes_nav_control_whitespace_1762() {
        // Navigation / control (ESC-prefixed) → NOT composing.
        for seq in [
            &b"\x1b[A"[..], // Up
            b"\x1b[B",      // Down
            b"\x1b[C",      // Right
            b"\x1b[D",      // Left
            b"\x1b[H",      // Home
            b"\x1b[F",      // End
            b"\x1b[5~",     // PageUp
            b"\x1b[6~",     // PageDown
            b"\x1b[3~",     // Delete
            b"\x1b[Z",      // Shift+Tab (BackTab, if ever forwarded)
            b"\x1bOP",      // F1
            b"\x1b",        // Esc
            b"\x1ba",       // Alt+a
        ] {
            assert!(
                !is_text_composing_input(seq),
                "ESC-seq {seq:?} must NOT be text-composing"
            );
        }
        // Bare control bytes → NOT composing.
        assert!(!is_text_composing_input(&[0x01])); // Ctrl+A
        assert!(!is_text_composing_input(b"\t")); // Tab
        assert!(!is_text_composing_input(&[0x7f])); // Backspace (DEL)
        assert!(!is_text_composing_input(b"\r")); // Enter (submit — counted separately)
        assert!(!is_text_composing_input(b"\n")); // Shift+Enter
                                                  // Lone whitespace → NOT composing (#1762 fat-fingered space).
        assert!(!is_text_composing_input(b" "));
        assert!(!is_text_composing_input(b"   "));
        assert!(!is_text_composing_input(&[])); // empty

        // Real character input → IS composing.
        assert!(is_text_composing_input(b"a"));
        assert!(is_text_composing_input(b"hello"));
        assert!(is_text_composing_input(b"hi there")); // space among text still composing
        assert!(is_text_composing_input("café".as_bytes())); // UTF-8
        assert!(is_text_composing_input("日本語".as_bytes())); // multibyte
                                                               // Bracketed paste wraps PASTED TEXT → composing.
        assert!(is_text_composing_input(b"\x1b[200~pasted\x1b[201~"));
    }

    /// #1762 behavioral contract: exercising the exact gate `write_to_focused`
    /// applies (`if is_text_composing_input(bytes) { record_input_activity }`),
    /// a navigation key leaves the pane Clean (actionable injects NOT deferred),
    /// while real typing marks it Drafting (#1675 still protects a live draft).
    /// (`write_to_focused` itself needs a PTY-backed Layout; the wiring is the
    /// 3-line gate, exercised here against the real predicate + draft_state.)
    #[test]
    fn nav_key_does_not_defer_but_typing_does_1762() {
        let home = tmp_home("1762-behavior");
        let agent = "agent1";

        // (a) operator browses history with Up while idle → gate skips → no draft.
        let up = b"\x1b[A";
        if is_text_composing_input(up) {
            notification_queue::record_input_activity(&home, agent);
        }
        assert_eq!(
            notification_queue::draft_state(&home, agent),
            notification_queue::DraftState::None,
            "#1762: a nav key must NOT mark a draft → actionable notif not deferred"
        );

        // (b) operator types real text → gate records → draft present (deferred).
        if is_text_composing_input(b"hello") {
            notification_queue::record_input_activity(&home, agent);
        }
        assert_eq!(
            notification_queue::draft_state(&home, agent),
            notification_queue::DraftState::Drafting,
            "#1762: real typing still marks a draft (#1675 preserved)"
        );
        std::fs::remove_dir_all(home).ok();
    }

    // -----------------------------------------------------------------------
    // Regression pins: app mode tick consumers (t-20260423022134)
    // -----------------------------------------------------------------------

    #[test]
    fn app_mode_fires_one_shot_schedule() {
        // Write a one-shot schedule with past run_at directly to disk,
        // call check_schedules, verify it fires (auto-disabled).
        let home = tmp_home("sched-fire");
        let past = (chrono::Utc::now() - chrono::Duration::seconds(2)).to_rfc3339();
        let store_json = serde_json::json!({
            "schema_version": 2,
            "schedules": [{
                "id": "s-test-oneshot",
                "message": "ping",
                "target": "nonexistent-agent",
                "trigger": {"kind": "once", "at": past},
                "enabled": true,
                "timezone": "UTC",
                "label": "test-oneshot",
                "created_at": chrono::Utc::now().to_rfc3339(),
                "updated_at": chrono::Utc::now().to_rfc3339(),
                "run_history": []
            }]
        });
        std::fs::create_dir_all(&home).expect("create home");
        std::fs::write(
            home.join("schedules.json"),
            serde_json::to_string_pretty(&store_json).expect("serialize"),
        )
        .expect("write schedule file");

        // Fire the tick — schedule is past due, should trigger.
        crate::daemon::cron_tick::check_schedules(&home);

        // Verify: schedule should now be disabled (one-shot auto-disable).
        let store = crate::schedules::load(&home);
        let sched = store.schedules.iter().find(|s| s.id == "s-test-oneshot");
        assert!(
            sched.is_some_and(|s| !s.enabled),
            "one-shot schedule must be auto-disabled after firing"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn app_mode_health_decay_runs() {
        // Verify health.maybe_decay() is callable on an agent handle —
        // binding test that the tick consumer code path compiles and
        // exercises the health decay method.
        use crate::health::HealthTracker;
        let mut health = HealthTracker::new();
        // maybe_decay on a fresh tracker should not panic or change state.
        health.maybe_decay(true);
        assert_eq!(
            health.state.display_name(),
            "healthy",
            "fresh tracker should remain healthy after decay tick"
        );
    }
}

#[cfg(test)]
mod review_repro_app_tui;
