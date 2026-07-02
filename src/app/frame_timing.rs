//! Frame-rate and notification-sync throttles for the TUI render loop.

pub(crate) const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);
pub(crate) const BOOT_FRAME_TIME_CAP: std::time::Duration = std::time::Duration::from_millis(80);
pub(crate) const MAX_BOOT_CATCHUP: std::time::Duration = std::time::Duration::from_millis(1500);
pub(crate) const NOTIF_SYNC_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

pub(crate) fn trace_tty_size(enabled: bool, phase: &str) {
    if !enabled {
        return;
    }
    let (cols, rows) = crossterm::terminal::size().unwrap_or((0, 0));
    tracing::info!(
        tag = "#2057-startup",
        phase,
        cols,
        rows,
        "controlling-TTY kernel winsize at startup milestone"
    );
}

pub(crate) fn should_draw(
    last_draw: Option<std::time::Instant>,
    now: std::time::Instant,
    frame_interval: std::time::Duration,
) -> bool {
    match last_draw {
        None => true,
        Some(t) => now.duration_since(t) >= frame_interval,
    }
}

pub(crate) fn should_sync_notifications(
    last_sync: Option<std::time::Instant>,
    now: std::time::Instant,
    interval: std::time::Duration,
) -> bool {
    match last_sync {
        None => true,
        Some(t) => now.duration_since(t) >= interval,
    }
}
