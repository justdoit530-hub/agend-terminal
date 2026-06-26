//! #2413 Phase D — grok session-tail observer source.
//!
//! Grok CLI writes lifecycle events to
//! `{GROK_HOME or ~/.grok}/sessions/<url-encoded-cwd>/<uuid>/events.jsonl` (append) alongside
//! a `summary.json` sidecar (which carries the session `cwd` at `info.cwd`). This module is a
//! strictly **READ-ONLY tail** of `events.jsonl`: each appended line → [`Evidence`]
//! (`authority=Stream`) → the SAME per-agent buffer the reducer consumes ([`super::push`]).
//! It NEVER writes `~/.grok` and never injects anything — grok produces the files itself.
//!
//! Mirrors the kiro plane (`kiro.rs`); the reducer + Evidence schema are unchanged. Grok-specific
//! deltas (confirm-first research, 2026-06-25):
//! - **nested dir** (`sessions/<encoded-cwd>/<uuid>/events.jsonl`), not kiro's flat uuid dir.
//! - **attribution via sibling `summary.json` → `info.cwd`** (kiro used `<uuid>.json`).
//! - **richer lifecycle** (`turn_started` / `first_token` / `tool_started` mid-turn flush).
//!
//! ⚠ CAVEAT: `phase_changed` is extremely noisy (1000+ lines per session for streaming
//! chunks). Only `permission_prompt` maps to evidence; all other phases are ignored.
//!
//! Cross-platform (`std::fs` tail; no unix socket), so nothing here is cfg-gated except the
//! macOS `/private` reconciliation (unix-only), mirroring `kiro.rs`.

use super::evidence::{Evidence, EvidenceKind};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Poll cadence for the tail loop. grok live-flushes `events.jsonl` mid-turn, so ~1 s gives
/// near-real-time state without busy-spinning the disk. (Same as kiro `rollout`.)
const TAIL_TICK: std::time::Duration = std::time::Duration::from_secs(1);

/// Only tail session files modified within this window — skips dormant old sessions while
/// still catching a live one being appended.
const DISCOVER_RECENT: std::time::Duration = std::time::Duration::from_secs(26 * 3600);

/// One grok `events.jsonl` line — only the fields we consume.
#[derive(Debug, Deserialize)]
struct GrokRecord {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    phase: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
}

/// Map one grok `events.jsonl` line → [`Evidence`] (`authority=Stream`). `now_ms` is the
/// fallback stamp when the line carries no parseable `ts`. `None` for a line that is not an
/// agent-state transition. PURE — unit-tested against real line shapes, no I/O.
pub(crate) fn record_to_evidence(line: &str, now_ms: u64) -> Option<Evidence> {
    let rec: GrokRecord = serde_json::from_str(line.trim()).ok()?;
    let at_ms = rec.ts.as_deref().and_then(parse_iso_ms).unwrap_or(now_ms);
    let kind = match rec.event_type.as_str() {
        "turn_started" => EvidenceKind::TurnStarted,
        "first_token" => EvidenceKind::Responding,
        "tool_started" => EvidenceKind::ToolStarted {
            name: rec.tool_name,
        },
        "tool_completed" => EvidenceKind::ToolEnded,
        "turn_ended" => EvidenceKind::TurnEnded {
            stop_reason: rec.outcome,
        },
        "permission_requested" => EvidenceKind::ApprovalRequired,
        "phase_changed" => match rec.phase.as_deref() {
            Some("permission_prompt") => EvidenceKind::ApprovalRequired,
            // `waiting_for_model` is optional Thinking signal — reducer infers Thinking from
            // TurnStarted + silence; skip the noisy streaming_* phases entirely.
            _ => return None,
        },
        _ => return None,
    };
    Some(Evidence::stream(kind, at_ms))
}

/// Parse an ISO-8601 / RFC-3339 instant (e.g. `2026-06-25T05:04:44.491Z`) → epoch ms.
fn parse_iso_ms(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis().max(0) as u64)
}

/// Read the session `cwd` from an `events.jsonl`'s sibling `summary.json`. `None` if the
/// sidecar is missing (it may briefly lag the `.jsonl`) or carries no cwd — the caller retries.
pub(crate) fn sidecar_cwd(events_jsonl: &Path) -> Option<String> {
    let summary = events_jsonl.parent()?.join("summary.json");
    let txt = std::fs::read_to_string(summary).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    Some(v.get("info")?.get("cwd")?.as_str()?.to_string())
}

/// Map a session cwd → the agend grok agent that owns it. SCOPED + separator-agnostic:
/// the cwd must EQUAL `<home>/workspace/<name>` for a LIVE grok agent, compared by path
/// COMPONENTS (`Path` eq handles `\` vs `/` for Windows) AND rooted at THIS daemon's `home`
/// (a stray `*/workspace/<name>` outside the fleet is NOT attributed). Identical to kiro
/// `agent_for_cwd`. `/tmp`→`/private/tmp` macOS canonicalization reconciled by
/// [`strip_private`].
fn agent_for_cwd(cwd: &str, home: &Path, grok_agents: &[String]) -> Option<String> {
    let cwd_path = Path::new(strip_private(cwd));
    let ws = home.join("workspace");
    grok_agents
        .iter()
        .find(|name| cwd_path == ws.join(name))
        .cloned()
}

/// Strip macOS `/private` canonicalization (`/private/tmp/...` → `/tmp/...`) so a
/// `/tmp`-rooted daemon home matches. Unix-only (Windows has no such prefix). No-op else.
#[cfg(unix)]
fn strip_private(p: &str) -> &str {
    match p.strip_prefix("/private") {
        Some(rest) if rest.starts_with('/') => rest,
        _ => p,
    }
}
#[cfg(not(unix))]
fn strip_private(p: &str) -> &str {
    p
}

/// grok's session root (`{GROK_HOME or ~/.grok}/sessions`).
fn grok_sessions_root() -> Option<PathBuf> {
    let base = std::env::var("GROK_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".grok"))
        })?;
    Some(base.join("sessions"))
}

/// Per-file tail cursor: byte offset consumed + the agent the session is attributed to
/// (resolved once from `summary.json` cwd; `None` until resolved / not ours).
struct Cursor {
    offset: u64,
    agent: Option<String>,
}

/// Spawn the grok session tailer — a fire-and-forget daemon thread (mirrors
/// `kiro::spawn` / `rollout::spawn`). No-op unless [`super::enabled`]. Wired into BOTH
/// `run_core` AND `run_app` (the #2434 lesson: the live fleet daemon is app mode).
pub fn spawn(registry: crate::agent::AgentRegistry, home: PathBuf) {
    if !super::enabled() {
        return;
    }
    // fire-and-forget: a detached read-only tail of grok's own session files. It owns no
    // daemon state, holds no lock across I/O, and exits when the process does. Losing it on
    // shutdown is harmless (next boot re-discovers from the live session tail). (§10.5)
    let _ = std::thread::Builder::new()
        .name("shadow-grok-tail".into())
        .spawn(move || {
            let Some(root) = grok_sessions_root() else {
                tracing::info!(
                    tag = "#shadow-observer",
                    "grok session tailer: no GROK_HOME/HOME — disabled"
                );
                return;
            };
            tracing::info!(tag = "#shadow-observer", root = %root.display(),
                "grok session tailer listening (stream plane)");
            let mut cursors: HashMap<PathBuf, Cursor> = HashMap::new();
            loop {
                tail_once(&root, &registry, &home, &mut cursors);
                std::thread::sleep(TAIL_TICK);
            }
        });
}

/// One tail cycle: (re)discover recent session files, attribute each via `summary.json`
/// cwd, and drain newly-appended lines → Evidence → the per-agent buffer.
fn tail_once(
    root: &Path,
    registry: &crate::agent::AgentRegistry,
    home: &Path,
    cursors: &mut HashMap<PathBuf, Cursor>,
) {
    let grok_agents = live_grok_agents(registry);
    if grok_agents.is_empty() {
        return;
    }
    for file in discover_sessions(root) {
        let cur = cursors.entry(file.clone()).or_insert(Cursor {
            offset: 0,
            agent: None,
        });
        drain_file(&file, cur, home, &grok_agents);
    }
}

/// Snapshot the live grok agent names (brief registry lock, released before any I/O).
fn live_grok_agents(registry: &crate::agent::AgentRegistry) -> Vec<String> {
    let reg = crate::agent::lock_registry(registry);
    reg.values()
        .filter(|h| h.backend_command.contains("grok"))
        .map(|h| h.name.to_string())
        .collect()
}

/// Recently-modified `events.jsonl` files under `sessions/<encoded-cwd>/<uuid>/`.
fn discover_sessions(root: &Path) -> Vec<PathBuf> {
    let recent_cutoff = std::time::SystemTime::now()
        .checked_sub(DISCOVER_RECENT)
        .unwrap_or(std::time::UNIX_EPOCH);
    let mut out = Vec::new();
    let Ok(cwd_dirs) = std::fs::read_dir(root) else {
        return out;
    };
    for cwd_ent in cwd_dirs.flatten() {
        let cwd_path = cwd_ent.path();
        if !cwd_path.is_dir() {
            continue;
        }
        let Ok(uuid_dirs) = std::fs::read_dir(&cwd_path) else {
            continue;
        };
        for uuid_ent in uuid_dirs.flatten() {
            let events = uuid_ent.path().join("events.jsonl");
            if !events.is_file() {
                continue;
            }
            let fresh = std::fs::metadata(&events)
                .and_then(|m| m.modified())
                .map(|m| m >= recent_cutoff)
                .unwrap_or(true);
            if fresh {
                out.push(events);
            }
        }
    }
    out
}

/// Read newly-appended lines of one session `events.jsonl` from the cursor, attribute it
/// (via `summary.json` cwd) to a grok agent, and push each transition as Evidence.
/// Attribution is retried each tick until the sidecar resolves — and the cursor is NOT
/// advanced until an owning agent is resolved, so no lines are lost to a sidecar/registration
/// race (a session whose cwd is not a live fleet grok agent simply re-checks cheaply and
/// never advances).
fn drain_file(file: &Path, cur: &mut Cursor, home: &Path, grok_agents: &[String]) {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};
    if cur.agent.is_none() {
        match sidecar_cwd(file) {
            Some(cwd) => cur.agent = agent_for_cwd(&cwd, home, grok_agents),
            None => return,
        }
        if cur.agent.is_none() {
            return;
        }
    }
    let Ok(f) = std::fs::File::open(file) else {
        return;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len < cur.offset {
        cur.offset = 0;
    }
    if len <= cur.offset {
        return;
    }
    let mut reader = BufReader::new(f);
    if reader.seek(SeekFrom::Start(cur.offset)).is_err() {
        return;
    }
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let mut consumed = cur.offset;
    let mut line = String::new();
    let agent = match cur.agent.as_deref() {
        Some(a) => a,
        None => return,
    };
    loop {
        line.clear();
        let n = match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if !line.ends_with('\n') {
            break;
        }
        consumed += n as u64;
        if let Some(ev) = record_to_evidence(&line, now_ms) {
            super::push(agent, ev);
        }
    }
    cur.offset = consumed;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::evidence::{Authority, EvidenceKind};
    use super::*;

    fn kind_of(line: &str) -> Option<EvidenceKind> {
        record_to_evidence(line, 1_000).map(|e| e.kind)
    }

    #[test]
    fn maps_grok_lifecycle_events() {
        assert_eq!(
            kind_of(
                r#"{"ts":"2026-06-25T05:04:44.491Z","type":"turn_started","session_id":"019efd2a-8855-75f2-8d5b-eae114546887","turn_number":0}"#
            ),
            Some(EvidenceKind::TurnStarted)
        );
        assert_eq!(
            kind_of(r#"{"ts":"2026-06-25T05:04:46.287Z","type":"first_token"}"#),
            Some(EvidenceKind::Responding)
        );
        assert_eq!(
            kind_of(
                r#"{"ts":"2026-06-25T05:04:46.497Z","type":"tool_started","tool_name":"Glob"}"#
            ),
            Some(EvidenceKind::ToolStarted {
                name: Some("Glob".into())
            })
        );
        assert_eq!(
            kind_of(
                r#"{"ts":"2026-06-25T05:04:46.507Z","type":"tool_completed","tool_name":"Read","outcome":"success"}"#
            ),
            Some(EvidenceKind::ToolEnded)
        );
        assert_eq!(
            kind_of(
                r#"{"ts":"2026-06-25T05:06:41.597Z","type":"turn_ended","outcome":"completed"}"#
            ),
            Some(EvidenceKind::TurnEnded {
                stop_reason: Some("completed".into())
            })
        );
        assert_eq!(
            kind_of(
                r#"{"ts":"2026-06-25T05:04:46.498Z","type":"permission_requested","tool_name":"Glob"}"#
            ),
            Some(EvidenceKind::ApprovalRequired)
        );
        assert_eq!(
            kind_of(
                r#"{"ts":"2026-06-25T05:04:44.552Z","type":"phase_changed","phase":"permission_prompt"}"#
            ),
            Some(EvidenceKind::ApprovalRequired)
        );
    }

    #[test]
    fn noisy_phase_changed_and_unknown_types_are_none() {
        assert_eq!(
            kind_of(
                r#"{"ts":"2026-06-25T05:04:44.552Z","type":"phase_changed","phase":"streaming_reasoning"}"#
            ),
            None
        );
        assert_eq!(
            kind_of(
                r#"{"ts":"2026-06-25T05:04:44.552Z","type":"phase_changed","phase":"waiting_for_model"}"#
            ),
            None
        );
        assert_eq!(kind_of(r#"{"type":"mcp_server_connected"}"#), None);
        assert_eq!(kind_of("not json"), None);
        assert_eq!(kind_of(""), None);
    }

    #[test]
    fn evidence_is_stream_authority_at_ts() {
        let ev = record_to_evidence(
            r#"{"ts":"2026-06-25T05:04:44.491Z","type":"turn_started"}"#,
            9_999,
        )
        .unwrap();
        assert_eq!(ev.authority, Authority::Stream);
        assert_eq!(ev.at_ms, parse_iso_ms("2026-06-25T05:04:44.491Z").unwrap());
        assert_ne!(ev.at_ms, 9_999);
        let ev2 = record_to_evidence(r#"{"type":"tool_completed"}"#, 9_999).unwrap();
        assert_eq!(ev2.at_ms, 9_999);
    }

    #[test]
    fn sidecar_cwd_reads_summary_json() {
        let dir = std::env::temp_dir().join(format!("grok_sidecar_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let events = dir.join("events.jsonl");
        std::fs::write(&events, "").unwrap();
        assert_eq!(sidecar_cwd(&events), None);
        std::fs::write(
            dir.join("summary.json"),
            serde_json::json!({
                "info": {"id": "abc", "cwd": "/Users/x/proj"}
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(sidecar_cwd(&events).as_deref(), Some("/Users/x/proj"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
