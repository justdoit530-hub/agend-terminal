use super::InjectTarget;

const READBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const READBACK_POLL: std::time::Duration = std::time::Duration::from_millis(15);
const POSTSUBMIT_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);
/// Bottom rows scanned for the input area — the prompt + a few wrapped input rows.
const READBACK_TAIL_ROWS: usize = 8;

/// #1912: tail-sentinel of a (possibly multi-line) injected payload — the last
/// run of up to `MAX` chars on the final non-empty line, the line the submit `\r`
/// commits. Short + drawn from the bottom line so it stays robust to input-box
/// wrapping. Empty when the payload has no non-blank line (nothing to confirm).
pub(super) fn inject_sentinel(stripped: &str) -> String {
    const MAX: usize = 24;
    let last_line = stripped
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let take_from = last_line
        .char_indices()
        .rev()
        .take(MAX)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(0);
    last_line[take_from..]
        .chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .collect()
}

/// #1912/#3175: poll until the typed line's tail sentinel is a suffix of the
/// visible text ending exactly at the cursor. The VTerm matcher walks backwards
/// only over proven `WRAPLINE` boundaries, so an older row, scrollback, or text
/// after the cursor cannot authorize submit. Returns `false` on timeout; the
/// caller leaves the first write untouched and never retries or submits it.
/// Acquires the core lock only briefly per poll so `pty_read_loop` can render the
/// backend's echo between polls.
pub(super) fn readback_confirm_typed(target: &InjectTarget, stripped: &str) -> bool {
    readback_confirm_typed_with(target, stripped, READBACK_TIMEOUT, READBACK_POLL)
}

pub(super) fn readback_confirm_typed_with(
    target: &InjectTarget,
    stripped: &str,
    timeout: std::time::Duration,
    poll: std::time::Duration,
) -> bool {
    let sentinel = inject_sentinel(stripped);
    if sentinel.is_empty() {
        return false;
    }
    let max_rows = unicode_width::UnicodeWidthStr::width(sentinel.as_str()).max(1);
    let start = std::time::Instant::now();
    let mut polls = 0u32;
    loop {
        if target.deleted.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        let confirmed = target
            .core
            .lock()
            .vterm
            .cursor_anchored_suffix_matches(&sentinel, max_rows);
        if confirmed {
            tracing::debug!(
                tag = "#1912-readback-confirmed",
                polls,
                elapsed_ms = start.elapsed().as_millis() as u64,
                "typed inject line rendered in input area before submit"
            );
            return true;
        }
        if start.elapsed() >= timeout {
            tracing::warn!(
                tag = "#1912-readback-timeout",
                elapsed_ms = start.elapsed().as_millis() as u64,
                sentinel_len = sentinel.len(),
                "typed inject line not confirmed in input area within timeout"
            );
            return false;
        }
        std::thread::sleep(poll);
        polls += 1;
    }
}

/// Byte length above which a typed inject is spilled to a temp file and the
/// PTY receives a short "read this path" instruction instead of the payload.
///
/// 2048 is a starting point from the multi-KB spec injects that latched
/// `typed_inject_contaminated`. The readback window cannot cover a multi-KB
/// chunked type-in. Retune from measurement; this is not a protocol constant.
/// A payload of exactly this many bytes stays inline — only a greater length
/// spills.
pub(super) const TYPED_INJECT_FILE_FALLBACK_BYTES: usize = 2048;

/// Temp-file prefix. Same system-temp convention as `image_paste`'s
/// `agend_paste_` files — no new storage root.
const TYPED_INJECT_FILE_PREFIX: &str = "agend_inject_";

/// How long a spilled payload stays in the temp dir. The agent reads it within
/// seconds; this only bounds leftover files from later spills.
const TYPED_INJECT_FILE_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// One line, so readback-confirm's last-line sentinel still applies. `{path}`
/// is the absolute spill path.
const TYPED_INJECT_FILE_INSTRUCTION: &str = "[AGEND-INJECT-FILE] Read this file and carry out the full message inside it. Do not ask for a paste. Path: {path}";

static SPILL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Pre-flight size check used by `inject_with_target` before a typed inject.
/// Payloads at or under the threshold are returned unchanged. Larger payloads
/// are written under the system temp dir and replaced with a short instruction.
pub(super) fn typed_inject_text_for_delivery(payload: &str) -> std::io::Result<String> {
    typed_inject_text_for_delivery_in(&std::env::temp_dir(), payload)
}

/// Same as [`typed_inject_text_for_delivery`], but the spill directory is
/// injectable so the unit test does not share the process temp dir.
pub(super) fn typed_inject_text_for_delivery_in(
    dir: &std::path::Path,
    payload: &str,
) -> std::io::Result<String> {
    if payload.len() <= TYPED_INJECT_FILE_FALLBACK_BYTES {
        return Ok(payload.to_string());
    }
    let path = spill_typed_payload(dir, payload)?;
    let instruction = file_fallback_instruction(&path);
    if instruction.len() > TYPED_INJECT_FILE_FALLBACK_BYTES {
        let _ = std::fs::remove_file(&path);
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "typed-inject file fallback instruction exceeded the size threshold",
        ));
    }
    Ok(instruction)
}

fn file_fallback_instruction(path: &std::path::Path) -> String {
    TYPED_INJECT_FILE_INSTRUCTION.replace("{path}", &path.display().to_string())
}

fn spill_typed_payload(
    dir: &std::path::Path,
    payload: &str,
) -> std::io::Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    sweep_stale_inject_files(dir);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = SPILL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = dir.join(format!(
        "{TYPED_INJECT_FILE_PREFIX}{}_{seq}_{nanos}.txt",
        std::process::id()
    ));
    std::fs::write(&path, payload)?;
    Ok(path)
}

/// Best-effort. A temp-dir sweep must not fail the inject.
fn sweep_stale_inject_files(dir: &std::path::Path) {
    let now = std::time::SystemTime::now();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !(name.starts_with(TYPED_INJECT_FILE_PREFIX) && name.ends_with(".txt")) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        let stale = now
            .duration_since(modified)
            .map(|age| age > TYPED_INJECT_FILE_TTL)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// #1912: post-submit observability (no retry). A successful submit clears the
/// input line / grows the transcript, so the rendered tail CHANGES; returns `true`
/// the moment it does. If it stays byte-identical for `POSTSUBMIT_WINDOW`, the
/// submit likely didn't take — warn (log/metric only) and return `false`. NEVER
/// retries the submit: a second `\r` would risk double-submit.
pub(super) fn observe_post_submit(target: &InjectTarget) -> bool {
    observe_post_submit_with(target, POSTSUBMIT_WINDOW, READBACK_POLL)
}

pub(super) fn observe_post_submit_with(
    target: &InjectTarget,
    window: std::time::Duration,
    poll: std::time::Duration,
) -> bool {
    let before = target.core.lock().vterm.tail_lines(READBACK_TAIL_ROWS);
    let start = std::time::Instant::now();
    loop {
        if target.deleted.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        std::thread::sleep(poll);
        if target.core.lock().vterm.tail_lines(READBACK_TAIL_ROWS) != before {
            return true;
        }
        if start.elapsed() >= window {
            tracing::warn!(
                tag = "#1912-postsubmit-nochange",
                "input area unchanged after submit — a readback-confirmed line may not have submitted"
            );
            return false;
        }
    }
}
