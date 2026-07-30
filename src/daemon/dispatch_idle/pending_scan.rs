use std::path::Path;

use super::{pending_dir, PendingDispatch, SCHEMA_VERSION};

/// Read all pending dispatch sidecars from disk. Forward-compat: skips
/// any sidecar whose `schema_version` is unknown.
pub(crate) fn list_pending(home: &Path) -> Vec<PendingDispatch> {
    note_list_pending();

    let dir = pending_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(d) = serde_json::from_str::<PendingDispatch>(&content) else {
            continue;
        };
        if d.schema_version != SCHEMA_VERSION {
            continue;
        }
        out.push(d);
    }
    out.sort_by(|a, b| a.issued_at.cmp(&b.issued_at));
    out
}

#[cfg(test)]
thread_local! {
    static LIST_PENDING_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn note_list_pending() {
    LIST_PENDING_CALLS.with(|count| count.set(count.get() + 1));
}

#[cfg(not(test))]
pub(crate) fn note_list_pending() {}

#[cfg(test)]
pub(crate) fn reset_list_pending_call_count() {
    LIST_PENDING_CALLS.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn take_list_pending_call_count() -> usize {
    LIST_PENDING_CALLS.with(|count| count.replace(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::dispatch_idle::{pending_path, record_dispatch, PendingDispatch};
    use std::path::PathBuf;

    fn tmp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agend-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// #3001: a new correlated dispatch reuses one pending-sidecar snapshot for
    /// deduplication and stale-handoff cleanup. An older same-route sidecar with a
    /// different correlation is covered by one shared scan; refresh and
    /// no-correlation gates remain one and zero scans respectively.
    #[test]
    fn record_dispatch_reuses_pending_scan_for_stale_cleanup_3001() {
        let home = tmp_home("3001-single-pending-scan");
        let old_id = record_dispatch(
            &home,
            "sender-3001",
            "target-3001",
            Some("t-3001-old"),
            "task",
            600,
        )
        .expect("old sidecar");
        let old_path = pending_path(&home, &old_id);
        let mut old: PendingDispatch =
            serde_json::from_str(&std::fs::read_to_string(&old_path).expect("read old sidecar"))
                .expect("parse old sidecar");
        old.issued_at = "2020-01-01T00:00:00Z".to_string();
        crate::store::atomic_write(
            &old_path,
            serde_json::to_string_pretty(&old)
                .expect("serialize old sidecar")
                .as_bytes(),
        )
        .expect("rewrite old sidecar");

        reset_list_pending_call_count();
        let new_id = record_dispatch(
            &home,
            "sender-3001",
            "target-3001",
            Some("t-3001-new"),
            "task",
            600,
        )
        .expect("new sidecar");
        assert_eq!(
            take_list_pending_call_count(),
            1,
            "an older same-route sidecar must be covered by one shared scan"
        );
        let pending = list_pending(&home);
        assert!(
            pending.iter().all(|p| p.dispatch_id != old_id),
            "new dispatch must retire the older same-route sidecar"
        );
        assert!(pending.iter().any(|p| p.dispatch_id == new_id));

        reset_list_pending_call_count();
        let refreshed = record_dispatch(
            &home,
            "sender-3001",
            "target-3001",
            Some("t-3001-new"),
            "task",
            600,
        )
        .expect("refresh sidecar");
        assert_eq!(
            refreshed, new_id,
            "refresh must preserve the existing dispatch id"
        );
        assert_eq!(
            take_list_pending_call_count(),
            1,
            "refreshing an existing intent must retain its one-scan early return"
        );

        reset_list_pending_call_count();
        record_dispatch(&home, "sender-3001", "target-3001", None, "task", 600)
            .expect("uncorrelated sidecar");
        assert_eq!(
            take_list_pending_call_count(),
            0,
            "uncorrelated dispatches must not scan for dedup or stale handoff"
        );

        std::fs::remove_dir_all(&home).ok();
    }
}
