//! Verdict side-effects.

use std::path::Path;

pub(crate) fn process_verdicts(home: &Path, from: &str, msg: &crate::inbox::InboxMessage) {
    // #2010 2a: enqueue a release intent for ANY terminal verdict (widened from
    // VERIFIED-only). The sweeper releases the verdict-sender's (reviewer's) own
    // binding once their review task is terminal, bypassing the open-PR gate —
    // so a REJECTED/UNVERIFIED reviewer no longer leaks its worktree to the lead's
    // rework re-dispatch. The verdict KIND is irrelevant to the release path; the
    // pr_state recording below keys on the actual word.
    if crate::daemon::auto_release::is_verdict_message(msg) {
        if let Some(task_id) = msg.correlation_id.as_ref() {
            let intent = crate::daemon::auto_release::AutoReleaseIntent {
                task_id: task_id.clone(),
                reviewer: from.to_string(),
                verdict_msg_id: msg.id.clone(),
                reviewed_head: msg.reviewed_head.clone(),
                enqueued_at: chrono::Utc::now().to_rfc3339(),
                // t-worktree-leak (PR-1) Q1(b): a verdict no longer releases an
                // OPEN PR's worktree by default — the sweeper gates it through the
                // release invariant, so an IMPLEMENTER's release waits for the
                // terminal (merge/close) or no-PR+task-done event. The #2010 2a
                // reviewer-binding bypass is the sole exception, scoped to the
                // verdict sender's own binding. repo/branch/lease are derived by
                // the sweeper from the live binding.
                event_kind: Some("verdict".to_string()),
                repo: None,
                branch: None,
                lease: None,
            };
            if let Err(e) = crate::daemon::auto_release::enqueue_intent(home, &intent) {
                tracing::warn!(task_id = %task_id, error = %e, "#870 auto_release: enqueue failed");
            }
        }
    }
    // pr_state verdict recording — independent of the enqueue gate above and
    // keyed to the actual verdict word. VERIFIED keeps its §4.2 reviewed_head
    // staleness gate (a head-less VERIFIED must not flip the merge gate);
    // REJECTED/UNVERIFIED record regardless (UNVERIFIED is evidence-exempt).
    if msg.kind.as_deref() == Some("report") && msg.correlation_id.is_some() {
        // #2059: strip the `[report_result] ` wrapper (added by
        // comms::handle_report_result) via the SHARED helper, so the verdict-word
        // check sees the bare word — the same strip `is_terminal_verdict_text`
        // uses, so the two verdict consumers never drift. Without this, the
        // wrapped real wire text never matched and record_verdict was never
        // called (the pipeline-wide silence #2059 RCA'd).
        let text = crate::daemon::auto_release::strip_report_wrapper(&msg.text);
        let task_id = msg.correlation_id.as_deref().unwrap_or("");
        if text.starts_with("VERIFIED") {
            if msg.reviewed_head.is_some() {
                crate::daemon::pr_state::record_verdict(
                    home,
                    task_id,
                    from,
                    msg.reviewed_head.as_deref(),
                    crate::daemon::pr_state::VerdictKind::Verified,
                );
            }
        } else if text.starts_with("REJECTED") {
            crate::daemon::pr_state::record_verdict(
                home,
                task_id,
                from,
                msg.reviewed_head.as_deref(),
                crate::daemon::pr_state::VerdictKind::Rejected { reason: None },
            );
        } else if text.starts_with("UNVERIFIED") {
            crate::daemon::pr_state::record_verdict(
                home,
                task_id,
                from,
                msg.reviewed_head.as_deref(),
                crate::daemon::pr_state::VerdictKind::Unverified,
            );
        }
    }
}

pub(crate) fn bridge_verdict_to_review_task(
    home: &Path,
    reporter: &str,
    msg: &crate::inbox::InboxMessage,
) {
    use crate::mcp::handlers::comms_gates::{detect_verdict, Verdict};
    // Only ACTUAL verdict reports: a leading VERIFIED/REJECTED/UNVERIFIED token
    // (§3.12) AND a `reviewed_head` SHA (every reviewer verdict carries it, #1024).
    let Some(verdict) = detect_verdict(&msg.text) else {
        return;
    };
    if msg.reviewed_head.is_none() {
        return;
    }
    let corr = msg.correlation_id.as_deref().or(msg.task_id.as_deref());
    let task_id: Option<String> = match corr {
        Some(c) if c.starts_with("t-") => Some(c.to_string()),
        _ => crate::daemon::dispatch_idle::open_review_dispatch_for_reporter(home, reporter),
    };
    let Some(task_id) = task_id else {
        return;
    };
    // Any verdict → the reviewer responded → clear the dispatch sidecar (kills the
    // post-response stuck-nudge), regardless of VERIFIED vs REJECTED.
    let _ = crate::daemon::dispatch_idle::mark_resolved(home, &task_id);
    // Only VERIFIED closes the review task. terminal=true synthesized internally.
    if matches!(verdict, Verdict::Verified) {
        let _ = crate::tasks::auto_close::auto_close_on_report(
            home, "report", &task_id, reporter, &msg.text, true,
        );
    }
}
