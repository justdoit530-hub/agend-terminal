//! #2454 Slice 2: in-process bridges from MCP handlers to the shared API
//! service owners. Avoids MCP → socket → API self-IPC when a live
//! [`RuntimeContext`] is available.

use super::dispatch::RuntimeContext;
use crate::api::handlers::{self, HandlerCtx};
use serde_json::{json, Value};
use std::path::Path;

/// Build an API [`HandlerCtx`] from the MCP runtime registries.
pub(super) fn api_ctx<'a>(home: &'a Path, runtime: &'a RuntimeContext) -> HandlerCtx<'a> {
    HandlerCtx {
        registry: &runtime.registry,
        configs: &runtime.configs,
        externals: &runtime.externals,
        notifier: runtime.notifier.clone(),
        home,
    }
}

/// Map an API SEND response into the MCP tool response shape used by comms.
pub(super) fn map_send_ok(resp: &Value, target: &str) -> Value {
    if resp["ok"].as_bool() == Some(true) {
        let dm = resp["delivery_mode"].as_str().unwrap_or("pty");
        let mut out = json!({"target": target, "delivery_mode": dm});
        // Surface daemon-created task id when present (delegate auto-create).
        if let Some(tid) = resp.get("task_id").cloned() {
            if let Some(obj) = out.as_object_mut() {
                obj.insert("auto_created_task_id".into(), tid);
            }
        }
        if let Some(branch) = resp.get("branch_checked_out").cloned() {
            if let Some(obj) = out.as_object_mut() {
                obj.insert("branch_checked_out".into(), branch);
            }
        }
        out
    } else {
        json!({"error": resp["error"].as_str().unwrap_or("send failed")})
    }
}

/// #2454: deliver SEND in-process. `runtime=None` → explicit error (no socket
/// loopback, no silent inbox fallback that bypasses policy gates).
pub(super) fn send_in_process(
    home: &Path,
    runtime: Option<&RuntimeContext>,
    params: &Value,
    target: &str,
) -> Value {
    let Some(runtime) = runtime else {
        return json!({
            "error": "runtime unavailable: send requires the in-process daemon runtime"
        });
    };
    let ctx = api_ctx(home, runtime);
    let resp = handlers::messaging::handle_send(params, &ctx);
    map_send_ok(&resp, target)
}

/// #2454: SPAWN in-process.
pub(super) fn spawn_in_process(
    home: &Path,
    runtime: Option<&RuntimeContext>,
    params: &Value,
) -> Result<Value, String> {
    let Some(runtime) = runtime else {
        return Err(
            "runtime unavailable: spawn requires the in-process daemon runtime".to_string(),
        );
    };
    let ctx = api_ctx(home, runtime);
    Ok(handlers::instance::handle_spawn(params, &ctx))
}

/// #2454: DELETE (daemon-side) in-process.
pub(super) fn delete_in_process(
    home: &Path,
    runtime: Option<&RuntimeContext>,
    name: &str,
    no_wait: bool,
) -> Result<Value, String> {
    if let Some(runtime) = runtime {
        let ctx = api_ctx(home, runtime);
        let params = json!({"name": name, "no_wait": no_wait});
        return Ok(handlers::instance::handle_delete(&params, &ctx));
    }
    if let Some(reg) = crate::agent::get_pending_registry() {
        let configs =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let externals =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let ctx = HandlerCtx {
            registry: &reg,
            configs: &configs,
            externals: &externals,
            notifier: None,
            home,
        };
        let params = json!({"name": name, "no_wait": no_wait});
        return Ok(handlers::instance::handle_delete(&params, &ctx));
    }
    Err(
        "runtime unavailable: delete requires the in-process daemon runtime or registry"
            .to_string(),
    )
}

/// #2454: CREATE_TEAM in-process.
pub(super) fn create_team_in_process(
    home: &Path,
    runtime: Option<&RuntimeContext>,
    params: &Value,
) -> Result<Value, String> {
    let Some(runtime) = runtime else {
        return Err(
            "runtime unavailable: create_team requires the in-process daemon runtime".to_string(),
        );
    };
    let ctx = api_ctx(home, runtime);
    Ok(handlers::team::handle_create_team(params, &ctx))
}

/// #2454: UPDATE_TEAM in-process.
pub(super) fn update_team_in_process(
    home: &Path,
    runtime: Option<&RuntimeContext>,
    params: &Value,
) -> Result<Value, String> {
    let Some(runtime) = runtime else {
        return Err(
            "runtime unavailable: update_team requires the in-process daemon runtime".to_string(),
        );
    };
    let ctx = api_ctx(home, runtime);
    Ok(handlers::team::handle_update_team(params, &ctx))
}

/// #2454: INJECT in-process via shared `agent_ops::inject_input`.
#[allow(dead_code)] // available for future MCP inject tool paths
pub(super) fn inject_in_process(
    home: &Path,
    runtime: Option<&RuntimeContext>,
    name: &str,
    data: &str,
) -> Result<usize, String> {
    let Some(runtime) = runtime else {
        return Err(
            "runtime unavailable: inject requires the in-process daemon runtime".to_string(),
        );
    };
    crate::agent_ops::inject_input(
        &runtime.registry,
        &runtime.externals,
        home,
        name,
        data,
        false,
    )
    .map_err(|e| e.message())
}

/// #2454: LIST snapshot in-process (agents array from shared service).
pub(super) fn list_agents_in_process(
    home: &Path,
    runtime: Option<&RuntimeContext>,
) -> Result<Value, String> {
    let Some(runtime) = runtime else {
        return Err("runtime unavailable: list requires the in-process daemon runtime".to_string());
    };
    Ok(crate::agent_ops::list_snapshot(
        home,
        &runtime.registry,
        &runtime.externals,
    ))
}
