//! Daemon-side MCP tool dispatch.
//!
//! Sprint 56 Track I-Phase2c (#531): the stdio JSON-RPC server moved to
//! `src/bin/agend-mcp-bridge.rs` (the canonical bridge binary). This
//! module retains only the daemon-internal tool dispatcher
//! (`execute_tool`) plus the `handlers` and `tools` submodules used by
//! the bridge proxy and `api/handlers/mcp_proxy.rs`.

pub mod handlers;
pub(crate) mod registry;
pub mod tools;
pub mod usage_stats;

use serde_json::Value;

/// Service boundary: single public entry point for MCP tool execution
/// without a live RuntimeContext (tests / standalone bridge fallback).
/// Production daemon path prefers [`execute_tool_with_runtime`].
#[cfg_attr(not(test), allow(dead_code))]
pub fn execute_tool(tool_name: &str, args: &Value, instance_name: &str) -> Value {
    handlers::handle_tool(tool_name, args, instance_name)
}

/// #2454: execute with a live daemon RuntimeContext (in-process, no socket loopback).
pub fn execute_tool_with_runtime(
    tool_name: &str,
    args: &Value,
    instance_name: &str,
    runtime: handlers::dispatch::RuntimeContext,
) -> Value {
    handlers::handle_tool_with_runtime(tool_name, args, instance_name, Some(runtime))
}
