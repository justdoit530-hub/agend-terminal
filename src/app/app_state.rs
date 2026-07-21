//! #2453: root `AppState` / bounded `RestartState` owners for `run_app`'s
//! durable render-loop state.

use std::collections::HashMap;

use crossterm::event::{Event, KeyEventKind};
use ratatui::DefaultTerminal;

use super::*;
use crate::agent::AgentRegistry;
use super::frame_timing::trace_tty_size;
use super::overlay::Overlay;
use super::{
    flush_idle_notifications, kill_agent, sync_notification_state, write_to_focused,
};

/// #2453: typed restart sub-owner. Fields are retained for the structural
/// contract even when this fork has no live restart machinery wired yet.
#[derive(Default)]
#[allow(dead_code)]
pub(super) struct RestartState {
    pub(super) restart_outcome: (),
    pub(super) restart_probe: Option<()>,
    pub(super) restart_commit_pending: Option<()>,
}

pub(super) struct AppState {
    pub(super) ui: UiState,
    pub(super) known_remote_agents: std::collections::HashSet<String>,
    pub(super) pending_fwd: HashMap<usize, crossbeam_channel::Sender<Vec<u8>>>,
    pub(super) needs_resize: bool,
    pub(super) last_remote_sync: std::time::Instant,
    pub(super) last_session_save: std::time::Instant,
    pub(super) last_session_json: Option<String>,
    pub(super) last_draw: Option<std::time::Instant>,
    pub(super) dirty: bool,
    pub(super) last_notif_sync: Option<std::time::Instant>,
    /// Reserved durable owner (#2453); decision-badge path not yet ported.
    #[allow(dead_code)]
    pub(super) last_decision_sync: Option<std::time::Instant>,
    /// Reserved durable owner (#2453); decision-badge path not yet ported.
    #[allow(dead_code)]
    pub(super) pending_decisions_total: usize,
    pub(super) booting: bool,
    pub(super) boot_start: std::time::Instant,
    pub(super) attaches_expected: usize,
    #[allow(dead_code)]
    pub(super) restart: RestartState,
}

pub(super) type AttachPipeline = (
    crossbeam_channel::Sender<pane_factory::AttachOutcome>,
    crossbeam_channel::Receiver<pane_factory::AttachOutcome>,
    Vec<std::thread::JoinHandle<()>>,
);

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum LoopFlow {
    Continue,
    Break,
}

#[derive(Clone, Copy)]
pub(super) struct AppDeps<'a> {
    pub home: &'a std::path::PathBuf,
    pub fleet_path: &'a std::path::PathBuf,
    pub registry: &'a AgentRegistry,
    pub wakeup_tx: &'a crossbeam_channel::Sender<usize>,
    pub daemon_binary_stale: &'a crate::daemon::mcp_registry_watcher::DaemonBinaryStale,
    pub telegram_status: TelegramStatus,
    pub telegram_state: &'a Option<std::sync::Arc<dyn crate::channel::Channel>>,
    pub attached_run_dir: &'a Option<std::path::PathBuf>,
    pub attached_mode: bool,
    pub size_debug: bool,
}

impl AppState {
    pub(super) fn new() -> Self {
        Self {
            ui: UiState {
                layout: crate::layout::Layout::new(),
                last_tab: 0,
                name_counter: HashMap::new(),
                overlay: Overlay::None,
                key_handler: KeyHandler::new(),
                mouse_state: mouse::MouseState::default(),
            },
            known_remote_agents: std::collections::HashSet::new(),
            pending_fwd: HashMap::new(),
            needs_resize: true,
            last_remote_sync: std::time::Instant::now(),
            last_session_save: std::time::Instant::now(),
            last_session_json: None,
            last_draw: None,
            dirty: true,
            last_notif_sync: None,
            last_decision_sync: None,
            pending_decisions_total: 0,
            booting: true,
            boot_start: std::time::Instant::now(),
            attaches_expected: 0,
            restart: RestartState::default(),
        }
    }

    pub(super) fn restore_panes(
        &mut self,
        deps: &AppDeps<'_>,
        restore_start: std::time::Instant,
    ) -> Result<Vec<pane_factory::AttachJob>> {
        let AppDeps {
            home,
            fleet_path,
            registry,
            wakeup_tx,
            attached_run_dir,
            attached_mode,
            size_debug,
            ..
        } = *deps;
        let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
        let pane_rows = rows.saturating_sub(4);
        let pane_cols = cols.saturating_sub(2);

        let mut attach_jobs = Vec::new();

        if let Some(ref run_dir) = attached_run_dir {
            let started = session::restore_with_reconciliation_attached(
                home,
                fleet_path,
                run_dir,
                &mut self.ui.layout,
                wakeup_tx,
                pane_cols,
                pane_rows,
            );
            for tab in &self.ui.layout.tabs {
                for name in tab.root().agent_names() {
                    self.known_remote_agents.insert(name);
                }
            }
            if !started {
                tracing::warn!(
                    "attached to daemon but no agents are reachable; check `agend-terminal list`"
                );
            }
        } else {
            let started = session::restore_with_reconciliation(
                home,
                fleet_path,
                &mut self.ui.layout,
                &mut self.ui.name_counter,
                &mut attach_jobs,
                pane_cols,
                pane_rows,
            );
            if !started {
                pane_factory::spawn_pane_tab(
                    &mut self.ui.layout,
                    registry,
                    home,
                    "shell",
                    &crate::shell_command(),
                    &[],
                    crate::backend::SpawnMode::Fresh,
                    None,
                    &HashMap::new(),
                    "\r",
                    pane_cols,
                    pane_rows,
                    wakeup_tx,
                    &mut self.ui.name_counter,
                    pane_factory::SpawnIdentity::UnmanagedLocalShell,
                )?;
            }
        }

        trace_tty_size(size_debug, "post-fleet-spawn");
        tracing::info!(
            phase = "restore-complete",
            elapsed_ms = restore_start.elapsed().as_millis() as u64,
            attached = attached_mode,
            "restore-complete: session restore + fleet PTY spawns done"
        );
        Ok(attach_jobs)
    }

    pub(super) fn schedule_deferred_attaches(
        &mut self,
        mut attach_jobs: Vec<pane_factory::AttachJob>,
        attach_tx: &crossbeam_channel::Sender<pane_factory::AttachOutcome>,
        deps: &AppDeps<'_>,
    ) -> Vec<std::thread::JoinHandle<()>> {
        let AppDeps { home, registry, .. } = *deps;
        let mut attach_workers: Vec<std::thread::JoinHandle<()>> = Vec::new();
        if !attach_jobs.is_empty() {
            let (job_tx, job_rx) =
                crossbeam_channel::unbounded::<(usize, pane_factory::AttachSpec)>();
            const ATTACH_WORKERS: usize = 3;
            for w in 0..ATTACH_WORKERS {
                let job_rx = job_rx.clone();
                let attach_tx = attach_tx.clone();
                let registry = std::sync::Arc::clone(registry);
                let home = home.clone();
                let handle = std::thread::Builder::new()
                    .name(format!("attach_worker_{w}"))
                    .spawn(move || {
                        while let Ok((pane_id, spec)) = job_rx.recv() {
                            let outcome = pane_factory::run_attach(spec, pane_id, &registry, &home);
                            if attach_tx.send(outcome).is_err() {
                                break;
                            }
                        }
                    })
                    .expect("spawn attach worker");
                attach_workers.push(handle);
            }
            for job in attach_jobs.drain(..) {
                let pane_factory::AttachJob {
                    pane_id,
                    fwd_tx,
                    spec,
                } = job;
                self.pending_fwd.insert(pane_id, fwd_tx);
                let _ = job_tx.send((pane_id, spec));
            }
            drop(job_tx);
        }

        self.last_remote_sync = std::time::Instant::now();
        self.last_session_save = std::time::Instant::now();
        self.boot_start = std::time::Instant::now();
        self.attaches_expected = self.pending_fwd.len();
        attach_workers
    }

    pub(super) fn restore_and_attach(
        &mut self,
        deps: &AppDeps<'_>,
        restore_start: std::time::Instant,
    ) -> Result<AttachPipeline> {
        let attach_jobs = self.restore_panes(deps, restore_start)?;
        let (attach_tx, attach_rx) = crossbeam_channel::unbounded::<pane_factory::AttachOutcome>();
        let attach_workers = self.schedule_deferred_attaches(attach_jobs, &attach_tx, deps);
        Ok((attach_tx, attach_rx, attach_workers))
    }

    pub(super) fn poll_restart(&mut self, _deps: &AppDeps<'_>) -> LoopFlow {
        LoopFlow::Continue
    }

    pub(super) fn pre_select(&mut self, terminal: &mut DefaultTerminal, deps: &AppDeps<'_>) {
        self.close_dead_scratch_shell(deps);
        self.apply_pending_resize(terminal, deps);
        self.sync_badges(deps);
    }

    pub(super) fn close_dead_scratch_shell(&mut self, deps: &AppDeps<'_>) {
        let AppDeps { home, registry, .. } = *deps;
        if let Overlay::ScratchShell { pane } = &self.ui.overlay {
            if !agent_is_alive(registry, &pane.agent_name) {
                let name = pane.agent_name.clone();
                self.ui.overlay = Overlay::None;
                kill_agent(home, registry, &name);
            }
        }
    }

    pub(super) fn apply_pending_resize(
        &mut self,
        terminal: &mut DefaultTerminal,
        deps: &AppDeps<'_>,
    ) {
        let AppDeps { registry, .. } = *deps;
        if self.needs_resize {
            let (c, r) = crossterm::terminal::size().unwrap_or((120, 40));
            let pane_area = ratatui::layout::Rect::new(0, 1, c, r.saturating_sub(2));
            crate::layout::resize_panes(pane_area, &mut self.ui.layout, registry);
            let _ = terminal.clear();
            self.needs_resize = false;
        }
    }

    pub(super) fn sync_badges(&mut self, deps: &AppDeps<'_>) {
        let AppDeps { home, .. } = *deps;
        let notif_now = std::time::Instant::now();
        if super::frame_timing::should_sync_notifications(
            self.last_notif_sync,
            notif_now,
            NOTIF_SYNC_INTERVAL,
        ) {
            self.last_notif_sync = Some(notif_now);
            sync_notification_state(home, &mut self.ui.layout);
        }
        {
            static LAST_FLUSH: std::sync::Mutex<Option<std::time::Instant>> =
                std::sync::Mutex::new(None);
            let now = std::time::Instant::now();
            let should_flush = LAST_FLUSH
                .lock()
                .map(|guard| {
                    guard
                        .map(|t| now.duration_since(t).as_secs() >= 1)
                        .unwrap_or(true)
                })
                .unwrap_or(true);
            if should_flush {
                flush_idle_notifications(home, &mut self.ui.layout);
                if let Ok(mut guard) = LAST_FLUSH.lock() {
                    *guard = Some(now);
                }
            }
        }
    }

    pub(super) fn render_frame(
        &mut self,
        terminal: &mut DefaultTerminal,
        deps: &AppDeps<'_>,
    ) -> Result<()> {
        let AppDeps {
            home,
            registry,
            daemon_binary_stale,
            telegram_status,
            size_debug,
            ..
        } = *deps;
        let repeat_mode = self.ui.key_handler.in_repeat();

        if size_debug {
            let cross = crossterm::terminal::size().unwrap_or((0, 0));
            let term_sz = terminal
                .size()
                .map(|s| (s.width, s.height))
                .unwrap_or((0, 0));
            tracing::info!(
                tag = "#2057-size",
                crossterm_cols = cross.0,
                crossterm_rows = cross.1,
                terminal_size = ?term_sz,
                tabs = self.ui.layout.tabs.len(),
                "TUI draw size probe"
            );
        }

        let frame_now = std::time::Instant::now();
        if self.dirty && should_draw(self.last_draw, frame_now, FRAME_INTERVAL) {
            self.last_draw = Some(frame_now);
            self.dirty = false;
            if self.booting {
                let backlog_remains =
                    render::drain_all_panes_until(&mut self.ui.layout, BOOT_FRAME_TIME_CAP);
                let timed_out = self.boot_start.elapsed() >= MAX_BOOT_CATCHUP;
                if (self.pending_fwd.is_empty() && !backlog_remains) || timed_out {
                    self.booting = false;
                    tracing::info!(
                        phase = "boot-catchup-complete",
                        elapsed_ms = self.boot_start.elapsed().as_millis() as u64,
                        attaches_expected = self.attaches_expected,
                        attaches_pending = self.pending_fwd.len(),
                        timed_out = timed_out,
                        "#freeze-4: restart-flood boot catch-up drained"
                    );
                }
            } else {
                render::drain_all_panes(&mut self.ui.layout);
            }
            terminal.draw(|frame| {
                let binary_stale = daemon_binary_stale.load(std::sync::atomic::Ordering::Relaxed);
                if size_debug {
                    let a = frame.area();
                    tracing::info!(
                        tag = "#2057-area",
                        x = a.x,
                        y = a.y,
                        w = a.width,
                        h = a.height,
                        "frame.area() in draw"
                    );
                }
                render::render(
                    frame,
                    &mut self.ui.layout,
                    repeat_mode,
                    registry,
                    telegram_status,
                    binary_stale,
                );
                render_active_overlay(frame, &mut self.ui.overlay, &self.ui.layout, registry, home);
                if self.booting {
                    render::render_boot_indicator(
                        frame,
                        self.attaches_expected.saturating_sub(self.pending_fwd.len()),
                        self.attaches_expected,
                    );
                }
            })?;
            if self.booting || render::active_tab_has_pending_output(&self.ui.layout) {
                self.dirty = true;
            }
        }
        Ok(())
    }

    pub(super) fn select_timeout(&self) -> std::time::Duration {
        if self.dirty {
            match self.last_draw {
                Some(t) => FRAME_INTERVAL
                    .saturating_sub(t.elapsed())
                    .max(std::time::Duration::from_millis(1)),
                None => std::time::Duration::from_millis(1),
            }
        } else {
            std::time::Duration::from_millis(50)
        }
    }

    pub(super) fn handle_crossterm_event(
        &mut self,
        ev: Result<Event, crossbeam_channel::RecvError>,
        terminal: &mut DefaultTerminal,
        deps: &AppDeps<'_>,
    ) -> LoopFlow {
        let AppDeps {
            home,
            fleet_path,
            registry,
            wakeup_tx,
            telegram_state,
            ..
        } = *deps;
        self.dirty = true;
        let ev = match ev {
            Ok(e) => e,
            Err(_) => return LoopFlow::Break,
        };
        match ev {
            Event::Key(key) if key.kind != KeyEventKind::Press => LoopFlow::Continue,
            Event::Key(key) => {
                let ui_deps = UiDeps {
                    registry,
                    home,
                    fleet_path,
                    wakeup_tx,
                    telegram_state,
                };
                let outcome = self.ui.handle_key_event(key, &ui_deps);
                if outcome.needs_resize {
                    self.needs_resize = true;
                }
                if outcome.should_break {
                    LoopFlow::Break
                } else {
                    LoopFlow::Continue
                }
            }
            Event::Mouse(_) if !matches!(self.ui.overlay, Overlay::None) => LoopFlow::Continue,
            Event::Mouse(mouse_evt) => {
                let ui_deps = UiDeps {
                    registry,
                    home,
                    fleet_path,
                    wakeup_tx,
                    telegram_state,
                };
                let needs_resize = self.ui.handle_mouse_event(mouse_evt, &ui_deps);
                if needs_resize {
                    self.needs_resize = true;
                }
                LoopFlow::Continue
            }
            Event::Paste(text) => {
                match &mut self.ui.overlay {
                    Overlay::RenameTab { ref mut input }
                    | Overlay::RenamePane { ref mut input } => {
                        input.push_str(&text);
                    }
                    Overlay::Command {
                        ref mut input,
                        ref mut selected,
                    } => {
                        input.push_str(&text);
                        *selected = 0;
                    }
                    Overlay::ScratchShell { pane } => {
                        pane.write_input(registry, text.as_bytes());
                    }
                    Overlay::None => {
                        write_to_focused(home, &mut self.ui.layout, registry, text.as_bytes());
                    }
                    _ => {}
                }
                LoopFlow::Continue
            }
            Event::Resize(cols, rows) => {
                let pane_area = ratatui::layout::Rect::new(0, 1, cols, rows.saturating_sub(2));
                crate::layout::resize_panes(pane_area, &mut self.ui.layout, registry);
                let _ = terminal.clear();
                LoopFlow::Continue
            }
            _ => LoopFlow::Continue,
        }
    }

    pub(super) fn handle_wakeup(&mut self, wakeup_rx: &crossbeam_channel::Receiver<usize>) {
        while wakeup_rx.try_recv().is_ok() {}
        self.dirty = true;
    }

    pub(super) fn handle_attach_outcome(
        &mut self,
        outcome: Result<pane_factory::AttachOutcome, crossbeam_channel::RecvError>,
        deps: &AppDeps<'_>,
    ) {
        let AppDeps { home, registry, wakeup_tx, .. } = *deps;
        self.dirty = true;
        if let Ok(outcome) = outcome {
            let pane_id = outcome.pane_id();
            if let Some(fwd_tx) = self.pending_fwd.remove(&pane_id) {
                if let Some(pane) = self.ui.layout.find_pane_mut(pane_id) {
                    pane_factory::apply_attach_outcome(pane, registry, outcome, fwd_tx, wakeup_tx);
                } else if let pane_factory::AttachOutcome::Ready { name, .. } = &outcome {
                    kill_agent(home, registry, name);
                }
            }
        }
    }

    pub(super) fn handle_tui_event(
        &mut self,
        ev: Result<TuiEvent, crossbeam_channel::RecvError>,
        terminal: &mut DefaultTerminal,
        deps: &AppDeps<'_>,
    ) {
        let AppDeps {
            home,
            registry,
            wakeup_tx,
            daemon_binary_stale,
            telegram_status,
            ..
        } = *deps;
        self.dirty = true;
        if let Ok(event) = ev {
            if let TuiEvent::ScreenshotRequest(tx) = event {
                let svg = {
                    let size = terminal.size().unwrap_or_default();
                    let backend = ratatui::backend::TestBackend::new(
                        if size.width > 0 { size.width } else { 120 },
                        if size.height > 0 { size.height } else { 40 },
                    );
                    let mut snap_term =
                        ratatui::Terminal::new(backend).expect("TestBackend::new cannot fail");
                    let binary_stale =
                        daemon_binary_stale.load(std::sync::atomic::Ordering::Relaxed);
                    let _ = snap_term.draw(|frame| {
                        crate::render::render(
                            frame,
                            &mut self.ui.layout,
                            self.ui.key_handler.in_repeat(),
                            registry,
                            telegram_status,
                            binary_stale,
                        );
                        render_active_overlay(
                            frame,
                            &mut self.ui.overlay,
                            &self.ui.layout,
                            registry,
                            home,
                        );
                    });
                    crate::screenshot::buffer_to_svg(snap_term.backend())
                };
                let _ = tx.send(svg);
            } else {
                tui_events::handle_tui_event(event, &mut self.ui.layout, registry, wakeup_tx);
            }
            self.needs_resize = true;
        }
    }

    pub(super) fn handle_maintenance_tick(
        &mut self,
        deps: &AppDeps<'_>,
        app_externals: &crate::agent::ExternalRegistry,
        app_configs: &crate::api::ConfigRegistry,
        app_handlers: &[Box<dyn crate::daemon::per_tick::PerTickHandler>],
    ) {
        let AppDeps { home, registry, .. } = *deps;
        self.dirty = true;
        app_maintenance_tick(home, registry, app_externals, app_configs, app_handlers);
    }

    pub(super) fn handle_idle_tick(&mut self, deps: &AppDeps<'_>) {
        let AppDeps {
            home,
            fleet_path,
            wakeup_tx,
            attached_run_dir,
            ..
        } = *deps;
        self.dirty = true;
        if self.last_session_save.elapsed() >= std::time::Duration::from_secs(10) {
            self.last_session_save = std::time::Instant::now();
            session::save_session_if_changed(home, &self.ui.layout, &mut self.last_session_json);
        }
        if attached_run_dir.is_some()
            && self.last_remote_sync.elapsed() >= std::time::Duration::from_secs(2)
        {
            let current: std::collections::HashSet<String> =
                crate::runtime::list_agents_with_fallback(home)
                    .into_iter()
                    .collect();
            let mut to_add: Vec<String> = current
                .difference(&self.known_remote_agents)
                .cloned()
                .collect();
            to_add.sort();
            for name in &to_add {
                let (dc, dr) = crossterm::terminal::size().unwrap_or((120, 40));
                match pane_factory::create_remote_pane(
                    name,
                    home,
                    fleet_path,
                    &mut self.ui.layout,
                    dc.saturating_sub(2),
                    dr.saturating_sub(4),
                    wakeup_tx,
                ) {
                    Ok(pane) => {
                        let tab_name = pane.agent_name.clone();
                        self.known_remote_agents.insert(tab_name.to_string());
                        if let Some(idx) = self.ui.layout.single_pane_tab_index_for_agent(&tab_name)
                        {
                            self.ui.layout.tabs[idx] =
                                crate::layout::Tab::new(tab_name.to_string(), pane);
                            tracing::info!(
                                agent = %name,
                                "reused retained tab for re-appeared remote agent (no duplicate)"
                            );
                        } else {
                            self.ui.layout.push_tab_preserve_focus(
                                crate::layout::Tab::new(tab_name.to_string(), pane),
                            );
                            tracing::info!(
                                agent = %name,
                                "opened tab for newly-appeared remote agent"
                            );
                        }
                        self.needs_resize = true;
                    }
                    Err(e) => tracing::warn!(
                        agent = %name,
                        error = %e,
                        "remote pane attach failed during sync",
                    ),
                }
            }
            let gone: Vec<String> = self
                .known_remote_agents
                .difference(&current)
                .cloned()
                .collect();
            for name in &gone {
                tracing::warn!(
                    agent = %name,
                    "daemon-side agent gone; pane retained with stale output",
                );
                self.known_remote_agents.remove(name);
            }
            self.last_remote_sync = std::time::Instant::now();
        }
    }
}
