//! #1440 agent-backend env isolation — allowlist + credential passthrough.

use crate::backend::Backend;

/// Environment variable names that fleet.yaml-supplied `env:` maps are NOT
/// allowed to override when spawning an agent. These either (a) carry
/// credentials that only the host user should control, (b) govern dynamic
/// linking and would let a hostile fleet.yaml load attacker-supplied code
/// into the spawned process, or (c) are agend's own runtime plumbing.
///
/// Matching is case-insensitive for cross-platform safety: Windows env is
/// case-insensitive, so `anthropic_api_key` and `ANTHROPIC_API_KEY` map to the
/// same variable there, and a pure case-sensitive deny-list would miss it.
const SENSITIVE_ENV_KEYS: &[&str] = &[
    // API credentials for backends we drive
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
    // Cloud credentials commonly present in dev environments
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    // Git forge tokens
    "GITHUB_TOKEN",
    "GITLAB_TOKEN",
    "NPM_TOKEN",
    // Dynamic-linker injection vectors (Linux / macOS)
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    // agend's own runtime wiring — overriding these lets a template redirect
    // the spawned agent to a different home / break MCP config discovery
    "AGEND_HOME",
    "AGEND_INSTANCE_NAME",
];

/// #1440: base runtime env allowlist — the minimum any agent CLI needs to
/// launch and reach its provider. Injected only if present in the daemon env
/// (so Windows-only keys are harmless on Unix and vice versa). Corp-specific
/// extras (`NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`, …) are intentionally absent
/// — operators name those in `passthrough_env`, keeping the default minimal
/// and auditable.
const BASE_ENV_ALLOWLIST: &[&str] = &[
    // Identity / shell / paths
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "PATH",
    // Locale
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "TZ",
    // Temp dirs
    "TMPDIR",
    "TMP",
    "TEMP",
    // Agent IO / auth socket
    "SSH_AUTH_SOCK",
    // XDG base dirs
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
    // Proxies (lower + upper case both seen in the wild)
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    // Windows essentials — inject-if-present; absent on Unix. Without these a
    // child process fails to start on Windows (CI runs windows-latest).
    "SYSTEMROOT",
    "SystemDrive",
    "windir",
    "PATHEXT",
    "COMSPEC",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
];

/// Returns true if the env-var name is on the spawn-time deny-list.
pub fn is_sensitive_env_key(key: &str) -> bool {
    SENSITIVE_ENV_KEYS
        .iter()
        .any(|denied| denied.eq_ignore_ascii_case(key))
}

/// #1440: is agent-backend env isolation enabled? Default OFF (phased rollout
/// — this version does not change default spawn behavior).
pub fn env_isolation_enabled() -> bool {
    std::env::var("AGEND_ENV_ISOLATION").as_deref() == Ok("1")
}

/// #1440: outcome of [`resolve_child_env`].
pub struct ChildEnvPlan {
    /// `(key, value)` pairs to inject into the child after `env_clear()`.
    pub injected: Vec<(String, String)>,
    /// Names of source-env vars that would NOT survive isolation (warn input).
    pub dropped: Vec<String>,
}

/// Case-insensitive membership (matches `is_sensitive_env_key`; also handles
/// Windows' case-insensitive env-var names, e.g. `Path` vs `PATH`).
pub(crate) fn env_key_in(key: &str, list: &[&str]) -> bool {
    list.iter().any(|k| k.eq_ignore_ascii_case(key))
}

/// #1440: PURE — decide which inherited env vars survive isolation. A var
/// survives iff it is in the base allowlist, OR a credential key the detected
/// backend declares (these intentionally override `SENSITIVE_ENV_KEYS` for the
/// owning backend only → cross-backend credential isolation), OR an operator
/// `passthrough` key that is NOT itself sensitive (so `LD_PRELOAD` stays
/// blocked even if listed). `source_env` is a snapshot — the real daemon env
/// in production, an injected map in tests.
pub fn resolve_child_env(
    backend: Option<&Backend>,
    passthrough: &[String],
    source_env: &std::collections::BTreeMap<String, String>,
) -> ChildEnvPlan {
    let creds: &[&str] = backend.map(|b| b.credential_env_keys()).unwrap_or(&[]);
    let pass: Vec<&str> = passthrough.iter().map(String::as_str).collect();
    let mut injected = Vec::new();
    let mut dropped = Vec::new();
    for (k, v) in source_env {
        let allowed = env_key_in(k, BASE_ENV_ALLOWLIST)
            || env_key_in(k, creds)
            || (env_key_in(k, &pass) && !is_sensitive_env_key(k));
        if allowed {
            injected.push((k.clone(), v.clone()));
        } else {
            dropped.push(k.clone());
        }
    }
    injected.sort();
    dropped.sort();
    ChildEnvPlan { injected, dropped }
}

/// #1440: one-time warning listing inherited env var KEY NAMES (never values)
/// that would be dropped under isolation. Lets operators preview the impact
/// before flipping `AGEND_ENV_ISOLATION=1`.
pub(crate) fn warn_env_isolation_disabled_once(dropped: &[String]) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        if dropped.is_empty() {
            return;
        }
        tracing::warn!(
            dropped_keys = %dropped.join(", "),
            "AGEND_ENV_ISOLATION off: agent backends inherit the full daemon env. \
             Under isolation these inherited keys would be dropped (names only). \
             Opt in with AGEND_ENV_ISOLATION=1 + passthrough_env."
        );
    });
}
