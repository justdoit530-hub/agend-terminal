use super::{PerTickHandler, TickContext};

pub(crate) struct ContextWatermarkHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
}

impl ContextWatermarkHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
        }
    }
}

impl PerTickHandler for ContextWatermarkHandler {
    fn name(&self) -> &'static str {
        "context_watermark"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }

        let reg = crate::agent::lock_registry(ctx.registry);
        for handle in reg.values() {
            let name = &handle.name;
            let mut core = handle.core.lock();
            let reading = core.state.resolved_context(Some(ctx.home));
            if let Some((pct, _)) = reading {
                if pct > 80.0 {
                    core.health
                        .set_blocked_reason(crate::health::BlockedReason::ContextHigh);
                    core.health
                        .set_blocked_note(Some("context > 80%: consider compacting".to_string()));
                    tracing::warn!(agent = %name, pct, "context_watermark: context high alert issued");
                } else {
                    if let Some(crate::health::BlockedReason::ContextHigh) =
                        core.health.current_reason
                    {
                        core.health.clear_blocked_reason();
                        tracing::info!(agent = %name, pct, "context_watermark: context high alert cleared");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex as PLMutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn test_context_watermark_warning_issued_and_cleared() {
        let home = std::env::temp_dir().join(format!("agend-ctxwatermark-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();

        let registry: crate::agent::AgentRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let externals: crate::agent::ExternalRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let configs: Arc<PLMutex<HashMap<String, crate::daemon::AgentConfig>>> =
            Arc::new(PLMutex::new(HashMap::new()));

        let (handle, _reader) = crate::daemon::per_tick::mock_live_agent_no_context("test-worker");
        let core = Arc::clone(&handle.core);

        // 1. Set context_pct to 85% (> 80.0)
        {
            let mut c = core.lock();
            c.state.set_context_pct_for_test(85.0);
        }

        registry.lock().insert(handle.id, handle);

        let handler = ContextWatermarkHandler::new(1);
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        // Run handler -> Warning should be set
        handler.run(&ctx);

        {
            let c = core.lock();
            assert_eq!(
                c.health.current_reason,
                Some(crate::health::BlockedReason::ContextHigh)
            );
            assert_eq!(
                c.health.current_note,
                Some("context > 80%: consider compacting".to_string())
            );
        }

        // 2. Set context_pct to 75% (<= 80.0)
        {
            let mut c = core.lock();
            c.state.set_context_pct_for_test(75.0);
        }

        // Run handler -> Warning should be cleared
        handler.run(&ctx);

        {
            let c = core.lock();
            assert!(c.health.current_reason.is_none());
            assert!(c.health.current_note.is_none());
        }

        std::fs::remove_dir_all(&home).ok();
    }
}
