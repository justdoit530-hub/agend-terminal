//! MCP tool dispatch table — #694 BLOCK 2.
//!
//! `handle_tool` (in `mod.rs`) historically routed 30+ MCP tools through
//! a 143-line `match` literal. This module introduces a linear-scan
//! dispatch table so tools can register their handlers as data instead
//! of as match arms. Adding a tool becomes "append an entry"; un-
//! migrated tools fall through to the (shrinking) inline match.
//!
//! **Signature design** — the 30+ arms have at least four distinct
//! handler shapes (`(home, args, instance)`, `(home, args)`,
//! `(home, args, sender)`, `(home, args, instance, sender)`). Rather
//! than commit to one shape, this module uses a single uniform
//! [`HandlerFn`] keyed on a [`HandlerCtx`] struct that bundles every
//! common parameter. Each migrated tool gets a tiny adapter fn that
//! pulls the fields it needs out of `HandlerCtx`.
//!
//! **Linear scan** — `<10ns` for 30 entries vs `~50ns` allocator hit
//! for HashMap/phf, and the table size is bounded by the MCP tool
//! catalogue, so static-search is cheap and avoids the deps.
//!
//! **Fallback in mod.rs** — [`try_dispatch`] returns `Option<Value>`
//! (`None` = "tool name not in table"). `handle_tool` falls back to the
//! existing inline match for un-migrated arms; the catch-all
//! `unknown tool` branch in that match still handles fully-unknown
//! names.

use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use super::{
    binding_state, channel, ci, comms, ephemeral, force_release, gc, instance, restart, schedule,
    task, worktree,
};

/// Shared per-call context — every common parameter `handle_tool`
/// would otherwise pass into the match arms, bundled together so each
/// [`HandlerFn`] has a single uniform shape.
pub(crate) struct HandlerCtx<'a> {
    pub home: &'a Path,
    pub args: &'a Value,
    pub instance_name: &'a str,
    pub sender: &'a Option<Sender>,
    /// #2454: live daemon runtime when entered via in-process mcp_tool.
    /// Standalone bridge / tests leave this `None`.
    pub runtime: Option<&'a RuntimeContext>,
}

/// Optional daemon runtime state available only when MCP tools are executed
/// through the in-process API `mcp_tool` path. Standalone bridge calls leave it
/// absent and keep legacy socket/fallback behavior where still present.
#[derive(Clone)]
pub(crate) struct RuntimeContext {
    pub registry: crate::agent::AgentRegistry,
    /// Reserved for later #2454 slices (delete/spawn share configs).
    #[allow(dead_code)]
    pub configs: crate::api::ConfigRegistry,
    pub externals: crate::agent::ExternalRegistry,
    pub notifier: Option<std::sync::Arc<dyn crate::api::ApiNotifier>>,
}

/// One MCP tool's dispatcher. Function pointer (not `Box<dyn …>`) so
/// the slice in [`registered_handlers`] is `const`-friendly and
/// allocation-free.
pub(crate) type HandlerFn = fn(&HandlerCtx<'_>) -> Value;

/// #1602: validate `args` against the tool's declared `inputSchema` before
/// dispatch. The schemas declare `required[]` but nothing enforced it, so a
/// mis-named / omitted param failed LATE and misleadingly (the operator's
/// `reply` bug: a wrong key silently became an empty `text`). Now:
///
/// - a missing REQUIRED key → hard-reject with `<tool>: missing required
///   parameter '<name>'`;
/// - an UNKNOWN key → warn only (forward-compat; never reject).
///
/// Returns `Some(error_value)` to short-circuit dispatch, or `None` to proceed.
///
/// **Enforceability invariant (#1602/#1603):** every field a tool declares in
/// `required[]` MUST be one the handler genuinely needs — i.e. the handler
/// ERRORS (not defaults) when it's absent. The systematic audit found 5 tools
/// whose handler instead DEFAULTS a "required" field, so their schemas were
/// LYING. They were schema-aligned (field removed from `required[]`) rather than
/// allowlisted, keeping this validator a plain hard-reject. The 5:
/// `mode` (`action` → `"get"`), `create_instance` (`name` auto-derived in team
/// mode; single path still errors "missing 'name'"), `set_waiting_on`
/// (`condition` absent = clear), and `set_display_name` / `set_description`
/// (handler tolerates absent, sets `""` — a separate follow-up decides whether
/// set-X-without-X should hard-error).
///
/// Tools whose handler DOES error on a missing field (`reply.message`,
/// `send.message`, `delete_instance.instance`, action-based `task`/`decision`)
/// keep their `required[]` and are hard-rejected here. A new tool that declares
/// a field its handler defaults will trip the pinning test below.
fn validate_args(tool: &str, def: &Value, args: &Value) -> Option<Value> {
    let schema = &def["inputSchema"];
    if let Some(required) = schema["required"].as_array() {
        for req in required.iter().filter_map(Value::as_str) {
            // Rank8: treat a present-but-JSON-null value as missing. `args.get`
            // returns `Some(Value::Null)` for `{"req": null}`, so a bare
            // `is_none()` let null slip through — the handler then defaulted it
            // (e.g. `as_str().unwrap_or("")` → an empty reply) and the failure
            // surfaced opaquely downstream (Telegram 400) instead of here. A
            // legit empty string is NOT null, so `""` still passes.
            if args.get(req).is_none_or(Value::is_null) {
                return Some(json!({
                    "error": format!("{tool}: missing required parameter '{req}'")
                }));
            }
        }
    }
    if let (Some(props), Some(obj)) = (schema["properties"].as_object(), args.as_object()) {
        for key in obj.keys() {
            if !props.contains_key(key) {
                tracing::warn!(
                    tool, param = %key,
                    "#1602: unknown parameter (ignored) — not in the tool's inputSchema"
                );
            }
        }
    }
    None
}

/// Look the `tool` name up in the registry. Returns `Some(value)`
/// on hit; returns `None` if the tool isn't registered — the caller
/// falls back to the inline `match` in `mod.rs` for un-migrated arms.
pub(super) fn try_dispatch(tool: &str, ctx: &HandlerCtx<'_>) -> Option<Value> {
    crate::mcp::registry::all()
        .iter()
        .find(|entry| entry.name == tool)
        .map(|entry| {
            // #1602: enforce the declared inputSchema at the single dispatch
            // chokepoint — a missing required param is rejected with a clear
            // named error instead of failing late inside the handler.
            if let Some(err) = validate_args(entry.name, &(entry.definition)(), ctx.args) {
                return err;
            }
            (entry.handler)(ctx)
        })
}

// ---------------------------------------------------------------------
// Adapter generation macro — eliminates per-tool boilerplate.
//
// Each invocation generates:
//   fn dispatch_<ident>(ctx: &HandlerCtx<'_>) -> Value { <body> }
//
// Four shapes match the four handler signatures in the codebase:
//   (home, args, instance)        → shape: hai
//   (home, args)                  → shape: ha
//   (home, args, instance, sender)→ shape: hais
//   (home, args, sender)          → shape: has
//   (home)                        → shape: h
//   (args)                        → shape: a
//   custom body                   → shape: custom
// ---------------------------------------------------------------------
macro_rules! adapter {
    // Generic fn-generating arm: emit `fn $name(ctx) -> Value` that delegates to
    // the matching `@call` arm for `$shape`. Collapses the former six per-shape
    // fn arms (hai/ha/hais/has/h/a) into one — byte-identical expansion. The
    // `@call` arms below lead with `@` (not an `ident`), so an `adapter!(@call …)`
    // invocation can never match this `$name:ident` arm.
    ($name:ident, $shape:ident, $handler:expr) => {
        pub(crate) fn $name(ctx: &HandlerCtx<'_>) -> Value {
            adapter!(@call ctx, $shape, $handler)
        }
    };
    (@call $ctx:ident, hai, $handler:expr) => {
        $handler($ctx.home, $ctx.args, $ctx.instance_name)
    };
    (@call $ctx:ident, ha, $handler:expr) => {
        $handler($ctx.home, $ctx.args)
    };
    (@call $ctx:ident, hais, $handler:expr) => {
        $handler($ctx.home, $ctx.args, $ctx.instance_name, $ctx.sender)
    };
    (@call $ctx:ident, has, $handler:expr) => {
        $handler($ctx.home, $ctx.args, $ctx.sender)
    };
    (@call $ctx:ident, har, $handler:expr) => {
        $handler($ctx.home, $ctx.args, $ctx.runtime)
    };
    (@call $ctx:ident, h, $handler:expr) => {
        $handler($ctx.home)
    };
    (@call $ctx:ident, a, $handler:expr) => {
        $handler($ctx.args)
    };
}

macro_rules! action_adapter {
    ($name:ident, $tool_label:literal, [ $( $action:literal => $handler:expr , $shape:ident );+ $(;)? ]) => {
        pub(crate) fn $name(ctx: &HandlerCtx<'_>) -> Value {
            match ctx.args["action"].as_str().unwrap_or("") {
                $( $action => { adapter!(@call ctx, $shape, $handler) } )+
                other => json!({"error": format!(concat!("unknown ", $tool_label, " action: {}"), other)}),
            }
        }
    };
}

// ---------------------------------------------------------------------
// Flat adapters — one per simple (non-action-based) tool.
// ---------------------------------------------------------------------

pub(crate) fn dispatch_list_instances(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_list_instances(ctx.home, ctx.args, ctx.instance_name, ctx.runtime)
}
/// #2454 Slice 2: create/start/replace/delete/restart need RuntimeContext.
pub(crate) fn dispatch_create_instance(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_create_instance(ctx.home, ctx.args, ctx.instance_name, ctx.runtime)
}
adapter!(
    dispatch_set_description,
    hai,
    instance::handle_set_description
);
pub(crate) fn dispatch_interrupt(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_interrupt(ctx.home, ctx.args, ctx.runtime)
}
adapter!(dispatch_tokens, ha, crate::token_cost::handle_tokens);
pub(crate) fn dispatch_delete_instance(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_delete_instance(ctx.home, ctx.args, ctx.runtime)
}
pub(crate) fn dispatch_start_instance(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_start_instance(ctx.home, ctx.args, ctx.runtime)
}
pub(crate) fn dispatch_replace_instance(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_replace_instance(ctx.home, ctx.args, ctx.runtime)
}
pub(crate) fn dispatch_restart_instance(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_restart_instance(ctx.home, ctx.args, ctx.runtime)
}
adapter!(
    dispatch_agy_quota,
    ha,
    crate::api::handlers::agy_quota::handle_agy_quota
);
pub(crate) fn dispatch_move_pane(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_move_pane(ctx.home, ctx.args, ctx.runtime)
}
adapter!(
    dispatch_set_waiting_on,
    hais,
    instance::handle_set_waiting_on
);
/// #2454 Slice 2: send needs RuntimeContext for in-process delivery.
pub(crate) fn dispatch_send(ctx: &HandlerCtx<'_>) -> Value {
    comms::handle_unified_send(ctx.home, ctx.args, ctx.sender, ctx.runtime)
}
adapter!(dispatch_bind_self, has, worktree::handle_bind_self);
adapter!(
    dispatch_binding_state,
    has,
    binding_state::handle_binding_state
);
adapter!(
    dispatch_release_worktree,
    has,
    worktree::handle_release_worktree
);
adapter!(
    dispatch_force_release_worktree,
    has,
    force_release::handle_force_release_worktree
);
adapter!(dispatch_gc_dry_run, has, gc::handle_gc_dry_run);
adapter!(
    dispatch_download_attachment,
    hai,
    channel::handle_download_attachment
);
adapter!(dispatch_reply, hai, channel::handle_reply);
adapter!(
    dispatch_set_display_name,
    hai,
    instance::handle_set_display_name
);
pub(crate) fn dispatch_pane_snapshot(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_pane_snapshot(ctx.home, ctx.args, ctx.runtime)
}
pub(crate) fn dispatch_list_rules(ctx: &HandlerCtx<'_>) -> Value {
    let agent_name = match ctx.args.get("agent_name").and_then(|v| v.as_str()) {
        Some(name) => name,
        None => return json!({"error": "missing required parameter 'agent_name'"}),
    };
    json!(crate::reflexion::list_rules(ctx.home, agent_name))
}
pub(crate) fn dispatch_record_mistake(ctx: &HandlerCtx<'_>) -> Value {
    let summary = match ctx.args.get("summary").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return json!({"error": "missing required parameter 'summary'"}),
    };
    let agent_name = match ctx.args.get("agent_name").and_then(|v| v.as_str()) {
        Some(a) => a,
        None => return json!({"error": "missing required parameter 'agent_name'"}),
    };
    let category = ctx.args.get("category").and_then(|v| v.as_str());
    let reporter = ctx
        .sender
        .as_ref()
        .map(|s| s.as_str())
        .unwrap_or("anonymous");

    let rule_id = crate::reflexion::record_mistake(
        ctx.home, reporter, agent_name, summary, ctx.args, category,
    );

    json!({ "rule_id": rule_id })
}
adapter!(
    dispatch_task_sweep_config,
    ha,
    task::handle_task_sweep_config
);
adapter!(dispatch_restart_daemon, h, restart::handle_restart_daemon);

// ---------------------------------------------------------------------
// Action-based adapters — match on args["action"], route to per-action
// handler. Unknown actions produce tool-specific error JSON.
// ---------------------------------------------------------------------

const KEYWORD_MIN_LEN: usize = 3;

/// Common task-description filler — excluded so generic dispatches still
/// receive all rules (empty keywords ⇒ [`rule_is_relevant`] passes everything).
const KEYWORD_STOPWORDS: &[&str] = &[
    "the",
    "and",
    "for",
    "with",
    "from",
    "this",
    "that",
    "into",
    "before",
    "after",
    "when",
    "your",
    "have",
    "will",
    "been",
    "were",
    "they",
    "please",
    "task",
    "work",
    "worker",
    "original",
    "description",
    "implement",
    "create",
    "make",
    "change",
    "complete",
    "update",
    "review",
    "branch",
    "repo",
];

/// Tokenize a task description into lowercase keywords for relevance matching.
pub(crate) fn extract_keywords(task_description: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    task_description
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= KEYWORD_MIN_LEN)
        .map(str::to_lowercase)
        .filter(|w| !KEYWORD_STOPWORDS.contains(&w.as_str()))
        .filter(|w| seen.insert(w.clone()))
        .collect()
}

/// True when `rule_text` shares at least one keyword with the task, or when no
/// keywords were extracted (generic/short descriptions — fail-open).
pub(crate) fn rule_is_relevant(rule_text: &str, keywords: &[String]) -> bool {
    if keywords.is_empty() {
        return true;
    }
    let haystack = rule_text.to_lowercase();
    keywords.iter().any(|kw| haystack.contains(kw))
}

pub(crate) fn dispatch_task(ctx: &HandlerCtx<'_>) -> Value {
    if ctx.args["action"].as_str() == Some("create") {
        let mut modified_args = ctx.args.clone();
        let original_message = modified_args["description"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if let Some(agent_name) = modified_args["assignee"].as_str() {
            if !agent_name.is_empty() {
                let mut sections = vec![original_message.to_string()];
                let keywords = extract_keywords(&original_message);
                let rules: Vec<_> = crate::reflexion::list_rules(ctx.home, agent_name)
                    .into_iter()
                    .filter(|r| rule_is_relevant(&r.rule_text, &keywords))
                    .collect();
                if !rules.is_empty() {
                    let rules_text = rules
                        .iter()
                        .map(|r| format!("- {}", r.rule_text))
                        .collect::<Vec<_>>()
                        .join("\n");
                    sections.push(format!("[適用規則]\n{rules_text}"));
                }
                let cross_rules: Vec<_> =
                    crate::reflexion::list_cross_agent_rules(ctx.home, agent_name)
                        .into_iter()
                        .filter(|r| rule_is_relevant(&r.rule_text, &keywords))
                        .collect();
                if !cross_rules.is_empty() {
                    let own_rule_texts: std::collections::HashSet<&str> =
                        rules.iter().map(|r| r.rule_text.as_str()).collect();
                    let cross_text = cross_rules
                        .iter()
                        .filter(|r| !own_rule_texts.contains(r.rule_text.as_str()))
                        .map(|r| format!("- [{}] {}", r.agent_name, r.rule_text))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !cross_text.is_empty() {
                        sections.push(format!("[其他 Worker 規則參考]\n{cross_text}"));
                    }
                }
                if semantic_search_enabled() {
                    if let Some(mem0_context) = dispatch_mem0_context(&original_message) {
                        sections.push(mem0_context);
                    }
                }
                if sections.len() > 1 {
                    modified_args["description"] = json!(sections.join("\n\n"));
                }
            }
        }
        let result = task::handle_task(ctx.home, &modified_args, ctx.instance_name);
        attach_decompose_fields(result, &original_message)
    } else {
        task::handle_task(ctx.home, ctx.args, ctx.instance_name)
    }
}

fn attach_decompose_fields(result: Value, original_message: &str) -> Value {
    if result.get("error").is_some() {
        return result;
    }
    let Some(subtasks) = maybe_decompose_task(original_message) else {
        return result;
    };
    if subtasks.len() < 2 {
        return result;
    }
    let mut obj = match result {
        Value::Object(map) => map,
        other => return other,
    };
    obj.insert("decomposed".to_string(), json!(true));
    obj.insert("subtasks".to_string(), json!(subtasks));
    Value::Object(obj)
}

const DECOMPOSE_MIN_DESCRIPTION_LEN: usize = 500;
const DECOMPOSE_CONJUNCTION_MARKERS: &[&str] = &["and", "以及", "還要", "並且"];

fn decompose_enabled() -> bool {
    match std::env::var("DECOMPOSE_ENABLED") {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

fn should_attempt_decompose(description: &str) -> bool {
    if description.chars().count() <= DECOMPOSE_MIN_DESCRIPTION_LEN {
        return false;
    }
    let haystack = description.to_lowercase();
    DECOMPOSE_CONJUNCTION_MARKERS
        .iter()
        .any(|marker| haystack.contains(marker))
}

fn maybe_decompose_task(description: &str) -> Option<Vec<String>> {
    if !decompose_enabled() || !should_attempt_decompose(description) {
        return None;
    }
    dispatch_decompose_task(description)
}

fn dispatch_decompose_task(description: &str) -> Option<Vec<String>> {
    let description = description.to_string();
    // fire-and-forget: not detached; joined below before returning to bound Ollama lookup lifetime.
    let worker = match std::thread::Builder::new()
        .name("agend-ollama-decompose".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    tracing::warn!(error = %err, "failed to build Ollama decompose runtime");
                    return None;
                }
            };
            rt.block_on(decompose_task_with_ollama(&description))
        }) {
        Ok(worker) => worker,
        Err(err) => {
            tracing::warn!(error = %err, "failed to spawn Ollama decompose thread");
            return None;
        }
    };
    match worker.join() {
        Ok(subtasks) => subtasks,
        Err(_) => {
            tracing::warn!("Ollama decompose thread panicked");
            None
        }
    }
}

async fn decompose_task_with_ollama(description: &str) -> Option<Vec<String>> {
    let base_url =
        std::env::var("OLLAMA_HTTP_URL").unwrap_or_else(|_| "http://localhost:11434".to_string());
    let model =
        std::env::var("OLLAMA_DECOMPOSE_MODEL").unwrap_or_else(|_| "qwen2.5:7b".to_string());
    let client = ollama_http_client()?;
    decompose_task_with_client(client, &base_url, &model, description).await
}

async fn decompose_task_with_client(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    description: &str,
) -> Option<Vec<String>> {
    let prompt = format!(
        "Analyze this task description and decide whether it contains 2 or more independent subtasks \
         that could be dispatched separately.\n\n\
         Task description:\n{description}\n\n\
         If there are 2 or more independent subtasks, reply with ONLY a JSON array of strings — \
         each string is one subtask description.\n\
         If there are fewer than 2 independent subtasks, reply with ONLY an empty JSON array: []\n\n\
         Reply with JSON only, no markdown."
    );
    let response = match client
        .post(format!("{}/api/chat", base_url.trim_end_matches('/')))
        .json(&json!({
            "model": model,
            "stream": false,
            "messages": [{"role": "user", "content": prompt}],
        }))
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!(error = %err, "Ollama decompose request failed");
            return None;
        }
    };
    if !response.status().is_success() {
        tracing::warn!(
            status = %response.status(),
            "Ollama decompose returned non-success status"
        );
        return None;
    }
    let body: Value = match response.json().await {
        Ok(body) => body,
        Err(err) => {
            tracing::warn!(error = %err, "Ollama decompose response was not valid JSON");
            return None;
        }
    };
    parse_decompose_subtasks(body["message"]["content"].as_str().unwrap_or(""))
}

fn parse_decompose_subtasks(raw: &str) -> Option<Vec<String>> {
    let mut text = raw.trim();
    if text.starts_with("```") {
        text = text
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();
    }
    let array = serde_json::from_str::<Vec<String>>(text).ok()?;
    let subtasks: Vec<String> = array
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if subtasks.len() >= 2 {
        Some(subtasks)
    } else {
        None
    }
}

fn ollama_http_client() -> Option<&'static reqwest::Client> {
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            match reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
            {
                Ok(client) => Some(client),
                Err(err) => {
                    tracing::warn!(error = %err, "failed to build Ollama decompose HTTP client");
                    None
                }
            }
        })
        .as_ref()
}

fn semantic_search_enabled() -> bool {
    match std::env::var("SEMANTIC_SEARCH_ENABLED") {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}

fn dispatch_mem0_context(message: &str) -> Option<String> {
    let query = message.to_string();
    // fire-and-forget: not detached; joined below before returning to bound Mem0 lookup lifetime.
    let worker = match std::thread::Builder::new()
        .name("agend-mem0-dispatch".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    tracing::warn!(error = %err, "failed to build Mem0 dispatch lookup runtime");
                    return None;
                }
            };
            rt.block_on(search_mem0_context(&query))
        }) {
        Ok(worker) => worker,
        Err(err) => {
            tracing::warn!(error = %err, "failed to spawn Mem0 dispatch lookup thread");
            return None;
        }
    };
    match worker.join() {
        Ok(context) => context,
        Err(_) => {
            tracing::warn!("Mem0 dispatch lookup thread panicked");
            None
        }
    }
}

async fn search_mem0_context(query: &str) -> Option<String> {
    let base_url =
        std::env::var("MEM0_HTTP_URL").unwrap_or_else(|_| "http://localhost:5174".to_string());
    let user_id = std::env::var("MEM0_USER_ID").unwrap_or_else(|_| "neo".to_string());
    let client = mem0_http_client()?;
    search_mem0_context_with_client(client, &base_url, &user_id, query).await
}

async fn search_mem0_context_with_client(
    client: &reqwest::Client,
    base_url: &str,
    user_id: &str,
    query: &str,
) -> Option<String> {
    let url = format!("{}/search", base_url.trim_end_matches('/'));
    let response = match client
        .post(url)
        .json(&json!({
            "query": query,
            "limit": 3,
            "user_id": user_id,
        }))
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!(error = %err, "Mem0 dispatch lookup request failed");
            return None;
        }
    };
    if !response.status().is_success() {
        tracing::warn!(
            status = %response.status(),
            "Mem0 dispatch lookup returned non-success status"
        );
        return None;
    }
    let body: Value = match response.json().await {
        Ok(body) => body,
        Err(err) => {
            tracing::warn!(error = %err, "Mem0 dispatch lookup response was not valid JSON");
            return None;
        }
    };
    let results = match body["results"].as_array() {
        Some(results) => results,
        None => {
            tracing::warn!("Mem0 dispatch lookup response missing results array");
            return None;
        }
    };
    let memories = results
        .iter()
        .filter(|result| result["score"].as_f64().is_some_and(|score| score >= 0.6))
        .filter_map(|result| result["memory"].as_str())
        .map(|memory| format!("- {memory}"))
        .collect::<Vec<_>>();
    if memories.is_empty() {
        None
    } else {
        Some(format!("[過去經驗]\n{}", memories.join("\n")))
    }
}

fn mem0_http_client() -> Option<&'static reqwest::Client> {
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            match reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
            {
                Ok(client) => Some(client),
                Err(err) => {
                    tracing::warn!(error = %err, "failed to build Mem0 dispatch HTTP client");
                    None
                }
            }
        })
        .as_ref()
}

action_adapter!(dispatch_ci, "ci", [
    "watch"   => ci::handle_watch_ci,   hai;
    "unwatch" => ci::handle_unwatch_ci, hai;
    "status"  => ci::handle_status_ci,  hai;
]);

action_adapter!(dispatch_decision, "decision", [
    "post"   => task::handle_post_decision,   hais;
    "list"   => task::handle_list_decisions,   ha;
    "update" => task::handle_update_decision,  hai;
    "answer" => task::handle_answer_decision,  hais;
]);

action_adapter!(dispatch_deployment, "deployment", [
    "deploy"   => schedule::handle_deploy_template,      hai;
    "teardown" => schedule::handle_teardown_deployment,   ha;
    "list"     => schedule::handle_list_deployments,      h;
]);

action_adapter!(dispatch_ephemeral, "ephemeral", [
    "spawn" => ephemeral::handle_spawn, hai;
    "list"  => ephemeral::handle_list,  ha;
    "reap"  => ephemeral::handle_reap,  ha;
]);

/// #2454: custom (not `action_adapter!`) — health writes need RuntimeContext.
pub(crate) fn dispatch_health(ctx: &HandlerCtx<'_>) -> Value {
    match ctx.args["action"].as_str().unwrap_or("") {
        "report" => instance::handle_report_health(
            ctx.home,
            ctx.args,
            ctx.instance_name,
            ctx.sender,
            ctx.runtime,
        ),
        "clear" => instance::handle_clear_blocked_reason(ctx.home, ctx.args, ctx.runtime),
        other => json!({"error": format!("unknown health action: {other}")}),
    }
}

action_adapter!(dispatch_repo, "repo", [
    "checkout"                => ci::handle_checkout_repo,              hai;
    "release"                 => ci::handle_release_repo,               a;
    "cleanup_init_commits"    => ci::handle_cleanup_init_commits,       hai;
    "cleanup_merged_branches" => ci::handle_cleanup_merged_branches,    hai;
    "merge"                   => ci::handle_merge_repo,                 hai;
]);

action_adapter!(dispatch_schedule, "schedule", [
    "create" => schedule::handle_create_schedule,  hai;
    "list"   => schedule::handle_list_schedules,   ha;
    "update" => schedule::handle_update_schedule,  ha;
    "delete" => schedule::handle_delete_schedule,  ha;
]);

action_adapter!(dispatch_team, "team", [
    "create" => task::handle_create_team,  har;
    "delete" => task::handle_delete_team,  ha;
    "list"   => task::handle_list_teams,   h;
    "update" => task::handle_update_team,  har;
]);

// `inbox` — branch on `args["action"]` then arg presence:
//   - `action=ack`  → confirm processed (#2299; delivering → processed)
//   - `action=clear` → quiet compact-clear
//   - `message_id` present → describe single message
//   - else `thread_id` present → describe thread
//   - else → drain pending
pub(crate) fn dispatch_inbox(ctx: &HandlerCtx<'_>) -> Value {
    let action = ctx.args.get("action").and_then(|v| v.as_str());
    if action == Some("ack") {
        // #2299 explicit ack (C): confirm the agent HANDLED what it drained →
        // delivering → processed, so the reclaim-TTL won't re-deliver it.
        comms::handle_inbox_ack(ctx.home, ctx.args, ctx.instance_name)
    } else if action == Some("discharge") {
        // #2622: the deliberate exit for a channel-reply obligation that will
        // not be (or no longer needs to be) answered — durably suppresses
        // re-arm, stops the ladder, LOUDLY notifies the operator. Sibling of
        // `ack`/`clear` (all obligation-settling ops on inbox messages).
        channel::handle_discharge(ctx.home, ctx.args, ctx.instance_name)
    } else if action == Some("clear") {
        // #inbox-gc part a: quiet compact-clear (explicit action — never the
        // no-arg drain). Obligations stay unread; returns bounded summaries.
        comms::handle_inbox_clear(ctx.home, ctx.instance_name)
    } else if ctx
        .args
        .get("message_id")
        .and_then(|v| v.as_str())
        .is_some()
    {
        comms::handle_describe_message(ctx.home, ctx.args, ctx.instance_name)
    } else if ctx.args.get("thread_id").and_then(|v| v.as_str()).is_some() {
        comms::handle_describe_thread(ctx.home, ctx.args)
    } else {
        comms::handle_inbox(ctx.home, ctx.instance_name)
    }
}

pub(crate) fn dispatch_tui_screenshot(ctx: &HandlerCtx<'_>) -> Value {
    if let Some(rt) = ctx.runtime {
        let api_ctx = crate::mcp::handlers::runtime_bridge::api_ctx(ctx.home, rt);
        let resp = crate::api::handlers::instance::handle_tui_screenshot(&api_ctx);
        if resp["ok"].as_bool() == Some(true) {
            return serde_json::json!({"svg": resp["svg"]});
        }
    }
    match crate::api::call(
        ctx.home,
        &serde_json::json!({"method": crate::api::method::TUI_SCREENSHOT, "params": {}}),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            serde_json::json!({"svg": resp["svg"]})
        }
        Ok(resp) => {
            serde_json::json!({"error": resp["error"].as_str().unwrap_or("tui_screenshot failed")})
        }
        Err(e) => serde_json::json!({"error": format!("tui_screenshot: {e}")}),
    }
}

// `watchdog` — actions with inline business logic (not just forwarding).
pub(crate) fn dispatch_watchdog(ctx: &HandlerCtx<'_>) -> Value {
    match ctx.args["action"].as_str().unwrap_or("") {
        "snooze" => dispatch_watchdog_snooze(ctx),
        "resume" => dispatch_watchdog_resume(ctx),
        "status" => dispatch_watchdog_status(ctx),
        "ack" => dispatch_watchdog_ack(ctx),
        other => json!({"error": format!("unknown watchdog action: {other}")}),
    }
}

fn dispatch_watchdog_snooze(ctx: &HandlerCtx<'_>) -> Value {
    use crate::daemon::idle_watchdog;

    const MAX_SNOOZE_SECS: i64 = 4 * 3600;

    let duration_str = ctx.args["duration"].as_str().unwrap_or("1h");
    let secs = match parse_duration_secs(duration_str) {
        Some(s) => s.min(MAX_SNOOZE_SECS),
        None => return json!({"error": format!("invalid duration: {duration_str}")}),
    };
    let until = chrono::Utc::now() + chrono::Duration::seconds(secs);
    let actor = ctx.instance_name;
    match idle_watchdog::snooze_fleet_idle(ctx.home, until, actor) {
        Ok(snooze) => {
            crate::event_log::log(
                ctx.home,
                "watchdog_snooze",
                actor,
                &format!(
                    "fleet idle snoozed until {} ({duration_str})",
                    snooze.snoozed_until
                ),
            );
            json!({
                "snoozed": true,
                "snoozed_until": snooze.snoozed_until,
                "duration_secs": secs,
            })
        }
        Err(e) => json!({"error": format!("snooze failed: {e}")}),
    }
}

fn dispatch_watchdog_resume(ctx: &HandlerCtx<'_>) -> Value {
    use crate::daemon::idle_watchdog;
    idle_watchdog::resume_fleet_idle(ctx.home);
    crate::event_log::log(
        ctx.home,
        "watchdog_resume",
        ctx.instance_name,
        "fleet idle snooze cleared",
    );
    json!({"snoozed": false})
}

fn dispatch_watchdog_status(ctx: &HandlerCtx<'_>) -> Value {
    use crate::daemon::idle_watchdog;
    if let Some(snooze) = idle_watchdog::get_snooze_state(ctx.home) {
        let remaining = chrono::DateTime::parse_from_rfc3339(&snooze.snoozed_until)
            .ok()
            .map(|dt| {
                dt.with_timezone(&chrono::Utc)
                    .signed_duration_since(chrono::Utc::now())
                    .num_seconds()
                    .max(0)
            })
            .unwrap_or(0);
        json!({
            "snoozed": true,
            "snoozed_until": snooze.snoozed_until,
            "remaining_secs": remaining,
            "actor": snooze.actor,
        })
    } else {
        let ack_info = idle_watchdog::fleet_ack_status().map(|ts| json!({"acked_at": ts}));
        json!({"snoozed": false, "ack": ack_info})
    }
}

fn dispatch_watchdog_ack(ctx: &HandlerCtx<'_>) -> Value {
    use crate::daemon::idle_watchdog;
    let ts = idle_watchdog::ack_fleet_idle();
    let actor = ctx.instance_name;
    crate::event_log::log(
        ctx.home,
        "watchdog_ack",
        actor,
        "fleet idle acked — suppressed until post-ack activity",
    );
    json!({
        "acked": true,
        "acked_at": ts,
    })
}

pub(crate) fn dispatch_config(ctx: &HandlerCtx<'_>) -> Value {
    match ctx.args["action"].as_str().unwrap_or("") {
        "get" => {
            let key = ctx.args["key"].as_str().unwrap_or("");
            if key.is_empty() {
                return json!({"error": "key is required for get"});
            }
            match crate::runtime_config::get_key(key) {
                Ok(v) => json!({"key": key, "value": v}),
                Err(e) => json!({"error": e}),
            }
        }
        "set" => {
            let key = ctx.args["key"].as_str().unwrap_or("");
            let value = ctx.args["value"].as_str().unwrap_or("");
            if key.is_empty() || value.is_empty() {
                return json!({"error": "key and value are required for set"});
            }
            match crate::runtime_config::set(ctx.home, key, value) {
                Ok(_) => json!({"ok": true, "key": key, "value": value}),
                Err(e) => json!({"error": e}),
            }
        }
        "list" => json!({"config": crate::runtime_config::list()}),
        other => json!({"error": format!("unknown config action: {other}")}),
    }
}

/// #1339: read the operator-mode (GET-ONLY for agents). `mode get` → current
/// mode + delegate. SETTING the mode is operator-only and lives on the operator
/// transport (`agend-terminal mode <active|away|sleep>` CLI → the direct `MODE`
/// API method); the ingress gate blocks any agent `mode set` regardless, so this
/// tool exposes read access only — agents observe the mode (e.g. to back off when
/// the operator is away/asleep) but can never change operator authority.
pub(crate) fn dispatch_mode(ctx: &HandlerCtx<'_>) -> Value {
    match ctx.args["action"].as_str().unwrap_or("get") {
        "get" => {
            let s = crate::operator_mode::get();
            json!({
                "ok": true,
                "mode": s.mode,
                "delegate_to": s.delegate_to,
                "delegate_scope": s.delegate_scope,
            })
        }
        other => json!({
            "error": format!(
                "mode is read-only via MCP (action '{other}'); set the operator mode with the \
                 `agend-terminal mode <active|away|sleep>` CLI (operator-only)"
            )
        }),
    }
}

/// Parse human-friendly duration strings like "2h", "30m", "1h30m".
/// A bare number without suffix is interpreted as **minutes**.
fn parse_duration_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut total: i64 = 0;
    let mut num_buf = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num_buf.push(ch);
        } else {
            let n: i64 = num_buf.parse().ok()?;
            num_buf.clear();
            // Checked arithmetic: an attacker-controlled digit run (e.g.
            // "9999999999999999h") parses to a valid i64 whose `* 3600` overflows
            // i64::MAX — debug builds panic, release wraps to a bogus value.
            // Reject the overflow as invalid (None) instead.
            let secs = match ch {
                'h' => n.checked_mul(3600)?,
                'm' => n.checked_mul(60)?,
                's' => n,
                _ => return None,
            };
            total = total.checked_add(secs)?;
        }
    }
    if !num_buf.is_empty() {
        let n: i64 = num_buf.parse().ok()?;
        total = total.checked_add(n.checked_mul(60)?)?; // bare number = minutes
    }
    if total > 0 {
        Some(total)
    } else {
        None
    }
}

// ---------------------------------------------------------------------
// Context-aware worker routing — prefer idle workers with lower context%.
// ---------------------------------------------------------------------

/// Context% assumed when telemetry is absent (medium load).
pub(crate) const DEFAULT_UNKNOWN_CONTEXT_PCT: f32 = 50.0;

/// Sort key for context-aware routing (`None` → [`DEFAULT_UNKNOWN_CONTEXT_PCT`]).
pub(crate) fn context_sort_key(pct: Option<f32>) -> f32 {
    pct.unwrap_or(DEFAULT_UNKNOWN_CONTEXT_PCT)
}

fn is_operated_idle(
    raw: crate::state::AgentState,
    observed: Option<&crate::daemon::shadow::reducer::ObservedStatus>,
) -> bool {
    crate::daemon::shadow::operated_state(raw, observed) == crate::state::AgentState::Idle
}

/// Snapshot idle candidates with their resolved `context_pct` (registry lock only).
fn snapshot_idle_workers_with_context(
    registry: &crate::agent::AgentRegistry,
    candidates: &[String],
    home: &Path,
) -> Vec<(String, Option<f32>)> {
    use std::collections::HashSet;

    let candidate_set: HashSet<&str> = candidates.iter().map(String::as_str).collect();
    let reg = crate::agent::lock_registry(registry);
    reg.values()
        .filter(|handle| candidate_set.contains(handle.name.as_str()))
        .filter_map(|handle| {
            let (raw, observed, context) = {
                let c = handle.core.lock();
                (
                    c.state.current,
                    c.observed_status.clone(),
                    c.state.resolved_context(Some(home)),
                )
            };
            if !is_operated_idle(raw, observed.as_ref()) {
                return None;
            }
            let pct = context.map(|(p, _)| p);
            Some((handle.name.to_string(), pct))
        })
        .collect()
}

/// Among `candidates`, return the operated-idle worker with the lowest context%.
pub(crate) fn select_lowest_context_idle_worker(
    registry: &crate::agent::AgentRegistry,
    candidates: &[String],
    home: &Path,
) -> Option<String> {
    let mut idle = snapshot_idle_workers_with_context(registry, candidates, home);
    if idle.is_empty() {
        return None;
    }
    idle.sort_by(|a, b| {
        context_sort_key(a.1)
            .partial_cmp(&context_sort_key(b.1))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    Some(idle[0].0.clone())
}

/// Candidate worker names for a kind=task dispatch that would otherwise broadcast.
pub(crate) fn resolve_task_dispatch_candidates(
    home: &Path,
    args: &Value,
    sender: &str,
) -> Vec<String> {
    if let Some(arr) = args["instances"].as_array() {
        return arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .filter(|n| n != sender)
            .collect();
    }
    if let Some(team) = args["team"].as_str() {
        return crate::teams::get_members(home, team)
            .into_iter()
            .filter(|n| n != sender)
            .collect();
    }
    if let Some(tags) = args["tags"].as_array() {
        use std::collections::HashSet;

        let tag_set: HashSet<String> = tags
            .iter()
            .filter_map(|v| v.as_str().map(str::to_lowercase))
            .collect();
        if tag_set.is_empty() {
            return Vec::new();
        }
        let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
            .unwrap_or_default();
        return fleet
            .instances
            .keys()
            .filter(|name| {
                if tag_set.contains(&name.to_lowercase()) {
                    return true;
                }
                fleet.instances.get(*name).is_some_and(|ic| {
                    ic.role_kind.is_some_and(|rk| {
                        serde_json::to_value(rk)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_lowercase))
                            .is_some_and(|rk_str| tag_set.contains(&rk_str))
                    })
                })
            })
            .filter(|n| *n != sender)
            .cloned()
            .collect();
    }
    Vec::new()
}

/// When dispatching kind=task to multiple candidates, pick the idle worker with
/// the lowest `context_pct`. Returns `None` when routing does not apply.
pub(crate) fn try_resolve_context_aware_task_target(
    home: &Path,
    registry: &crate::agent::AgentRegistry,
    args: &Value,
    sender: &str,
) -> Option<String> {
    if args["instance"].as_str().is_some_and(|s| !s.is_empty()) {
        return None;
    }
    if args["request_kind"].as_str() != Some("task") {
        return None;
    }
    let candidates = resolve_task_dispatch_candidates(home, args, sender);
    if candidates.len() < 2 {
        return None;
    }
    select_lowest_context_idle_worker(registry, &candidates, home)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Read;
    use std::net::TcpListener;
    use std::thread;

    fn ctx_for<'a>(home: &'a Path, args: &'a Value, instance: &'a str) -> HandlerCtx<'a> {
        static EMPTY_SENDER: Option<Sender> = None;
        HandlerCtx {
            home,
            args,
            instance_name: instance,
            sender: &EMPTY_SENDER,
            runtime: None,
        }
    }

    /// Context-aware routing: two idle workers at 30% vs 70% → pick 30%.
    #[cfg(unix)]
    #[test]
    fn context_aware_routing_prefers_lower_context_worker() {
        use crate::agent::{self, AgentRegistry};
        use crate::backend::Backend;
        use crate::state::AgentState;
        use parking_lot::Mutex;
        use std::collections::HashMap;
        use std::sync::Arc;

        fn worker_with_context(name: &str, pct: f32) -> agent::AgentHandle {
            let id = crate::types::InstanceId::default();
            let handle = agent::mk_test_handle(name, id);
            {
                let mut core = handle.core.lock();
                core.state = crate::state::StateTracker::new(Some(&Backend::ClaudeCode));
                core.state.current = AgentState::Idle;
                core.state.feed(&format!(
                    "────────────\n  Model: test | Ctx Used: {pct:.1}% | branch\n  bypass permissions on\n❯\n"
                ));
            }
            handle
        }

        let home = std::env::temp_dir();
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut reg = agent::lock_registry(&registry);
            let high = worker_with_context("worker-high", 70.0);
            let low = worker_with_context("worker-low", 30.0);
            reg.insert(high.id, high);
            reg.insert(low.id, low);
        }

        let candidates = vec!["worker-high".into(), "worker-low".into()];
        let selected = select_lowest_context_idle_worker(&registry, &candidates, &home)
            .expect("should select an idle worker");
        assert_eq!(selected, "worker-low");
    }

    #[test]
    fn context_sort_key_defaults_unknown_to_fifty() {
        assert!((context_sort_key(None) - DEFAULT_UNKNOWN_CONTEXT_PCT).abs() < f32::EPSILON);
        assert!((context_sort_key(Some(12.5)) - 12.5).abs() < f32::EPSILON);
    }

    #[test]
    fn extract_keywords_skips_stopwords_and_short_tokens() {
        assert!(extract_keywords("Original description").is_empty());
        let kw = extract_keywords("Run cargo test before push");
        assert!(kw.contains(&"cargo".to_string()));
        assert!(kw.contains(&"test".to_string()));
        assert!(kw.contains(&"push".to_string()));
        assert!(kw.contains(&"run".to_string()));
    }

    #[test]
    fn rule_is_relevant_fail_open_on_empty_keywords() {
        assert!(rule_is_relevant("NEVER open wrong repo", &[]));
    }

    #[test]
    fn test_dispatch_keyword_filter_fail_open_on_all_stopwords() {
        let description = "implement the task with repo branch";
        let keywords = extract_keywords(description);
        assert!(
            keywords.is_empty(),
            "all-stopword descriptions should yield no keywords: {keywords:?}"
        );

        let rule_texts = [
            "NEVER open wrong repo",
            "Always run cargo test before push",
            "Capture reviewer evidence before reporting",
        ];
        for rule_text in rule_texts {
            assert!(
                rule_is_relevant(rule_text, &keywords),
                "fail-open: {rule_text:?} should be relevant when keywords are empty"
            );
        }
    }

    #[test]
    fn rule_is_relevant_matches_substring() {
        let kw = extract_keywords("Verify cargo test evidence");
        assert!(rule_is_relevant("Always run tests before push", &kw));
        assert!(rule_is_relevant(
            "Capture reviewer evidence before reporting",
            &kw
        ));
        assert!(!rule_is_relevant("Always use justdoit530-hub for PRs", &kw));
    }

    #[test]
    fn semantic_search_enabled_defaults_true_and_accepts_false_values() {
        std::env::remove_var("SEMANTIC_SEARCH_ENABLED");
        assert!(semantic_search_enabled());

        std::env::set_var("SEMANTIC_SEARCH_ENABLED", "false");
        assert!(!semantic_search_enabled());

        std::env::set_var("SEMANTIC_SEARCH_ENABLED", "1");
        assert!(semantic_search_enabled());

        std::env::remove_var("SEMANTIC_SEARCH_ENABLED");
    }

    #[test]
    fn decompose_enabled_defaults_false_and_accepts_true_values() {
        std::env::remove_var("DECOMPOSE_ENABLED");
        assert!(!decompose_enabled());

        std::env::set_var("DECOMPOSE_ENABLED", "true");
        assert!(decompose_enabled());

        std::env::set_var("DECOMPOSE_ENABLED", "0");
        assert!(!decompose_enabled());

        std::env::remove_var("DECOMPOSE_ENABLED");
    }

    #[test]
    fn should_attempt_decompose_requires_length_and_conjunction() {
        let short = format!("{} and more", "a".repeat(100));
        assert!(!should_attempt_decompose(&short));

        let long_no_conj = "x".repeat(DECOMPOSE_MIN_DESCRIPTION_LEN + 1);
        assert!(!should_attempt_decompose(&long_no_conj));

        let long_with_conj = format!(
            "{} and {}",
            "x".repeat(DECOMPOSE_MIN_DESCRIPTION_LEN),
            "implement module B"
        );
        assert!(should_attempt_decompose(&long_with_conj));

        let long_with_cjk = format!("{} 以及 還要測試", "模".repeat(500));
        assert!(should_attempt_decompose(&long_with_cjk));
    }

    #[test]
    fn parse_decompose_subtasks_accepts_json_array() {
        let parsed =
            parse_decompose_subtasks(r#"["subtask one", "subtask two"]"#).expect("two subtasks");
        assert_eq!(parsed, vec!["subtask one", "subtask two"]);
        assert!(parse_decompose_subtasks(r#"["only one"]"#).is_none());
        assert!(parse_decompose_subtasks("not json").is_none());
    }

    #[test]
    fn decompose_disabled_skips_ollama_call_and_leaves_result_unchanged() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let contacted = Arc::new(AtomicBool::new(false));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        listener.set_nonblocking(true).expect("set nonblocking");
        let base_url = format!("http://{}", listener.local_addr().expect("server addr"));
        let contacted_flag = Arc::clone(&contacted);
        let _server = thread::spawn(move || {
            if listener.accept().is_ok() {
                contacted_flag.store(true, Ordering::SeqCst);
            }
        });

        std::env::remove_var("DECOMPOSE_ENABLED");
        std::env::set_var("OLLAMA_HTTP_URL", &base_url);

        let description = format!(
            "{} and {}",
            "implement module A ".repeat(40),
            "以及 implement module B with tests"
        );
        assert!(should_attempt_decompose(&description));
        assert!(maybe_decompose_task(&description).is_none());
        assert!(
            !contacted.load(Ordering::SeqCst),
            "DECOMPOSE_ENABLED=false must not contact Ollama"
        );

        let result = attach_decompose_fields(
            json!({"id": "t-test", "event": "created", "status": "created"}),
            &description,
        );
        assert!(result.get("decomposed").is_none());
        assert!(result.get("subtasks").is_none());

        std::env::remove_var("OLLAMA_HTTP_URL");
    }

    #[test]
    fn decompose_task_with_client_parses_subtasks_from_mock_ollama() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let base_url = format!("http://{}", listener.local_addr().expect("server addr"));
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut buf = [0; 8192];
            let _ = stream.read(&mut buf);
            let body =
                r#"{"message":{"content":"[\"build API endpoint\", \"add integration tests\"]"}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            std::io::Write::write_all(&mut stream, response.as_bytes()).expect("write response");
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .expect("client");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let subtasks = rt
            .block_on(decompose_task_with_client(
                &client,
                &base_url,
                "qwen2.5:7b",
                "long task and more",
            ))
            .expect("subtasks");

        server.join().expect("server thread");
        assert_eq!(subtasks.len(), 2);
        assert_eq!(subtasks[0], "build API endpoint");
    }

    #[test]
    fn search_mem0_context_filters_by_score() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let base_url = format!("http://{}", listener.local_addr().expect("server addr"));
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut buf = [0; 4096];
            let _ = stream.read(&mut buf);
            let body = r#"{"results":[{"memory":"too weak","score":0.59},{"memory":"relevant rule","score":0.6}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            std::io::Write::write_all(&mut stream, response.as_bytes()).expect("write response");
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .expect("client");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let context = rt
            .block_on(search_mem0_context_with_client(
                &client,
                &base_url,
                "neo",
                "dispatch task",
            ))
            .expect("semantic context");

        server.join().expect("server thread");
        assert!(context.contains("[過去經驗]"));
        assert!(context.contains("relevant rule"));
        assert!(!context.contains("too weak"));
    }

    #[test]
    fn search_mem0_context_timeout_fails_open() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let base_url = format!("http://{}", listener.local_addr().expect("server addr"));
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut buf = [0; 4096];
            let _ = stream.read(&mut buf);
            thread::sleep(Duration::from_millis(250));
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .expect("client");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let context = rt.block_on(search_mem0_context_with_client(
            &client,
            &base_url,
            "neo",
            "dispatch task",
        ));

        server.join().expect("server thread");
        assert!(context.is_none());
    }

    #[test]
    fn try_dispatch_returns_none_for_unregistered_tool() {
        let home = std::env::temp_dir();
        let args = json!({});
        let ctx = ctx_for(&home, &args, "");
        assert!(try_dispatch("definitely_not_a_real_tool", &ctx).is_none());
    }

    #[test]
    fn try_dispatch_returns_some_for_registered_tool() {
        let home = std::env::temp_dir();
        let args = json!({});
        let ctx = ctx_for(&home, &args, "");
        assert!(try_dispatch("list_instances", &ctx).is_some());
    }

    #[test]
    fn registered_handler_names_pin() {
        let names: Vec<&'static str> = crate::mcp::registry::all().iter().map(|e| e.name).collect();
        assert_eq!(
            names,
            vec![
                "reply",
                "download_attachment",
                "send",
                "inbox",
                "list_instances",
                "list_rules",
                "record_mistake",
                "create_instance",
                "delete_instance",
                "start_instance",
                "replace_instance",
                "restart_instance",
                "interrupt",
                "set_display_name",
                "set_description",
                "set_waiting_on",
                "move_pane",
                "pane_snapshot",
                "tui_screenshot",
                "decision",
                "task",
                "task_sweep_config",
                "restart_daemon",
                "team",
                "schedule",
                "deployment",
                "ephemeral",
                "ci",
                "health",
                "watchdog",
                "config",
                "repo",
                "bind_self",
                "release_worktree",
                "force_release_worktree",
                "binding_state",
                "gc_dry_run",
                "tokens",
                "mode",
                "agy_quota",
            ]
        );
        assert_eq!(crate::mcp::registry::all().len(), 40);
    }

    #[test]
    fn every_advertised_tool_is_routed_somewhere() {
        // #t-3 audit: the prior version grepped mod.rs + dispatch.rs SOURCE
        // text for the quoted tool name — a name appearing in a comment or
        // unrelated string would satisfy it without the tool actually being
        // routed (false confidence). We now drive the REAL routing path:
        // `try_dispatch` looks the name up in `mcp::registry::all()` (the
        // authoritative routing registry) and returns Some iff it routes.
        let defs = crate::mcp::tools::tool_definitions();
        let arr = defs
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tool_definitions() should return {tools: [...]}");
        let names: Vec<String> = arr
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .map(str::to_string)
            .collect();
        assert!(!names.is_empty(), "tool_definitions() advertised no tools");

        let home = std::env::temp_dir();
        let args = json!({});
        let ctx = ctx_for(&home, &args, "");
        let missing: Vec<&str> = names
            .iter()
            .filter(|name| try_dispatch(name, &ctx).is_none())
            .map(String::as_str)
            .collect();
        assert!(
            missing.is_empty(),
            "tools advertised by tool_definitions() but not routed through the dispatch registry: {missing:?}"
        );
    }

    #[test]
    fn try_dispatch_routes_known_action_through_base_handler() {
        let home = std::env::temp_dir();
        let cases: &[(&str, &[&str])] = &[
            (
                "task",
                &[
                    "create", "list", "claim", "update", "done", "sweep", "health", "activity",
                ],
            ),
            ("ci", &["watch", "unwatch", "status"]),
            ("decision", &["post", "list", "update"]),
            ("deployment", &["deploy", "teardown", "list"]),
            ("health", &["report", "clear"]),
            (
                "repo",
                &[
                    "checkout",
                    "release",
                    "cleanup_init_commits",
                    "cleanup_merged_branches",
                ],
            ),
            ("schedule", &["create", "list", "update", "delete"]),
            ("team", &["create", "delete", "list", "update"]),
            ("watchdog", &["snooze", "resume", "status", "ack"]),
            ("config", &["get", "set", "list"]),
        ];
        for (tool, actions) in cases {
            for action in actions.iter() {
                let args = json!({ "action": action });
                let ctx = ctx_for(&home, &args, "");
                assert!(
                    try_dispatch(tool, &ctx).is_some(),
                    "tool='{tool}' action='{action}' did not route through dispatch table"
                );
            }
        }
    }

    #[test]
    fn try_dispatch_unknown_action_falls_through_to_error() {
        let home = std::env::temp_dir();
        let args = json!({"action": "frobnicate-not-a-real-action"});
        let ctx = ctx_for(&home, &args, "");
        let result = try_dispatch("task", &ctx);
        assert!(result.is_some(), "base handler must still return Some");
        let v = result.unwrap();
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("");
        assert!(
            err.contains("unknown") || err.contains("action"),
            "expected unknown-action error from base; got: {v:?}"
        );
    }

    #[test]
    fn try_dispatch_missing_action_falls_through_to_base() {
        let home = std::env::temp_dir();
        let args = json!({}); // no "action" key
        let ctx = ctx_for(&home, &args, "");
        assert!(try_dispatch("task", &ctx).is_some());
    }

    // ── #1084 watchdog snooze MCP tests ──────────────────────────

    fn watchdog_home(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agend-watchdog-mcp-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn watchdog_snooze_then_status_round_trip() {
        let home = watchdog_home("snooze-status");
        let args = json!({"action": "snooze", "duration": "1h"});
        let ctx = ctx_for(&home, &args, "test-agent");
        let result = try_dispatch("watchdog", &ctx).unwrap();
        assert_eq!(result["snoozed"], true);
        assert!(result["snoozed_until"].is_string());
        assert_eq!(result["duration_secs"], 3600);

        let status_args = json!({"action": "status"});
        let status_ctx = ctx_for(&home, &status_args, "test-agent");
        let status = try_dispatch("watchdog", &status_ctx).unwrap();
        assert_eq!(status["snoozed"], true);
        assert!(status["remaining_secs"].as_i64().unwrap() > 0);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn watchdog_snooze_duration_clamped_to_4h() {
        let home = watchdog_home("snooze-clamp");
        let args = json!({"action": "snooze", "duration": "24h"});
        let ctx = ctx_for(&home, &args, "test-agent");
        let result = try_dispatch("watchdog", &ctx).unwrap();
        assert_eq!(
            result["duration_secs"],
            4 * 3600,
            "#1084: 24h must clamp to 4h"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn watchdog_resume_clears_snooze() {
        let home = watchdog_home("resume");
        let snooze_args = json!({"action": "snooze", "duration": "2h"});
        let ctx = ctx_for(&home, &snooze_args, "test-agent");
        try_dispatch("watchdog", &ctx);

        let resume_args = json!({"action": "resume"});
        let resume_ctx = ctx_for(&home, &resume_args, "test-agent");
        let result = try_dispatch("watchdog", &resume_ctx).unwrap();
        assert_eq!(result["snoozed"], false);

        let status_args = json!({"action": "status"});
        let status_ctx = ctx_for(&home, &status_args, "test-agent");
        let status = try_dispatch("watchdog", &status_ctx).unwrap();
        assert_eq!(status["snoozed"], false);
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #1602: inputSchema enforcement at dispatch ──────────────────────

    #[test]
    fn validate_rejects_missing_required_with_named_error() {
        // #3084: reply uses anyOf(message|message_from_file) so the top-level
        // required list is empty — schema validation alone no longer rejects {}.
        // Pin that empty-required tools still pass validate_args (handler enforces).
        let def = crate::mcp::tools::def_reply();
        assert!(
            validate_args("reply", &def, &json!({})).is_none(),
            "reply schema has no top-level required fields after #3084"
        );
    }

    #[test]
    fn validate_passes_when_required_present_and_unknown_only_warns() {
        // `message` present → no reject; an unknown key only warns (no reject).
        let def = crate::mcp::tools::def_reply();
        assert!(
            validate_args("reply", &def, &json!({"message": "hi"})).is_none(),
            "valid call must pass"
        );
        assert!(
            validate_args("reply", &def, &json!({"message": "hi", "bogus": 1})).is_none(),
            "unknown param must warn, not reject"
        );
    }

    /// #1602/#1603 audit pin. The systematic re-audit found 5 tools whose
    /// HANDLER defaults a field instead of erroring on its absence, so that
    /// field must NOT be in `required[]` (else the validator would hard-reject a
    /// legitimate call). Pins BOTH that the schema omits it from `required[]`
    /// AND that the validator lets the field-less call through. If a future
    /// tool/edit declares a handler-defaulted field required, this fails — re-run
    /// the audit (`grep unwrap_or` the handler).
    #[test]
    fn handler_defaulted_fields_are_not_declared_required() {
        use crate::mcp::tools::*;
        // (tool, def, handler-defaulted field, a legit call that omits it)
        let cases = [
            ("mode", def_mode(), "action", json!({})), // → "get" (read-only)
            (
                "create_instance",
                def_create_instance(),
                "name",
                json!({"team": "dev", "count": 2, "backend": "claude"}),
            ), // team mode auto-names; single path still errors "missing 'name'"
            (
                "set_waiting_on",
                def_set_waiting_on(),
                "condition",
                json!({}),
            ), // → clear
            (
                "set_display_name",
                def_set_display_name(),
                "name",
                json!({}),
            ), // → ""
            (
                "set_description",
                def_set_description(),
                "description",
                json!({}),
            ), // → ""
        ];
        for case in &cases {
            let (tool, def, field, args) = (case.0, &case.1, case.2, &case.3);
            let declares_required = def["inputSchema"]["required"]
                .as_array()
                .is_some_and(|r| r.iter().any(|v| v.as_str() == Some(field)));
            assert!(
                !declares_required,
                "{tool}: '{field}' is handler-defaulted — it must NOT be declared required[]"
            );
            assert!(
                validate_args(tool, def, args).is_none(),
                "{tool}: omitting handler-defaulted '{field}' must pass validation"
            );
        }
    }

    /// #1602: genuinely-required fields (the handler ERRORS on absence) stay
    /// enforced — the validator hard-rejects them with a named error.
    #[test]
    fn genuinely_required_fields_are_hard_rejected() {
        use crate::mcp::tools::*;
        // #3084: reply/send no longer top-level-require `message` (anyOf with
        // message_from_file); keep pins on tools that still hard-require fields.
        let cases = [
            ("delete_instance", def_delete_instance(), "instance"),
            ("task", def_task(), "action"),
        ];
        for case in &cases {
            let (tool, def, field) = (case.0, &case.1, case.2);
            let err = validate_args(tool, def, &json!({})).expect("must reject");
            assert_eq!(
                err["error"],
                format!("{tool}: missing required parameter '{field}'"),
                "{tool} must hard-reject its missing required field"
            );
        }
    }

    #[test]
    fn try_dispatch_rejects_reply_without_message() {
        // End-to-end: schema no longer requires `message` alone (#3084 anyOf),
        // so the handler rejects when neither message nor message_from_file is set.
        let home = std::env::temp_dir();
        let args = json!({}); // no message
        let ctx = ctx_for(&home, &args, "alpha");
        let result = try_dispatch("reply", &ctx).expect("registered tool returns Some");
        assert_eq!(
            result["error"], "missing 'message' or 'message_from_file'",
            "dispatch must reject reply with no message: {result}"
        );
    }

    // ── Rank8 bug-audit: present-but-JSON-null required field ───────────────
    // `{"message": null}` slipped through validation because `args.get(req)`
    // returns `Some(Value::Null)` (not `None`), so `is_none()` saw it as
    // "present". `handle_reply` then did `as_str().unwrap_or("")` → forwarded an
    // EMPTY string → opaque downstream channel rejection (Telegram 400) instead
    // of a clean early named error. The fix treats present-but-null as missing.

    #[test]
    fn validate_rejects_present_but_null_required_field() {
        // Rank8 pin: null required value rejects like missing. Use `task` (still
        // has a top-level required field after #3084 relaxed reply/send).
        let def = crate::mcp::tools::def_task();
        let err = validate_args("task", &def, &json!({"action": null}))
            .expect("a null required field must reject like a missing one");
        assert_eq!(
            err["error"], "task: missing required parameter 'action'",
            "present-but-null must reject with the same named error as missing: {err}"
        );
    }

    #[test]
    fn validate_allows_empty_string_required_field() {
        // Precision: ONLY JSON null counts as missing. A legit empty string is a
        // real present value (null=absent, ""=present) and must still pass, so
        // the fix never wrongly blocks a caller that means to send "".
        let def = crate::mcp::tools::def_reply();
        assert!(
            validate_args("reply", &def, &json!({"message": ""})).is_none(),
            "empty-string message is a present value, not null — must not be rejected"
        );
    }

    #[test]
    fn validate_rejects_null_for_all_genuinely_required_fields() {
        // The null-as-missing rule lives in validate_args, so it benefits EVERY
        // handler — not just reply. Mirror the genuinely-required cases.
        use crate::mcp::tools::*;
        let cases = [
            ("delete_instance", def_delete_instance(), "instance"),
            ("task", def_task(), "action"),
        ];
        for case in &cases {
            let (tool, def, field) = (case.0, &case.1, case.2);
            let mut obj = serde_json::Map::new();
            obj.insert(field.to_string(), serde_json::Value::Null);
            let args = serde_json::Value::Object(obj);
            let err = validate_args(tool, def, &args).expect("a null required field must reject");
            assert_eq!(
                err["error"],
                format!("{tool}: missing required parameter '{field}'"),
                "{tool}: null '{field}' must be rejected like missing"
            );
        }
    }

    #[test]
    fn try_dispatch_rejects_send_without_message_or_message_from_file() {
        // Handler-level send rejects when neither message nor
        // message_from_file is present (after selector/invariant checks pass).
        // Supply a valid sender (real fleet identity) to bypass the
        // identity gate and hit the message-validity gate.
        let home = std::env::temp_dir();
        let args = json!({"instance": "target"}); // valid selector, no message
        let ctx = HandlerCtx {
            home: &home,
            args: &args,
            instance_name: "alpha",
            sender: &Some(crate::identity::Sender::new("alpha").expect("valid sender name")),
            runtime: None,
        };
        let result = super::try_dispatch("send", &ctx).expect("registered tool returns Some");
        assert_eq!(
            result["error"], "missing or empty 'message'",
            "send must reject with no message or message_from_file: {result}"
        );
    }

    // #2454 Slice 5 RED: task health/sweep must consume the live registry
    // forwarded through the real MCP task dispatcher.  The current handler
    // still self-IPC's through the API (or reads runtime state from disk), so
    // these tests deterministically fail with no daemon/socket listener.
    fn runtime_with_external(name: &str) -> RuntimeContext {
        RuntimeContext {
            registry: std::sync::Arc::new(
                parking_lot::Mutex::new(std::collections::HashMap::new()),
            ),
            configs: Default::default(),
            externals: std::sync::Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::from([(
                    name.to_string(),
                    crate::agent::ExternalAgentHandle {
                        backend_command: "codex".to_string(),
                        pid: 4242,
                    },
                )]),
            )),
            notifier: None,
        }
    }

    fn task_runtime_ctx<'a>(
        home: &'a Path,
        args: &'a Value,
        runtime: &'a RuntimeContext,
    ) -> HandlerCtx<'a> {
        static EMPTY_SENDER: Option<Sender> = None;
        HandlerCtx {
            home,
            args,
            instance_name: "operator",
            sender: &EMPTY_SENDER,
            runtime: Some(runtime),
        }
    }

    fn backdate_task(home: &Path, task_id: &str) {
        let path = home.join("task_events.jsonl");
        let old = (chrono::Utc::now() - chrono::Duration::days(31)).to_rfc3339();
        let content = std::fs::read_to_string(&path).expect("task event log");
        let rewritten = content
            .lines()
            .map(|line| {
                let mut value: Value = serde_json::from_str(line).expect("task event JSON");
                if value["event"]["task_id"] == task_id {
                    value["timestamp"] = json!(old);
                }
                serde_json::to_string(&value).expect("serialized task event")
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(path, format!("{rewritten}\n")).expect("rewrite task event log");
    }

    #[test]
    fn task_health_uses_supplied_runtime_without_api_listener_2454() {
        let home = std::env::temp_dir().join(format!(
            "agend-task-health-runtime-red-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&home).expect("create temp home");
        let created = crate::tasks::handle(
            &home,
            "operator",
            &json!({
                "action": "create",
                "title": "runtime-owned task",
                "assignee": "live-agent"
            }),
        );
        let task_id = created["id"].as_str().expect("created task id");
        backdate_task(&home, task_id);

        let args = json!({"action": "health"});
        let runtime = runtime_with_external("live-agent");
        let ctx = task_runtime_ctx(&home, &args, &runtime);
        let result = try_dispatch("task", &ctx).expect("task dispatch result");
        assert_eq!(
            result["live_agents_available"], true,
            "health must use the supplied live RuntimeContext without API fallback: {result}"
        );
        let strict_owners = result["ghost_owners"]["strict_owners"]
            .as_array()
            .expect("strict owner list");
        assert!(
            !strict_owners.iter().any(|owner| owner == "live-agent"),
            "a supplied live owner must not be classified as strict/ghost: {result}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn task_sweep_uses_supplied_runtime_without_api_listener_2454() {
        let home = std::env::temp_dir().join(format!(
            "agend-task-sweep-runtime-red-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&home).expect("create temp home");
        let created = crate::tasks::handle(
            &home,
            "operator",
            &json!({
                "action": "create",
                "title": "runtime-owned stale task",
                "assignee": "live-agent"
            }),
        );
        let task_id = created["id"].as_str().expect("created task id");
        backdate_task(&home, task_id);

        let args = json!({"action": "sweep"});
        let runtime = runtime_with_external("live-agent");
        let ctx = task_runtime_ctx(&home, &args, &runtime);
        let result = try_dispatch("task", &ctx).expect("task dispatch result");
        assert_eq!(
            result["dry_run"], true,
            "sweep must return a dry-run plan: {result}"
        );
        let disbanded = result["categories"]["team_disbanded"]
            .as_array()
            .expect("team_disbanded category");
        assert!(
            !disbanded.iter().any(|candidate| candidate["id"] == task_id),
            "a supplied live owner must not be classified as team-disbanded: {result}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn task_health_and_sweep_runtime_none_are_explicit_errors_2454() {
        let home = std::env::temp_dir().join(format!(
            "agend-task-runtime-none-red-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&home).expect("create temp home");
        for action in ["health", "sweep"] {
            let args = json!({"action": action});
            let ctx = ctx_for(&home, &args, "operator");
            let result = try_dispatch("task", &ctx).expect("task dispatch result");
            assert!(
                result["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("runtime unavailable"),
                "task action={action} with runtime=None must fail explicitly, never socket-fallback: {result}"
            );
        }
        std::fs::remove_dir_all(home).ok();
    }

    // #2454 Slice 6 RED: move_pane must use the forwarded RuntimeContext
    // directly. The current adapter still calls the daemon MOVE_PANE socket,
    // so this real dispatch path deterministically fails without a listener.
    fn move_pane_runtime() -> RuntimeContext {
        RuntimeContext {
            registry: std::sync::Arc::new(
                parking_lot::Mutex::new(std::collections::HashMap::new()),
            ),
            configs: Default::default(),
            externals: std::sync::Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            )),
            notifier: None,
        }
    }

    fn move_pane_runtime_ctx<'a>(
        home: &'a Path,
        args: &'a Value,
        runtime: &'a RuntimeContext,
    ) -> HandlerCtx<'a> {
        static EMPTY_SENDER: Option<Sender> = None;
        HandlerCtx {
            home,
            args,
            instance_name: "operator",
            sender: &EMPTY_SENDER,
            runtime: Some(runtime),
        }
    }

    #[test]
    fn move_pane_uses_supplied_runtime_without_api_listener_2454() {
        let home = std::env::temp_dir().join(format!(
            "agend-move-pane-runtime-red-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&home).expect("create temp home");
        let args = json!({
            "instance": "agent-a",
            "target_tab": "team-x",
        });
        let runtime = move_pane_runtime();
        let ctx = move_pane_runtime_ctx(&home, &args, &runtime);
        let result = try_dispatch("move_pane", &ctx).expect("move_pane dispatch result");
        assert_eq!(
            result["ok"], true,
            "move_pane must succeed through RuntimeContext without an API listener: {result}"
        );
        assert_eq!(
            result["instance"], "agent-a",
            "move_pane target must be echoed"
        );
        assert_eq!(
            result["target_tab"], "team-x",
            "move_pane tab must be echoed"
        );

        let log = std::fs::read_to_string(home.join("event-log.jsonl"))
            .expect("runtime move_pane must emit one event-log record");
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(
            lines.len(),
            1,
            "move_pane must emit exactly one event: {lines:?}"
        );
        assert!(
            lines[0].contains("\"kind\":\"move_pane\"")
                && lines[0].contains("agent-a")
                && lines[0].contains("team-x")
                && lines[0].contains("Horizontal"),
            "move_pane event must preserve default split semantics: {lines:?}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn move_pane_vertical_split_and_notifier_contract_2454() {
        use crate::api::{ApiEvent, ApiNotifier, PaneMoveSplitDir};

        struct RecordingNotifier {
            events: parking_lot::Mutex<Vec<ApiEvent>>,
        }

        impl RecordingNotifier {
            fn new() -> Self {
                Self {
                    events: parking_lot::Mutex::new(Vec::new()),
                }
            }

            fn take(&self) -> Vec<ApiEvent> {
                std::mem::take(&mut *self.events.lock())
            }
        }

        impl ApiNotifier for RecordingNotifier {
            fn notify(&self, event: ApiEvent) {
                self.events.lock().push(event);
            }
        }

        let home = std::env::temp_dir().join(format!(
            "agend-move-pane-notifier-red-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&home).expect("create temp home");
        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();
        let notifier = std::sync::Arc::new(RecordingNotifier::new());
        let notifier_trait: std::sync::Arc<dyn crate::api::ApiNotifier> = notifier.clone();
        let ctx = crate::api::handlers::HandlerCtx {
            registry: &registry,
            configs: &configs,
            externals: &externals,
            notifier: Some(notifier_trait),
            home: &home,
        };

        let default = crate::api::handlers::instance::handle_move_pane(
            &json!({"agent": "agent-a", "target_tab": "team-x"}),
            &ctx,
        );
        assert_eq!(default["ok"], true, "API move_pane default must succeed");
        let vertical = crate::api::handlers::instance::handle_move_pane(
            &json!({
                "agent": "agent-b",
                "target_tab": "team-y",
                "split_dir": "vertical",
            }),
            &ctx,
        );
        assert_eq!(vertical["ok"], true, "API move_pane vertical must succeed");

        let events = notifier.take();
        assert_eq!(events.len(), 2, "default + vertical must emit two events");
        assert!(matches!(
            &events[0],
            ApiEvent::PaneMoved {
                agent,
                target_tab,
                split_dir: PaneMoveSplitDir::Horizontal,
            } if agent == "agent-a" && target_tab == "team-x"
        ));
        assert!(matches!(
            &events[1],
            ApiEvent::PaneMoved {
                agent,
                target_tab,
                split_dir: PaneMoveSplitDir::Vertical,
            } if agent == "agent-b" && target_tab == "team-y"
        ));
        let log = std::fs::read_to_string(home.join("event-log.jsonl"))
            .expect("API move_pane must emit event-log records");
        assert_eq!(
            log.lines().count(),
            2,
            "two move_pane calls must emit two records"
        );
        assert!(log
            .lines()
            .all(|line| line.contains("\"kind\":\"move_pane\"")));
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn move_pane_runtime_none_is_explicit_and_never_socket_fallback_2454() {
        let home = std::env::temp_dir().join(format!(
            "agend-move-pane-runtime-none-red-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&home).expect("create temp home");
        let args = json!({
            "instance": "agent-a",
            "target_tab": "team-x",
        });
        let ctx = ctx_for(&home, &args, "operator");
        let result = try_dispatch("move_pane", &ctx).expect("move_pane dispatch result");
        assert!(
            result["error"]
                .as_str()
                .unwrap_or_default()
                .contains("runtime unavailable"),
            "runtime=None must fail explicitly rather than use socket fallback: {result}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn move_pane_shared_service_and_no_api_call_invariants_2454() {
        let dispatch = include_str!("dispatch.rs");
        let production_dispatch = dispatch
            .split("#[cfg(test)]")
            .next()
            .expect("dispatch production source");
        assert!(
            !production_dispatch
                .contains("adapter!(dispatch_move_pane, ha, instance::handle_move_pane)"),
            "move_pane must use a runtime-aware custom dispatch adapter"
        );
        let runtime_start = production_dispatch
            .find("pub(crate) struct RuntimeContext {")
            .expect("RuntimeContext declaration");
        let runtime_end = production_dispatch[runtime_start..]
            .find("/// One MCP tool's dispatcher")
            .map(|offset| runtime_start + offset)
            .expect("RuntimeContext end marker");
        let runtime_region = &production_dispatch[runtime_start..runtime_end];
        assert!(
            runtime_region.contains("notifier"),
            "RuntimeContext must own a notifier for MCP move_pane"
        );
        let move_start = production_dispatch
            .find("pub(crate) fn dispatch_move_pane(")
            .expect("runtime-aware dispatch_move_pane declaration");
        let move_end = production_dispatch[move_start..]
            .find("pub(crate) fn dispatch_usage_limit_takeover(")
            .map(|offset| move_start + offset)
            .expect("dispatch_move_pane end marker");
        let move_region = &production_dispatch[move_start..move_end];
        assert!(
            move_region.contains("notifier"),
            "dispatch_move_pane must forward RuntimeContext notifier"
        );

        let mcp = include_str!("instance_metadata.rs");
        let mcp_start = mcp
            .find("pub(super) fn handle_move_pane(")
            .expect("MCP move_pane handler");
        let mcp_end = mcp[mcp_start..]
            .find("pub(super) fn handle_pane_snapshot(")
            .map(|offset| mcp_start + offset)
            .expect("MCP pane_snapshot handler");
        let mcp_region = &mcp[mcp_start..mcp_end];
        assert!(
            mcp_region.contains("agent_ops::move_pane"),
            "MCP move_pane must call the shared neutral service"
        );
        assert!(
            !mcp_region.lines().any(|line| {
                let trimmed = line.trim();
                !trimmed.starts_with("//") && trimmed.contains("api::call")
            }),
            "MCP move_pane production region must contain no api::call"
        );

        let api = include_str!("../../api/handlers/instance.rs");
        let api_start = api
            .find("pub(crate) fn handle_move_pane(")
            .expect("API move_pane handler");
        let api_end = api[api_start..]
            .find("pub(crate) fn handle_set_blocked_reason(")
            .map(|offset| api_start + offset)
            .expect("API blocked-reason handler");
        let api_region = &api[api_start..api_end];
        assert!(
            api_region.contains("agent_ops::move_pane"),
            "API move_pane must call the same shared neutral service"
        );
    }

    // ── #2454 Slice 8: delayed async INJECT loopback RED ──────────────

    /// #2454 S8: runtime=Some routes through inject_input (succeeds with
    /// a live mock agent, no daemon needed).
    #[test]
    fn inject_with_routing_runtime_some_uses_inject_input_2454() {
        use crate::mcp::handlers::instance_state::spawn::inject_with_routing;
        let home =
            std::env::temp_dir().join(format!("agend-inject-routing-some-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let registry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let (handle, _reader) = crate::daemon::per_tick::mock_live_agent_no_context("agent-x");
        let id = handle.id;
        registry.lock().insert(id, handle);
        let externals =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!("instances:\n  agent-x:\n    id: {}\n", id.full()),
        )
        .unwrap();
        let rt_arcs = (registry, externals);
        let result = inject_with_routing(&home, "agent-x", b"hello", Some(&rt_arcs));
        assert!(
            result.is_ok(),
            "runtime=Some must succeed in-process without a daemon: {result:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2454 S8: runtime=None falls back to legacy api::call (fails with
    /// no daemon — proving it actually tries the socket path).
    #[test]
    fn inject_with_routing_runtime_none_uses_api_call_2454() {
        use crate::mcp::handlers::instance_state::spawn::inject_with_routing;
        let home =
            std::env::temp_dir().join(format!("agend-inject-routing-none-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let result = inject_with_routing(&home, "any-agent", b"hello", None);
        assert!(
            result.is_err(),
            "runtime=None must attempt api::call (fails with no daemon): {result:?}"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("daemon") || err.contains("run dir"),
            "runtime=None error must come from api::call path: {err}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Strip the test-only tail from a source file without treating `#[cfg(test)]`
    /// helper statics as the boundary.  The inventory below is intentionally
    /// generated from the checked-out production sources rather than a fixed
    /// syntax budget: compatibility transports are valid when their runtime
    /// branch is absent, and the successor STATUS call is cross-daemon.
    fn production_tail(source: &str) -> &str {
        let lines: Vec<&str> = source.lines().collect();
        let mut boundary = lines.len();
        for (index, line) in lines.iter().enumerate() {
            if line.trim() != "#[cfg(test)]" {
                continue;
            }
            let next = lines
                .iter()
                .skip(index + 1)
                .take(4)
                .map(|line| line.trim())
                .find(|line| !line.starts_with('#'))
                .unwrap_or("");
            let module_name = next
                .strip_prefix("mod ")
                .and_then(|name| name.split_whitespace().next())
                .unwrap_or("");
            if module_name == "tests" || module_name.ends_with("_tests") {
                boundary = index;
                break;
            }
        }
        // Reconstruct the byte boundary from the original source chunks so
        // CRLF checkouts and non-ASCII lines cannot drift into the middle of a
        // UTF-8 code point. `str::len()` is byte-based, and each inclusive
        // chunk carries its exact newline width (`\n` or `\r\n`).
        let end: usize = source
            .split_inclusive('\n')
            .take(boundary)
            .map(str::len)
            .sum();
        &source[..end]
    }

    fn code_without_strings_or_comments(line: &str) -> String {
        let mut result = String::with_capacity(line.len());
        let mut quoted = false;
        let mut escaped = false;
        let bytes = line.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            if quoted {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    quoted = false;
                }
                result.push(' ');
            } else if byte == b'"' {
                quoted = true;
                result.push(' ');
            } else if byte == b'/' && bytes.get(index + 1) == Some(&b'/') {
                break;
            } else {
                result.push(byte as char);
            }
            index += 1;
        }
        result
    }

    fn function_name(line: &str) -> Option<String> {
        let marker = line.find("fn ")? + 3;
        let name: String = line[marker..]
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
            .collect();
        (!name.is_empty()).then_some(name)
    }

    fn source_region<'a>(source: &'a str, function: &str) -> &'a str {
        let marker = format!("fn {function}");
        let start = source
            .find(&marker)
            .unwrap_or_else(|| panic!("missing source function {function}"));
        let body_start = source[start..]
            .find('{')
            .map(|offset| start + offset)
            .unwrap_or_else(|| panic!("missing body for source function {function}"));
        let bytes = source.as_bytes();
        let mut depth = 0usize;
        let mut quoted = false;
        let mut escaped = false;
        let mut char_literal = false;
        let mut raw_hashes = None;
        let mut line_comment = false;
        let mut block_comment_depth = 0usize;
        let mut index = body_start;
        while index < bytes.len() {
            let byte = bytes[index];
            let next = bytes.get(index + 1).copied();
            if line_comment {
                if byte == b'\n' {
                    line_comment = false;
                }
            } else if block_comment_depth > 0 {
                if byte == b'/' && next == Some(b'*') {
                    block_comment_depth += 1;
                    index += 1;
                } else if byte == b'*' && next == Some(b'/') {
                    block_comment_depth -= 1;
                    index += 1;
                }
            } else if let Some(hashes) = raw_hashes {
                if byte == b'"'
                    && bytes[index + 1..]
                        .iter()
                        .take_while(|candidate| **candidate == b'#')
                        .count()
                        == hashes
                {
                    raw_hashes = None;
                    index += hashes;
                }
            } else if quoted {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    quoted = false;
                }
            } else if char_literal {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'\'' {
                    char_literal = false;
                }
            } else if byte == b'/' && next == Some(b'/') {
                line_comment = true;
                index += 1;
            } else if byte == b'/' && next == Some(b'*') {
                block_comment_depth = 1;
                index += 1;
            } else if let Some((quote_index, hashes)) = {
                let (prefix_index, prefix_len) = if byte == b'r' {
                    (index, 1)
                } else if byte == b'b' && next == Some(b'r') {
                    (index, 2)
                } else {
                    (index, 0)
                };
                if prefix_len == 0 {
                    None
                } else {
                    let mut cursor = prefix_index + prefix_len;
                    while bytes.get(cursor) == Some(&b'#') {
                        cursor += 1;
                    }
                    (bytes.get(cursor) == Some(&b'"'))
                        .then_some((cursor, cursor - prefix_index - prefix_len))
                }
            } {
                raw_hashes = Some(hashes);
                index = quote_index;
            } else if byte == b'\'' && (next == Some(b'\\') || bytes.get(index + 2) == Some(&b'\''))
            {
                char_literal = true;
            } else if byte == b'"' {
                quoted = true;
            } else if byte == b'{' {
                depth += 1;
            } else if byte == b'}' {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return &source[start..=index];
                }
            }
            index += 1;
        }
        panic!("unterminated body for source function {function}");
    }

    fn transport_inventory() -> Vec<(String, usize, String, String)> {
        fn walk(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, files);
                } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                    files.push(path);
                }
            }
        }

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&root, &mut files);
        files.sort();
        let mut inventory = Vec::new();
        for path in files {
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let production = production_tail(&source);
            let relative = path
                .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                .unwrap_or(&path)
                .display()
                .to_string()
                .replace('\\', "/");
            let mut current_function = String::from("<module>");
            for (line_number, line) in production.lines().enumerate() {
                let code = code_without_strings_or_comments(line);
                if let Some(name) = function_name(&code) {
                    current_function = name;
                }
                let kind = if code.contains("api::call_at(") {
                    "call_at"
                } else if code.contains("api::call(") {
                    "call"
                } else if code.contains("request_local(") {
                    "request_local"
                } else {
                    continue;
                };
                inventory.push((
                    relative.clone(),
                    line_number + 1,
                    current_function.clone(),
                    kind.to_string(),
                ));
            }
        }
        inventory
    }

    fn classify_transport(path: &str, function: &str, kind: &str) -> &'static str {
        if path == "src/mcp/handlers/restart.rs" && function == "phase1_gate" && kind == "call_at" {
            return "cross_daemon_successor_status";
        }
        if path == "src/mcp/handlers/instance_state/lifecycle.rs"
            && function == "delete_with_runtime_or_legacy"
        {
            return "runtime_none_instance_delete";
        }
        if path == "src/mcp/handlers/instance_state/spawn.rs" && function == "inject_with_routing" {
            return "runtime_none_instance_inject";
        }
        if path == "src/mcp/handlers/instance_state/spawn.rs" && function == "legacy_spawn" {
            return "runtime_none_instance_spawn";
        }
        if path == "src/deployments.rs" && function == "spawn_instances_legacy" {
            return "runtime_none_deployment_spawn";
        }
        if path == "src/deployments.rs" && function == "create_deployment_team_legacy" {
            return "runtime_none_deployment_create_team";
        }
        if path == "src/deployments.rs" && function == "delete_instances_legacy" {
            return "runtime_none_deployment_delete";
        }
        if path == "src/agent_ops.rs" && function == "send_via_api_bridge" {
            return "runtime_none_send_bridge";
        }
        if path == "src/tasks/handler.rs" && function == "handle_sweep" {
            return "runtime_none_task_sweep_wrapper";
        }
        if path == "src/runtime.rs" && function == "list_live_agents" {
            return "shared_runtime_none_list_wrapper";
        }
        if path == "src/inbox/notify.rs" && function == "inject_with_submit_direct" {
            return "async_delivery_worker_inject";
        }
        if path == "src/agent/mod.rs" {
            return "agent_lifecycle_shell_fallback";
        }
        if path.starts_with("src/channel/telegram/") {
            return "telegram_channel_caller";
        }
        if matches!(
            path,
            "src/connect.rs"
                | "src/bugreport.rs"
                | "src/main.rs"
                | "src/tray/mod.rs"
                | "src/verify.rs"
        ) {
            return "standalone_or_operator_client";
        }
        panic!("unclassified production transport candidate: {path}:{function} ({kind})");
    }

    /// #2454 closure RED: generated inventory must classify every production
    /// transport candidate by runtime/process reachability.  The counts are
    /// derived from the scan; only the six compatibility sites and the one
    /// explicitly cross-daemon call are pinned by disposition.
    #[test]
    fn generated_production_transport_inventory_is_classified_2454() {
        let inventory = transport_inventory();
        assert!(
            !inventory.is_empty(),
            "production transport inventory is empty"
        );
        let mut counts = std::collections::BTreeMap::<&str, usize>::new();
        for (path, _line, function, kind) in &inventory {
            *counts
                .entry(classify_transport(path, function, kind))
                .or_default() += 1;
        }
        assert_eq!(counts.get("cross_daemon_successor_status"), Some(&1));
        assert_eq!(counts.get("runtime_none_instance_delete"), Some(&1));
        assert_eq!(counts.get("runtime_none_instance_inject"), Some(&1));
        assert_eq!(counts.get("runtime_none_instance_spawn"), Some(&1));
        assert_eq!(counts.get("runtime_none_deployment_spawn"), Some(&1));
        assert_eq!(counts.get("runtime_none_deployment_create_team"), Some(&1));
        assert_eq!(counts.get("runtime_none_deployment_delete"), Some(&1));
        assert_eq!(counts.get("runtime_none_send_bridge"), Some(&1));
        assert_eq!(counts.get("runtime_none_task_sweep_wrapper"), Some(&1));
        assert_eq!(counts.get("shared_runtime_none_list_wrapper"), Some(&1));
    }

    /// The in-process MCP task adapter must never call the public LIST wrappers
    /// (which retain a socket fallback).  This also covers helper indirection:
    /// it must snapshot the injected registries and call the typed task entry.
    #[test]
    fn runtime_present_task_health_and_sweep_bypass_public_api_wrappers_2454() {
        let source = include_str!("dispatch.rs");
        let dispatch = source
            .split("pub(crate) fn dispatch_task")
            .nth(1)
            .and_then(|region| {
                region
                    .split("pub(crate) fn dispatch_usage_limit_takeover")
                    .next()
            })
            .expect("dispatch_task source region");
        assert!(dispatch.contains("agent_ops::list_snapshot"));
        assert!(dispatch.contains("tasks::handle_with_live_instances"));
        assert!(!dispatch.contains("tasks::handle_sweep("));
        assert!(!dispatch.contains("tasks::handle_health("));
    }

    /// Pin the branch/function-pointer/delayed-closure seams that sit between
    /// a real RuntimeContext=Some MCP ingress and the compatibility transports.
    /// This is deliberately structural: the generated inventory above records
    /// that the legacy calls exist, while this guard proves their owner branch
    /// is selected only when the runtime value is absent.
    #[test]
    fn runtime_present_compatibility_branches_are_structurally_unreachable_2454() {
        let spawn = include_str!("instance_state/spawn.rs");
        let inject = source_region(spawn, "inject_with_routing");
        assert!(inject.contains("if let Some((reg, ext))"));
        assert!(inject.contains("agent_ops::inject_input"));
        assert!(inject.contains("crate::api::call"));
        let spawn_route = source_region(spawn, "spawn_runtime_or_legacy");
        assert!(spawn_route.contains("let Some(runtime) = runtime else"));
        assert!(spawn_route.contains("return legacy(home, request)"));
        assert!(spawn_route.contains("spawn_instance"));

        let lifecycle = include_str!("instance_state/lifecycle.rs");
        let delete = source_region(lifecycle, "delete_with_runtime_or_legacy");
        assert!(delete.contains("if let Some(context)"));
        assert!(delete.contains("agent_ops::delete_instance"));
        assert!(delete.contains("crate::api::call"));

        let deployments = include_str!("../../deployments.rs");
        for (typed, legacy) in [
            ("spawn_instances", "spawn_instances_legacy"),
            ("create_deployment_team", "create_deployment_team_legacy"),
            ("teardown_with_runtime", "delete_instances_legacy"),
        ] {
            let typed_region = source_region(deployments, typed);
            let legacy_region = source_region(deployments, legacy);
            assert!(
                typed_region.contains("if let Some(runtime)")
                    || typed_region.contains("if let Some(context)"),
                "typed deployment owner {typed} must branch on runtime"
            );
            assert!(
                legacy_region.contains("crate::api::call"),
                "legacy deployment adapter {legacy} must retain the compatibility transport"
            );
        }

        let comms = include_str!("comms.rs");
        let send = source_region(comms, "handle_send_to_instance");
        assert!(send.contains("messaging::execute_send"));
        assert!(send.contains("send_via_api_bridge"));
        let delegate = include_str!("comms_delegate/mod.rs");
        assert!(delegate.contains("messaging::execute_send"));

        let state = include_str!("instance_state/mod.rs");
        assert!(state.contains("inject_with_routing"));
        assert!(state.contains("Arc::clone(&rt.registry)"));
        assert!(state.contains("Arc::clone(&rt.externals)"));
        assert!(
            source_region(include_str!("dispatch.rs"), "dispatch_deployment")
                .contains("ctx.runtime")
        );
    }

    #[test]
    fn source_region_is_brace_bounded_2454() {
        let source = concat!(
            "fn first() {\n",
            " let char_brace = '}';\n",
            " let after_char = \"AFTER_CHAR\";\n",
            " let raw = r###\"{ raw }\"###;\n",
            " let after_raw = \"AFTER_RAW\";\n",
            " /* outer { /* nested } */ still } */\n",
            " let after_comment = \"AFTER_COMMENT\";\n",
            " let sentinel = \"FIRST_SENTINEL\";\n",
            "}\n",
            "pub async fn second() { panic!(\"SECOND_SENTINEL\") }\n"
        );
        let first = source_region(source, "first");
        assert!(first.contains("AFTER_CHAR"));
        assert!(first.contains("AFTER_RAW"));
        assert!(first.contains("AFTER_COMMENT"));
        assert!(first.contains("FIRST_SENTINEL"));
        assert!(!first.contains("SECOND_SENTINEL"));
    }

    #[test]
    fn production_tail_preserves_utf8_crlf_boundary_2454() {
        let source = "pub fn first() {\r\n    let marker = \"—\";\r\n} // —\r\n#[cfg(test)]\r\nmod tests {\r\n    fn ignored() {}\r\n}\r\n";
        let production = production_tail(source);
        assert!(production.contains("let marker = \"—\""));
        assert!(!production.contains("mod tests"));
    }

    /// All team actions are part of the MCP runtime-present closure.  The
    /// current exact-main adapters drop RuntimeContext for list/delete; this
    /// guard intentionally stays RED until those two synchronous paths are
    /// routed through typed owners as well.
    #[test]
    fn runtime_present_team_actions_forward_runtime_2454() {
        let source = include_str!("dispatch.rs");
        let dispatch = source
            .split("pub(crate) fn dispatch_team")
            .nth(1)
            .and_then(|region| region.split("// `inbox` —").next())
            .expect("dispatch_team source region");
        assert!(
            dispatch.contains("handle_delete_team(ctx.home, ctx.args, ctx.runtime)"),
            "team delete must retain runtime context for the in-process cascade"
        );
        assert!(
            dispatch.contains("handle_list_teams(ctx.home, ctx.runtime)"),
            "team list must retain runtime context for the in-process live snapshot"
        );
    }

    /// The stale dispatch_interrupt comment must describe the typed injection
    /// route, not claim that INJECT remains a loopback after runtime forwarding.
    #[test]
    fn dispatch_interrupt_comment_matches_runtime_inject_route_2454() {
        let source = include_str!("dispatch.rs");
        let obsolete = ["its INJECT", " stays a loopback"].concat();
        assert!(
            !source.contains(&obsolete),
            "dispatch_interrupt still documents an obsolete INJECT loopback"
        );
    }

    /// #2454 Slice 13 RED: the neutral typed CREATE_TEAM service must own
    /// the team-creation logic, and both MCP and API handlers must route
    /// through thin adapters to it — not call teams::create directly.
    #[test]
    fn create_team_neutral_service_owns_logic_2454() {
        let task_src = include_str!("task.rs");
        let test_boundary = task_src
            .rfind("#[cfg(test)]\nmod ")
            .unwrap_or(task_src.len());
        let production = &task_src[..test_boundary];
        let create_fn_start = production
            .find("fn handle_create_team(")
            .expect("MCP handle_create_team must exist");
        let create_fn_end = production[create_fn_start..]
            .find("\n}\n")
            .map(|o| create_fn_start + o)
            .unwrap_or(production.len());
        let create_fn = &production[create_fn_start..create_fn_end];
        assert!(
            !create_fn.contains("api::call"),
            "#2454: MCP handle_create_team must not contain api::call: \
             it must route through the neutral typed service"
        );
        assert!(
            !create_fn.contains("teams::create("),
            "#2454: MCP handle_create_team must not call teams::create directly: \
             it must route through the neutral typed service"
        );
    }

    /// #2454 Slice 13 RED: both API and MCP CREATE_TEAM handlers must route
    /// through one shared neutral typed service. The neutral service must NOT
    /// take HandlerCtx or raw serde_json::Value as its primary boundary type
    /// — it must own a typed domain interface. Wrapping old logic behind a
    /// same-named raw-Value helper cannot pass this guard.
    #[test]
    fn create_team_both_adapters_share_neutral_typed_owner_2454() {
        let neutral_marker = "team_ops::create";

        // ── Adapter convergence: both MCP and API must call the same owner ──

        let mcp_src = include_str!("task.rs");
        let mcp_boundary = mcp_src.rfind("#[cfg(test)]\nmod ").unwrap_or(mcp_src.len());
        let mcp_prod = &mcp_src[..mcp_boundary];
        let mcp_fn_start = mcp_prod
            .find("fn handle_create_team(")
            .expect("MCP handle_create_team");
        let mcp_fn_end = mcp_prod[mcp_fn_start..]
            .find("\n}\n")
            .map(|o| mcp_fn_start + o)
            .unwrap_or(mcp_prod.len());
        let mcp_fn = &mcp_prod[mcp_fn_start..mcp_fn_end];

        let api_src = include_str!("../../api/handlers/team.rs");
        let api_boundary = api_src
            .rfind("#[cfg(test)]\n#[allow")
            .unwrap_or(api_src.len());
        let api_prod = &api_src[..api_boundary];
        let api_fn_start = api_prod
            .find("fn handle_create_team(")
            .expect("API handle_create_team");
        let api_fn_end = api_prod[api_fn_start..]
            .find("\n}\n")
            .map(|o| api_fn_start + o)
            .unwrap_or(api_prod.len());
        let api_fn = &api_prod[api_fn_start..api_fn_end];

        assert!(
            mcp_fn.contains(neutral_marker),
            "#2454: MCP handle_create_team must call neutral typed service \
             `{neutral_marker}`, not own the logic"
        );
        assert!(
            api_fn.contains(neutral_marker),
            "#2454: API handle_create_team must call neutral typed service \
             `{neutral_marker}`, not own the logic"
        );
        assert!(
            !mcp_fn.contains("HandlerCtx"),
            "#2454: MCP CREATE_TEAM adapter must not pass HandlerCtx to the service"
        );

        // ── Owner boundary: the neutral service's definition must be typed ──
        // Scan the module that should own the neutral service. Its `pub fn
        // create` (or `pub(crate) fn create`) signature must NOT accept
        // `HandlerCtx` or raw `Value`/`&Value` as a parameter — the boundary
        // must be typed domain types. A same-named helper that just wraps
        // teams::create with a raw Value interface cannot pass.

        // team_ops module must exist as a file
        let team_ops_exists =
            std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/team_ops.rs")).exists()
                || std::path::Path::new(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/src/team_ops/mod.rs"
                ))
                .exists();
        assert!(
            team_ops_exists,
            "#2454: neutral typed service module `team_ops` must exist as src/team_ops.rs or src/team_ops/mod.rs"
        );

        // Read the module and inspect the create function signature
        let team_ops_path =
            if std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/team_ops.rs"))
                .exists()
            {
                concat!(env!("CARGO_MANIFEST_DIR"), "/src/team_ops.rs")
            } else {
                concat!(env!("CARGO_MANIFEST_DIR"), "/src/team_ops/mod.rs")
            };
        let team_ops_src =
            std::fs::read_to_string(team_ops_path).expect("read team_ops module source");
        let create_fn_start = team_ops_src
            .find("fn create(")
            .or_else(|| team_ops_src.find("fn create_team("))
            .expect("team_ops must define a create/create_team function");
        // Extract the signature line (up to the opening brace)
        let sig_end = team_ops_src[create_fn_start..]
            .find('{')
            .map(|o| create_fn_start + o)
            .unwrap_or(team_ops_src.len());
        let signature = &team_ops_src[create_fn_start..sig_end];
        assert!(
            !signature.contains("HandlerCtx"),
            "#2454: team_ops::create signature must not accept HandlerCtx \
             (framework-coupled boundary): {signature}"
        );
        assert!(
            !signature.contains("&Value") && !signature.contains(": Value"),
            "#2454: team_ops::create signature must not accept raw serde_json::Value \
             (untyped boundary — wrapping old logic behind a same-named helper): {signature}"
        );
    }

    /// Wiring pin: both fire-and-forget thread bodies must call the shared
    /// `inject_with_routing` helper (not inline api::call or inject_input).
    #[test]
    fn both_delayed_inject_threads_call_shared_helper_2454() {
        let helper = concat!("inject_with_", "routing");
        let mod_src = include_str!("instance_state/mod.rs");
        let team_start = mod_src
            .find("\"team_task_inject\"")
            .expect("team_task_inject thread marker");
        let team_end = team_start + 400.min(mod_src.len() - team_start);
        let team_begin = team_start.saturating_sub(400);
        let team_region = &mod_src[team_begin..team_end];
        assert!(
            team_region.contains(helper),
            "team_task_inject region must call inject_with_routing"
        );

        let spawn_src = include_str!("instance_state/spawn.rs");
        let task_start = spawn_src
            .find("\"task_inject\"")
            .expect("task_inject thread marker");
        let task_end = task_start + 400.min(spawn_src.len() - task_start);
        let task_begin = task_start.saturating_sub(1000);
        let task_region = &spawn_src[task_begin..task_end];
        assert!(
            task_region.contains(helper),
            "task_inject region must call inject_with_routing"
        );
    }
}

#[cfg(test)]
mod review_repro_mcp_dispatch_comms;
