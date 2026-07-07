//! #2366 Tier 3: grok sentinel-primary verification telemetry.
//!
//! **Measure-first** — logs disagreements between a clean turn-completion sentinel
//! emit and the screen-scrape heuristic to `sentinel_verifier.jsonl`. Mirrors the
//! `recovery_shadow` / `capture_turn_sentinel_shadow` invariants: append-only,
//! fire-once latch, swallowed failures, zero control-flow effect.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;

use crate::daemon::shadow::evidence::Authority;
use crate::daemon::shadow::gate;
use crate::daemon::shadow::reducer::{ObservedState, ScreenSignal};
use crate::state::AgentState;

/// Side-log only when grok sentinel-primary is enabled for this backend.
pub(crate) fn enabled_for_backend(backend: &str) -> bool {
    crate::backend::Backend::from_command(backend)
        .is_some_and(|b| crate::state::turn_sentinel_primary_enabled(&b))
}

fn last_sig() -> &'static Mutex<HashMap<String, u64>> {
    static S: std::sync::OnceLock<Mutex<HashMap<String, u64>>> = std::sync::OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Signals in scope at the shadow_observe site — no new hot-path work.
pub(crate) struct DisagreementObs<'a> {
    pub agent: &'a str,
    pub backend: &'a str,
    pub sentinel_at_ms: u64,
    pub raw_state: AgentState,
    pub screen: ScreenSignal,
    pub observed_state: ObservedState,
    pub observed_authority: Authority,
}

/// Record ONE disagreement between sentinel idle and screen scrape. `()` → inert.
pub(crate) fn record_disagreement(home: &Path, obs: &DisagreementObs<'_>) {
    if !enabled_for_backend(obs.backend) {
        return;
    }
    let screen_idle = matches!(obs.screen, ScreenSignal::Idle);
    let sentinel_idle = obs.observed_authority == Authority::Sentinel
        && obs.observed_state == ObservedState::Idle;
    let disagrees = screen_idle != sentinel_idle
        || (sentinel_idle && !matches!(obs.raw_state, AgentState::Idle));
    if !disagrees {
        return;
    }

    let sig = {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        obs.agent.hash(&mut h);
        obs.sentinel_at_ms.hash(&mut h);
        obs.raw_state.display_name().hash(&mut h);
        format!("{:?}", obs.screen).hash(&mut h);
        format!("{:?}", obs.observed_state).hash(&mut h);
        format!("{:?}", obs.observed_authority).hash(&mut h);
        h.finish()
    };
    {
        let mut latch = last_sig().lock();
        if latch.get(obs.agent) == Some(&sig) {
            return;
        }
        latch.insert(obs.agent.to_string(), sig);
    }

    let record = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "agent": obs.agent,
        "backend": obs.backend,
        "sentinel_at_ms": obs.sentinel_at_ms,
        "raw_state": obs.raw_state.display_name(),
        "screen_signal": format!("{:?}", obs.screen),
        "screen_as_observed": gate::screen_as_observed(obs.screen)
            .map(|s| format!("{s:?}")),
        "observed_state": format!("{:?}", obs.observed_state),
        "observed_authority": format!("{:?}", obs.observed_authority),
        "sentinel_idle": sentinel_idle,
        "screen_idle": screen_idle,
    });

    let path = home.join("sentinel_verifier.jsonl");
    if let Err(e) = crate::state::append_jsonl(&path, &record) {
        tracing::debug!(
            target: "sentinel_verifier",
            error = %e,
            "#2366 sentinel verifier: append failed (swallowed)"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agend-sentinel-verifier-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn records(home: &std::path::Path) -> Vec<serde_json::Value> {
        let p = home.join("sentinel_verifier.jsonl");
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn non_grok_backend_is_skipped() {
        let home = tmp_home("skip");
        record_disagreement(
            &home,
            &DisagreementObs {
                agent: "dev",
                backend: "claude",
                sentinel_at_ms: 1,
                raw_state: AgentState::Active,
                screen: ScreenSignal::Working,
                observed_state: ObservedState::Idle,
                observed_authority: Authority::Sentinel,
            },
        );
        assert!(records(&home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn grok_disagreement_is_logged_once() {
        let home = tmp_home("grok");
        let obs = DisagreementObs {
            agent: "grok-dev",
            backend: "grok",
            sentinel_at_ms: 42,
            raw_state: AgentState::Active,
            screen: ScreenSignal::Working,
            observed_state: ObservedState::Idle,
            observed_authority: Authority::Sentinel,
        };
        record_disagreement(&home, &obs);
        record_disagreement(&home, &obs);
        let recs = records(&home);
        assert_eq!(recs.len(), 1, "fire-once latch collapses repeats");
        assert_eq!(recs[0]["agent"], "grok-dev");
        assert_eq!(recs[0]["sentinel_idle"], true);
        assert_eq!(recs[0]["screen_idle"], false);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn agreeing_idle_is_silent() {
        let home = tmp_home("agree");
        record_disagreement(
            &home,
            &DisagreementObs {
                agent: "grok-dev",
                backend: "grok",
                sentinel_at_ms: 1,
                raw_state: AgentState::Idle,
                screen: ScreenSignal::Idle,
                observed_state: ObservedState::Idle,
                observed_authority: Authority::Sentinel,
            },
        );
        assert!(records(&home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }
}