use std::path::Path;

pub(crate) fn write_daemon_id(run_dir: &Path) {
    let pid = std::process::id();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let token = crate::process::process_start_token(pid).unwrap_or(0);
    let _ = crate::store::atomic_write(&run_dir.join(".daemon"), format!("{pid}:{now}:{token}").as_bytes());
}

pub(crate) fn read_daemon_pid(run_dir: &Path) -> Option<u32> {
    std::fs::read_to_string(run_dir.join(".daemon")).ok()?.trim().split_once(':').and_then(|(pid, _)| pid.parse().ok())
}

pub(crate) fn read_daemon_boot_unix(run_dir: &Path) -> Option<u64> {
    std::fs::read_to_string(run_dir.join(".daemon")).ok()?.trim().split(':').nth(1).and_then(|ts| ts.parse().ok())
}

pub(crate) fn read_daemon_start_token(run_dir: &Path) -> Option<u64> {
    std::fs::read_to_string(run_dir.join(".daemon")).ok()?.trim().split(':').nth(2).and_then(|t| t.parse().ok())
}
