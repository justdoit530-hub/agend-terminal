//! API and MCP handler for agy subscription quota querying.

use serde_json::{json, Value};
use std::process::Command;
use std::path::Path;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ModelQuota {
    pub(crate) label: String,
    #[serde(rename = "remainingFraction")]
    pub(crate) remaining_fraction: f32,
    #[serde(rename = "resetTime")]
    pub(crate) reset_time: Option<String>,
}

#[derive(serde::Deserialize)]
struct GetUserStatusResponse {
    #[serde(rename = "userStatus")]
    user_status: Option<UserStatus>,
}

#[derive(serde::Deserialize)]
struct UserStatus {
    #[serde(rename = "cascadeModelConfigData")]
    cascade_model_config_data: Option<CascadeModelConfigData>,
}

#[derive(serde::Deserialize)]
struct CascadeModelConfigData {
    #[serde(rename = "clientModelConfigs")]
    client_model_configs: Option<Vec<ClientModelConfig>>,
}

#[derive(serde::Deserialize)]
struct ClientModelConfig {
    label: Option<String>,
    #[serde(rename = "quotaInfo")]
    quota_info: Option<QuotaInfo>,
}

#[derive(serde::Deserialize)]
struct QuotaInfo {
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f32>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
}

pub(crate) fn detect_language_server() -> Option<(u16, String)> {
    // 1. Find the PID
    let output = Command::new("pgrep")
        .args(["-f", "language_server.*subclient_type hub"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let pid_str = stdout.lines().next()?.trim();
    let pid: u32 = pid_str.parse().ok()?;

    // 2. Find the port via lsof
    let lsof_output = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-i", "-n", "-P"])
        .output()
        .ok()?;
    if !lsof_output.status.success() {
        return None;
    }
    let lsof_stdout = String::from_utf8_lossy(&lsof_output.stdout);
    let mut port = None;
    for line in lsof_stdout.lines() {
        if line.contains("LISTEN") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            for part in parts {
                if part.contains(':') {
                    if let Some(port_str) = part.split(':').last() {
                        if let Ok(p) = port_str.parse::<u16>() {
                            port = Some(p);
                            break;
                        }
                    }
                }
            }
            if port.is_some() {
                break;
            }
        }
    }
    let port = port?;

    // 3. Find CSRF token from process cmdline
    let ps_output = Command::new("ps")
        .args(["-ww", "-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !ps_output.status.success() {
        return None;
    }
    let ps_stdout = String::from_utf8_lossy(&ps_output.stdout);
    let cmdline = ps_stdout.trim();
    let args: Vec<&str> = cmdline.split_whitespace().collect();
    let mut csrf_token = None;
    for i in 0..args.len() {
        if args[i] == "--csrf_token" && i + 1 < args.len() {
            csrf_token = Some(args[i + 1].to_string());
            break;
        }
    }
    let csrf_token = csrf_token?;

    Some((port, csrf_token))
}

pub(crate) async fn fetch_agy_quota() -> Option<Vec<ModelQuota>> {
    let (port, token) = match detect_language_server() {
        Some(x) => x,
        None => {
            tracing::warn!("detect_language_server failed");
            return None;
        }
    };
    let client = match reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("reqwest build failed: {:?}", e);
            return None;
        }
    };
    let url = format!(
        "https://127.0.0.1:{}/exa.language_server_pb.LanguageServerService/GetUserStatus",
        port
    );
    let resp = match client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .header("X-Codeium-Csrf-Token", token)
        .json(&json!({}))
        .send()
        .await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("reqwest send failed: {:?}", e);
            return None;
        }
    };

    if !resp.status().is_success() {
        tracing::warn!("HTTP status error: {:?}", resp.status());
        return None;
    }

    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("failed to read response text: {:?}", e);
            return None;
        }
    };

    let payload: GetUserStatusResponse = match serde_json::from_str(&text) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("serde parse failed: {:?}, raw text: {}", e, text);
            return None;
        }
    };
    let configs = match payload
        .user_status
        .and_then(|us| us.cascade_model_config_data)
        .and_then(|cmd| cmd.client_model_configs) {
        Some(c) => c,
        None => {
            tracing::warn!("cascade_model_config_data or client_model_configs missing in payload");
            return None;
        }
    };

    let mut out = Vec::new();
    for config in configs {
        let label = config.label.unwrap_or_default();
        if let Some(quota) = config.quota_info {
            out.push(ModelQuota {
                label,
                remaining_fraction: quota.remaining_fraction.unwrap_or(0.0),
                reset_time: quota.reset_time,
            });
        }
    }
    Some(out)
}

fn block_on_value<F>(fut: F) -> F::Output
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|s| {
            s.spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to build nested tokio runtime for agy_quota")
                    .block_on(fut)
            })
            .join()
            .expect("nested block_on thread panicked for agy_quota")
        })
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to build tokio runtime for agy_quota")
            .block_on(fut)
    }
}

pub(crate) fn handle_agy_quota(_home: &Path, _args: &Value) -> Value {
    match block_on_value(fetch_agy_quota()) {
        Some(quotas) => json!({
            "ok": true,
            "models": quotas,
        }),
        None => json!({
            "ok": false,
            "error": "Failed to fetch quota from Google Antigravity language server",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_language_server_or_skip() {
        if let Some((port, token)) = detect_language_server() {
            assert!(port > 0);
            assert!(!token.is_empty());
        }
    }

    #[tokio::test]
    async fn test_fetch_agy_quota_integration() {
        if detect_language_server().is_none() {
            return;
        }
        let quotas = fetch_agy_quota().await.expect("Failed to fetch quota");
        assert!(!quotas.is_empty(), "Quotas list should not be empty");
        for q in quotas {
            assert!(!q.label.is_empty());
            assert!(q.remaining_fraction >= 0.0 && q.remaining_fraction <= 1.0);
        }
    }
}
