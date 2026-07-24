//! #2453: `UiState` holds layout, key handler, mouse state, name counter,
//! active overlay, and last tab index.

use std::collections::HashMap;
use std::path::Path;

use crossterm::event::{KeyEvent, MouseEvent};

use super::mouse;
use super::overlay::{self, Overlay, OverlayCtx};
use super::*;
use crate::agent::AgentRegistry;
use crate::layout::Layout;

pub(super) struct UiState {
    pub(super) layout: Layout,
    pub(super) last_tab: usize,
    pub(super) name_counter: HashMap<String, usize>,
    pub(super) overlay: Overlay,
    pub(super) key_handler: KeyHandler,
    pub(super) mouse_state: mouse::MouseState,
}

pub(super) struct UiDeps<'a> {
    pub(super) registry: &'a AgentRegistry,
    pub(super) home: &'a Path,
    pub(super) fleet_path: &'a Path,
    pub(super) wakeup_tx: &'a crossbeam_channel::Sender<usize>,
    pub(super) telegram_state: &'a Option<std::sync::Arc<dyn crate::channel::Channel>>,
}

#[derive(Default)]
pub(super) struct KeyOutcome {
    pub(super) needs_resize: bool,
    pub(super) should_break: bool,
}

impl UiState {
    pub(super) fn handle_key_event(&mut self, key: KeyEvent, deps: &UiDeps<'_>) -> KeyOutcome {
        let mut out = KeyOutcome::default();
        if !matches!(self.overlay, Overlay::None) {
            let mut octx = OverlayCtx {
                layout: &mut self.layout,
                registry: deps.registry,
                home: deps.home,
                fleet_path: deps.fleet_path,
                wakeup_tx: deps.wakeup_tx,
                name_counter: &mut self.name_counter,
                telegram_state: deps.telegram_state,
            };
            let outcome = overlay::handle_key(&mut self.overlay, key, &mut octx);
            out.needs_resize = outcome.needs_resize;
            return out;
        }

        let action = self.key_handler.handle(key);
        let mut dctx = dispatch::DispatchCtx {
            layout: &mut self.layout,
            registry: deps.registry,
            home: deps.home,
            fleet_path: deps.fleet_path,
            last_tab: &mut self.last_tab,
            wakeup_tx: deps.wakeup_tx,
            name_counter: &mut self.name_counter,
        };
        let res = dispatch::dispatch(action, &mut dctx);
        out.needs_resize = res.needs_resize;
        out.should_break = res.should_break;
        if let Some(ov) = res.new_overlay {
            self.overlay = ov;
        }
        out
    }

    pub(super) fn handle_mouse_event(&mut self, mouse_evt: MouseEvent, deps: &UiDeps<'_>) -> bool {
        if !matches!(self.overlay, Overlay::None) {
            return false;
        }
        let out = mouse::handle(
            mouse_evt,
            &mut self.layout,
            &mut self.mouse_state,
            deps.fleet_path,
            deps.registry,
        );
        let needs_resize = out.needs_resize;
        if let Some(prev) = out.new_last_tab {
            self.last_tab = prev;
        }
        if let Some(ov) = out.new_overlay {
            self.overlay = ov;
        }
        needs_resize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use crossterm::event::{MouseButton, MouseEventKind};

    #[test]
    fn ui_state_routes_key_and_mouse_events() {
        let registry: AgentRegistry = std::sync::Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = std::env::temp_dir().join(format!("agend-uistate-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let fleet_path = home.join("fleet.yaml");
        let (wakeup_tx, _rx) = crossbeam_channel::unbounded::<usize>();
        let telegram_state: Option<std::sync::Arc<dyn crate::channel::Channel>> = None;
        let deps = UiDeps {
            registry: &registry,
            home: &home,
            fleet_path: &fleet_path,
            wakeup_tx: &wakeup_tx,
            telegram_state: &telegram_state,
        };
        let mut ui = UiState {
            layout: Layout::new(),
            last_tab: 0,
            name_counter: HashMap::new(),
            overlay: Overlay::None,
            key_handler: KeyHandler::new(),
            mouse_state: mouse::MouseState::default(),
        };

        let ctrl_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        ui.handle_key_event(ctrl_b, &deps);
        assert!(
            matches!(ui.overlay, Overlay::None),
            "prefix must not open overlay"
        );
        let c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::empty());
        let out = ui.handle_key_event(c, &deps);
        assert!(!out.should_break, "NewTab must not break the loop");
        assert!(
            matches!(ui.overlay, Overlay::NewTabMenu { .. }),
            "dispatch path must apply new_overlay (NewTab opens the menu)"
        );

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
        ui.handle_key_event(esc, &deps);
        assert!(
            matches!(ui.overlay, Overlay::None),
            "Esc must close the overlay"
        );

        seed_border_drag(&mut ui);
        let needs_resize = ui.handle_mouse_event(mouse_up(), &deps);
        assert!(
            needs_resize,
            "non-modal border-drag mouse-up must request a resize"
        );
        assert!(
            ui.mouse_state.border_drag.is_none(),
            "mouse-up must clear the border-drag"
        );

        seed_border_drag(&mut ui);
        ui.overlay = Overlay::Help;
        let needs_resize = ui.handle_mouse_event(mouse_up(), &deps);
        assert!(
            !needs_resize,
            "modal overlay must swallow the mouse event (no resize)"
        );
        assert!(
            ui.mouse_state.border_drag.is_some(),
            "modal swallow must leave the border-drag untouched"
        );
        assert!(
            matches!(ui.overlay, Overlay::Help),
            "modal swallow must not change the overlay"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    fn mouse_up() -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        }
    }

    fn seed_border_drag(ui: &mut UiState) {
        ui.mouse_state.border_drag = Some((
            crate::layout::SplitBorderHit {
                split_area: (0, 1, 60, 38),
                dir: crate::layout::SplitDir::Vertical,
            },
            ratatui::layout::Rect::new(0, 1, 120, 38),
        ));
    }
}
