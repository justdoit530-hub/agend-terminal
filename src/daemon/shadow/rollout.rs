//! #2413 Phase D — codex rollout-tail observer source.
//!
//! Codex (TUI mode) live-flushes its session to
//! `~/.codex/sessions/<Y>/<M>/<D>/rollout-<ts>-<uuid>.jsonl` DURING a turn (confirm-first
//! verified: `function_call`/`response_item` records appear mid-turn, before
//! `task_complete`). This module is a strictly **READ-ONLY tail** of those files: each
//! appended JSONL record → [`Evidence`] (`authority=Stream`) → the SAME per-agent buffer
//! the reducer already consumes ([`super::push`]). It NEVER writes `~/.codex` and never
//! injects anything — codex produces the file itself, so this is the cleanest plane yet.
//!
//! Parallel to the claude hook plane in `mod.rs`: claude = unix-socket hook ingest
//! (`Authority::Hook`), codex = rollout tail (`Authority::Stream`). The reducer is
//! unchanged — both planes just fill the buffer.
//!
//! Cross-platform (`std::fs` tail; no unix socket), so nothing here is cfg-gated.

use super::evidence::{Evidence, EvidenceKind};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Poll cadence for the tail loop. Codex live-flushes within a turn, so ~1 s gives
/// near-real-time state without busy-spinning the disk.
const TAIL_TICK: std::time::Duration = std::time::Duration::from_secs(1);

/// One codex rollout JSONL record — only the fields we consume (codex emits more).
#[derive(Debug, Deserialize)]
struct RolloutRecord {
    /// ISO-8601 Z record-write time. We prefer it over wall-clock so replayed/lagged
    /// reads still stamp Evidence at the true event time.
    timestamp: Option<String>,
    #[serde(rename = "type")]
    rtype: String,
    #[serde(default)]
    payload: serde_json::Value,
}

/// Map one codex rollout JSONL line → [`Evidence`] (`authority=Stream`). `now_ms` is the
/// fallback stamp when the record carries no parseable timestamp. `None` for records that
/// are not an agent-state transition (`session_meta` / `turn_context` / `user_message` /
/// `developer`+`user` messages / a `token_count` with no rate-limit). PURE — unit-tested
/// against real record shapes, no I/O.
pub(crate) fn record_to_evidence(line: &str, now_ms: u64) -> Option<Evidence> {
    let rec: RolloutRecord = serde_json::from_str(line.trim()).ok()?;
    let at_ms = rec
        .timestamp
        .as_deref()
        .and_then(parse_iso_ms)
        .unwrap_or(now_ms);
    let p = &rec.payload;
    let ptype = p.get("type").and_then(|v| v.as_str());
    let kind = match (rec.rtype.as_str(), ptype) {
        // A turn began / ended.
        ("event_msg", Some("task_started")) => EvidenceKind::TurnStarted,
        ("event_msg", Some("task_complete")) => EvidenceKind::TurnEnded { stop_reason: None },
        // Assistant output is streaming (event-level notification).
        ("event_msg", Some("agent_message")) => EvidenceKind::Responding,
        // A tool (codex `exec_command` / MCP call) started / ended.
        ("response_item", Some("function_call")) => EvidenceKind::ToolStarted {
            name: p.get("name").and_then(|v| v.as_str()).map(str::to_string),
        },
        ("response_item", Some("function_call_output")) => EvidenceKind::ToolEnded,
        // A structured assistant message = responding; user/developer messages are the
        // PROMPT side, not agent state (they fall through to the `_` arm below).
        ("response_item", Some("message"))
            if p.get("role").and_then(|v| v.as_str()) == Some("assistant") =>
        {
            EvidenceKind::Responding
        }
        // Token accounting — also the carrier of codex's `rate_limits` (the bonus claude
        // hooks lack). Surface RateLimited only when a window is actually exhausted;
        // otherwise it's just usage accounting.
        ("event_msg", Some("token_count")) => {
            if let Some(retry_at_ms) = rate_limit_retry_at(p, at_ms) {
                EvidenceKind::RateLimited {
                    retry_at_ms: Some(retry_at_ms),
                }
            } else {
                let (input, output) = token_usage(p);
                EvidenceKind::TokenUsage { input, output }
            }
        }
        _ => return None,
    };
    Some(Evidence::stream(kind, at_ms))
}

/// Extract `(input, output)` token totals from a `token_count` payload's `info`.
fn token_usage(payload: &serde_json::Value) -> (u64, u64) {
    let last = payload.get("info").and_then(|i| i.get("last_token_usage"));
    let g = |k: &str| {
        last.and_then(|u| u.get(k))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
    (g("input_tokens"), g("output_tokens"))
}

/// If a `token_count` payload's `rate_limits` shows a window at/over capacity, return the
/// absolute epoch-ms instant it resets (best-effort). `None` = not currently throttled.
/// Codex reports `rate_limits.{primary,secondary}.{used_percent,resets_in_seconds}`.
fn rate_limit_retry_at(payload: &serde_json::Value, at_ms: u64) -> Option<u64> {
    let rl = payload.get("info").and_then(|i| i.get("rate_limits"))?;
    for win in ["primary", "secondary"] {
        let w = match rl.get(win) {
            Some(w) => w,
            None => continue,
        };
        let used = w
            .get("used_percent")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        if used >= 100.0 {
            let resets = w
                .get("resets_in_seconds")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            return Some(at_ms + resets * 1000);
        }
    }
    None
}

/// Parse an ISO-8601 / RFC-3339 instant (e.g. `2026-06-24T02:59:08.844Z`) → epoch ms.
fn parse_iso_ms(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis().max(0) as u64)
}

/// Extract the session cwd from a `session_meta` record (rollout line 1), normalized for
/// the macOS `/tmp`↔`/private/tmp` symlink so it matches an agend workspace path. `None`
/// if the line isn't a `session_meta` or carries no cwd.
pub(crate) fn session_cwd(line: &str) -> Option<String> {
    let rec: RolloutRecord = serde_json::from_str(line.trim()).ok()?;
    if rec.rtype != "session_meta" {
        return None;
    }
    let cwd = rec.payload.get("cwd")?.as_str()?;
    Some(normalize_path(cwd))
}

/// Normalize a path for cwd↔workspace comparison: strip the macOS `/private` prefix that
/// codex records for `/tmp` paths (`/private/tmp/...` → `/tmp/...`). A no-op elsewhere.
fn normalize_path(p: &str) -> String {
    p.strip_prefix("/private")
        .filter(|rest| rest.starts_with('/'))
        .map(str::to_string)
        .unwrap_or_else(|| p.to_string())
}

/// Map a normalized session cwd → the agend agent that owns it, given the daemon home and
/// the live (name, is_codex) set. An agend agent's workspace is `<home>/workspace/<name>`;
/// we only attribute rollouts whose cwd matches a CODEX agent's workspace (never a stray
/// codex the operator launched outside the fleet).
fn agent_for_cwd(cwd: &str, home: &Path, codex_agents: &[String]) -> Option<String> {
    let ws_root = normalize_path(&home.join("workspace").to_string_lossy());
    for name in codex_agents {
        let ws = format!("{ws_root}/{name}");
        if cwd == ws {
            return Some(name.clone());
        }
    }
    None
}

/// Today's codex rollout directory root (`<CODEX_HOME|~/.codex>/sessions`).
fn codex_sessions_root() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("CODEX_HOME") {
        return Some(PathBuf::from(h).join("sessions"));
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".codex").join("sessions"))
}

/// Per-file tail cursor: byte offset consumed + the agent the rollout is attributed to
/// (resolved once from the session_meta header; `None` until resolved / if not ours).
struct Cursor {
    offset: u64,
    agent: Option<String>,
}

/// Spawn the codex rollout tailer — a fire-and-forget daemon thread (mirrors
/// `api_activity_probe::spawn`). No-op unless [`super::enabled`]. Wired into BOTH
/// `run_core` AND `run_app` (the #2434 lesson: the live fleet daemon is app mode).
pub fn spawn(registry: crate::agent::AgentRegistry, home: PathBuf) {
    if !super::enabled() {
        return;
    }
    // fire-and-forget: a detached read-only tail of codex's own session files. It owns no
    // daemon state, holds no lock across I/O, and exits when the process does. Losing it on
    // shutdown is harmless (next boot re-discovers from the live rollout tail). (§10.5)
    let _ = std::thread::Builder::new()
        .name("shadow-codex-rollout".into())
        .spawn(move || {
            let Some(root) = codex_sessions_root() else {
                tracing::info!(
                    tag = "#shadow-observer",
                    "codex rollout tailer: no HOME/CODEX_HOME — disabled"
                );
                return;
            };
            tracing::info!(tag = "#shadow-observer", root = %root.display(),
                "codex rollout tailer listening (stream plane)");
            let mut cursors: HashMap<PathBuf, Cursor> = HashMap::new();
            loop {
                tail_once(&root, &registry, &home, &mut cursors);
                std::thread::sleep(TAIL_TICK);
            }
        });
}

/// One tail cycle: (re)discover today's rollout files, attribute each to a codex agent via
/// its session_meta cwd, and drain newly-appended records → Evidence → the per-agent
/// buffer. Bounded work; reads only, never under a held lock.
fn tail_once(
    root: &Path,
    registry: &crate::agent::AgentRegistry,
    home: &Path,
    cursors: &mut HashMap<PathBuf, Cursor>,
) {
    let codex_agents = live_codex_agents(registry);
    if codex_agents.is_empty() {
        return;
    }
    for file in discover_rollouts(root) {
        let cur = cursors.entry(file.clone()).or_insert(Cursor {
            offset: 0,
            agent: None,
        });
        drain_file(&file, cur, home, &codex_agents);
    }
}

/// Snapshot the live CODEX agent names (brief registry lock, released before any I/O).
fn live_codex_agents(registry: &crate::agent::AgentRegistry) -> Vec<String> {
    let reg = crate::agent::lock_registry(registry);
    reg.values()
        .filter(|h| h.backend_command.contains("codex"))
        .map(|h| h.name.to_string())
        .collect()
}

/// Today's rollout files under `<root>/<Y>/<M>/<D>/`. (Today only — a long-lived session
/// crossing midnight is rare for an agent and re-discovered as the next day's dir fills;
/// keeping it to today bounds the scan.)
fn discover_rollouts(root: &Path) -> Vec<PathBuf> {
    let now = chrono::Utc::now();
    let dir = root
        .join(now.format("%Y").to_string())
        .join(now.format("%m").to_string())
        .join(now.format("%d").to_string());
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    rd.filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
        })
        .collect()
}

/// Read newly-appended bytes of one rollout file from the cursor offset, attribute it (via
/// the first `session_meta` line) to an agend codex agent, and push each transition as
/// Evidence. A file not owned by any live codex agent is consumed-and-ignored (cursor
/// advances so we don't re-scan it).
fn drain_file(file: &Path, cur: &mut Cursor, home: &Path, codex_agents: &[String]) {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};
    let Ok(f) = std::fs::File::open(file) else {
        return;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len <= cur.offset {
        return; // nothing new (or truncated/rotated — leave for re-discovery)
    }
    let mut reader = BufReader::new(f);
    if reader.seek(SeekFrom::Start(cur.offset)).is_err() {
        return;
    }
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let mut consumed = cur.offset;
    let mut line = String::new();
    loop {
        line.clear();
        let n = match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        // Only act on a COMPLETE line (ends with '\n'); a partial trailing write is left
        // for the next tick (don't advance the cursor past it).
        if !line.ends_with('\n') {
            break;
        }
        consumed += n as u64;
        // Resolve the owning agent from the session_meta header (first line).
        if cur.agent.is_none() {
            if let Some(cwd) = session_cwd(&line) {
                cur.agent = agent_for_cwd(&cwd, home, codex_agents);
            }
            // Either way the header line itself is not a transition.
            continue;
        }
        if let Some(agent) = cur.agent.as_deref() {
            if let Some(ev) = record_to_evidence(&line, now_ms) {
                super::push(agent, ev);
            }
        }
    }
    cur.offset = consumed;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::evidence::{Authority, EvidenceKind};
    use super::*;
    use serial_test::serial;

    fn kind_of(line: &str) -> Option<EvidenceKind> {
        record_to_evidence(line, 1_000).map(|e| e.kind)
    }

    #[test]
    fn maps_turn_lifecycle_and_tools() {
        assert_eq!(
            kind_of(
                r#"{"timestamp":"2026-06-24T02:59:00.000Z","type":"event_msg","payload":{"type":"task_started"}}"#
            ),
            Some(EvidenceKind::TurnStarted)
        );
        assert_eq!(
            kind_of(
                r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","duration_ms":1200}}"#
            ),
            Some(EvidenceKind::TurnEnded { stop_reason: None })
        );
        assert_eq!(
            kind_of(
                r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c1"}}"#
            ),
            Some(EvidenceKind::ToolStarted {
                name: Some("exec_command".into())
            })
        );
        assert_eq!(
            kind_of(
                r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1"}}"#
            ),
            Some(EvidenceKind::ToolEnded)
        );
    }

    #[test]
    fn assistant_message_is_responding_user_is_not() {
        assert_eq!(
            kind_of(
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[]}}"#
            ),
            Some(EvidenceKind::Responding)
        );
        assert_eq!(
            kind_of(r#"{"type":"event_msg","payload":{"type":"agent_message","message":"hi"}}"#),
            Some(EvidenceKind::Responding)
        );
        // User / developer prompt side is NOT agent state.
        assert_eq!(
            kind_of(
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[]}}"#
            ),
            None
        );
        // session_meta / turn_context / user_message → not a transition.
        assert_eq!(kind_of(r#"{"type":"turn_context","payload":{}}"#), None);
        assert_eq!(
            kind_of(r#"{"type":"event_msg","payload":{"type":"user_message","message":"go"}}"#),
            None
        );
    }

    #[test]
    fn token_count_yields_usage_then_ratelimit_when_exhausted() {
        // Normal token_count → TokenUsage with the last-turn totals.
        let u = kind_of(
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":15797,"output_tokens":36}}}}"#,
        );
        assert_eq!(
            u,
            Some(EvidenceKind::TokenUsage {
                input: 15797,
                output: 36
            })
        );
        // A window at 100% used → RateLimited (the codex-only bonus).
        let r = kind_of(
            r#"{"timestamp":"2026-06-24T03:00:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1,"output_tokens":1},"rate_limits":{"primary":{"used_percent":100.0,"resets_in_seconds":60}}}}}"#,
        );
        match r {
            Some(EvidenceKind::RateLimited {
                retry_at_ms: Some(ms),
            }) => {
                // 03:00:00.000Z + 60s
                assert_eq!(
                    ms,
                    parse_iso_ms("2026-06-24T03:00:00.000Z").unwrap() + 60_000
                );
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn evidence_is_stream_authority_with_record_timestamp() {
        let ev = record_to_evidence(
            r#"{"timestamp":"2026-06-24T02:59:08.844Z","type":"event_msg","payload":{"type":"task_started"}}"#,
            9_999,
        )
        .unwrap();
        assert_eq!(ev.authority, Authority::Stream);
        // Stamped at the RECORD time, not the fallback now_ms.
        assert_eq!(ev.at_ms, parse_iso_ms("2026-06-24T02:59:08.844Z").unwrap());
        assert_ne!(ev.at_ms, 9_999);
    }

    #[test]
    fn malformed_or_partial_line_is_none() {
        assert_eq!(record_to_evidence("not json", 1), None);
        assert_eq!(record_to_evidence("", 1), None);
        assert_eq!(record_to_evidence("{}", 1), None);
    }

    #[test]
    fn session_cwd_extracts_and_normalizes_private_tmp() {
        let line = r#"{"type":"session_meta","payload":{"id":"u1","cwd":"/private/tmp/svcx/workspace/cx","originator":"codex-tui"}}"#;
        assert_eq!(session_cwd(line).as_deref(), Some("/tmp/svcx/workspace/cx"));
        // Non-/private paths pass through unchanged.
        let live = r#"{"type":"session_meta","payload":{"cwd":"/Users/x/.agend-terminal/workspace/codex-challenger"}}"#;
        assert_eq!(
            session_cwd(live).as_deref(),
            Some("/Users/x/.agend-terminal/workspace/codex-challenger")
        );
        // A non-session_meta line yields no cwd.
        assert_eq!(session_cwd(r#"{"type":"event_msg","payload":{}}"#), None);
    }

    #[test]
    fn agent_for_cwd_matches_only_fleet_codex_workspace() {
        let home = Path::new("/tmp/svcx");
        let agents = vec!["cx".to_string(), "cx2".to_string()];
        assert_eq!(
            agent_for_cwd("/tmp/svcx/workspace/cx", home, &agents).as_deref(),
            Some("cx")
        );
        // /private-normalized cwd matches the /tmp workspace.
        assert_eq!(
            agent_for_cwd(
                &normalize_path("/private/tmp/svcx/workspace/cx2"),
                home,
                &agents
            )
            .as_deref(),
            Some("cx2")
        );
        // A stray codex outside the fleet workspace is not attributed.
        assert_eq!(
            agent_for_cwd("/Users/x/some/other/dir", home, &agents),
            None
        );
        // An unknown agent name under the workspace root is not attributed.
        assert_eq!(
            agent_for_cwd("/tmp/svcx/workspace/ghost", home, &agents),
            None
        );
    }

    /// Integration: a real on-disk rollout file (session_meta header + a turn) tailed by
    /// `drain_file` resolves the owning agent from the cwd and pushes each transition as
    /// Stream Evidence into the shared buffer. Also pins the partial-trailing-line safety
    /// (a line without a newline is NOT consumed until completed).
    #[test]
    #[serial(shadow_observer)]
    fn drain_file_tails_real_rollout_into_buffer() {
        use std::io::Write;
        let home = std::env::temp_dir().join(format!("agend_rollout_{}", std::process::id()));
        let ws = home.join("workspace").join("cxt");
        std::fs::create_dir_all(&ws).unwrap();
        let cwd = ws.to_string_lossy().to_string();
        let roll = home.join("rollout-test.jsonl");
        let mut f = std::fs::File::create(&roll).unwrap();
        writeln!(
            f,
            r#"{{"type":"session_meta","payload":{{"cwd":"{cwd}","originator":"codex-tui"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"event_msg","payload":{{"type":"task_started"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"response_item","payload":{{"type":"function_call","name":"exec_command"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"event_msg","payload":{{"type":"task_complete","turn_id":"t1"}}}}"#
        )
        .unwrap();
        f.flush().unwrap();

        let mut cur = Cursor {
            offset: 0,
            agent: None,
        };
        let agents = vec!["cxt".to_string()];
        drain_file(&roll, &mut cur, &home, &agents);

        assert_eq!(
            cur.agent.as_deref(),
            Some("cxt"),
            "cwd resolved to the agent"
        );
        let evs = super::super::peek("cxt");
        let kinds: Vec<&EvidenceKind> = evs.iter().map(|e| &e.kind).collect();
        assert!(kinds.contains(&&EvidenceKind::TurnStarted), "{kinds:?}");
        assert!(
            kinds.iter().any(|k| matches!(
                k,
                EvidenceKind::ToolStarted { name } if name.as_deref() == Some("exec_command")
            )),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&&EvidenceKind::TurnEnded { stop_reason: None }),
            "{kinds:?}"
        );
        assert!(
            evs.iter().all(|e| e.authority == Authority::Stream),
            "all Stream authority"
        );
        let off_after = cur.offset;

        // Partial-trailing-line safety: append bytes WITHOUT a newline → not consumed.
        let mut f2 = std::fs::OpenOptions::new()
            .append(true)
            .open(&roll)
            .unwrap();
        write!(
            f2,
            r#"{{"type":"event_msg","payload":{{"type":"task_started"#
        )
        .unwrap();
        f2.flush().unwrap();
        drain_file(&roll, &mut cur, &home, &agents);
        assert_eq!(
            cur.offset, off_after,
            "partial line must not advance the cursor"
        );

        super::super::drain("cxt");
        super::super::forget_agent("cxt");
        let _ = std::fs::remove_dir_all(&home);
    }
}
