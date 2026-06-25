use serde::Serialize;
use serde_json::{json, Value};
use std::process::Command;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct ModelQuota {
    pub model: Option<String>,
    pub remaining_fraction: f64,
    pub reset_time: Option<String>,
}

pub(crate) fn handle_agy_quota() -> Value {
    match fetch_agy_quota_blocking() {
        Some(models) => json!({"ok": true, "models": models}),
        None => json!({
            "ok": false,
            "models": [],
            "error": "unable to detect Antigravity language server or fetch quota"
        }),
    }
}

pub(crate) fn detect_language_server() -> Option<(u16, String)> {
    let pid = detect_language_server_pid()?;
    let port = detect_listen_port(&pid)?;
    let cmdline = process_command_line(&pid)?;
    let csrf = extract_csrf_token(&cmdline)?;
    Some((port, csrf))
}

pub(crate) async fn fetch_agy_quota() -> Option<Vec<ModelQuota>> {
    let (port, csrf) = detect_language_server()?;
    let url = format!(
        "https://127.0.0.1:{port}/exa.language_server_pb.LanguageServerService/GetUserStatus"
    );
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    let body: Value = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .header("X-Codeium-Csrf-Token", csrf)
        .json(&json!({}))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let models = extract_model_quotas(&body);
    if models.is_empty() {
        None
    } else {
        Some(models)
    }
}

fn fetch_agy_quota_blocking() -> Option<Vec<ModelQuota>> {
    if tokio::runtime::Handle::try_current().is_ok() {
        // fire-and-forget: joined immediately to run an isolated runtime without blocking the caller runtime.
        return std::thread::spawn(fetch_agy_quota_blocking_on_new_runtime)
            .join()
            .ok()
            .flatten();
    }
    fetch_agy_quota_blocking_on_new_runtime()
}

fn fetch_agy_quota_blocking_on_new_runtime() -> Option<Vec<ModelQuota>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(fetch_agy_quota())
}

fn detect_language_server_pid() -> Option<String> {
    let out = Command::new("pgrep")
        .args(["-f", "language_server.*subclient_type hub"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

fn process_command_line(pid: &str) -> Option<String> {
    let out = Command::new("ps")
        .args(["-p", pid, "-ww", "-o", "command="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let cmd = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!cmd.is_empty()).then_some(cmd)
}

fn detect_listen_port(pid: &str) -> Option<u16> {
    let out = Command::new("lsof")
        .args(["-Pan", "-p", pid, "-iTCP", "-sTCP:LISTEN"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(parse_lsof_listen_port)
}

fn parse_lsof_listen_port(line: &str) -> Option<u16> {
    if !line.contains("LISTEN") {
        return None;
    }
    let before_arrow = line.split("->").next().unwrap_or(line);
    let port = before_arrow
        .rsplit_once(':')
        .map(|(_, tail)| tail)
        .unwrap_or(before_arrow)
        .split_whitespace()
        .next()?;
    port.parse().ok()
}

fn extract_csrf_token(cmdline: &str) -> Option<String> {
    let mut parts = cmdline.split_whitespace().peekable();
    while let Some(part) = parts.next() {
        if let Some(token) = part.strip_prefix("--csrf_token=") {
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
        if part == "--csrf_token" {
            if let Some(token) = parts.peek().copied().filter(|s| !s.is_empty()) {
                return Some(token.to_string());
            }
        }
    }
    None
}

fn extract_model_quotas(body: &Value) -> Vec<ModelQuota> {
    let Some(configs) = body
        .pointer("/userStatus/cascadeModelConfigData/clientModelConfigs")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    configs
        .iter()
        .filter_map(|config| {
            let quota = config.get("quotaInfo")?;
            let remaining_fraction = quota.get("remainingFraction").and_then(Value::as_f64)?;
            let model = ["model", "modelName", "displayName", "id"]
                .iter()
                .find_map(|key| config.get(*key).and_then(Value::as_str))
                .map(str::to_string);
            let reset_time = quota
                .get("resetTime")
                .or_else(|| quota.get("reset_time"))
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(ModelQuota {
                model,
                remaining_fraction,
                reset_time,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_csrf_token_from_flag_shapes() {
        assert_eq!(
            extract_csrf_token("language_server --csrf_token abc123 --x"),
            Some("abc123".to_string())
        );
        assert_eq!(
            extract_csrf_token("language_server --csrf_token=xyz"),
            Some("xyz".to_string())
        );
        assert_eq!(extract_csrf_token("language_server"), None);
    }

    #[test]
    fn parses_lsof_listen_port() {
        let line = "node 123 neo 55u IPv4 0x0 TCP 127.0.0.1:54321 (LISTEN)";
        assert_eq!(parse_lsof_listen_port(line), Some(54321));
        assert_eq!(parse_lsof_listen_port("node 123 TCP 127.0.0.1:99"), None);
    }

    #[test]
    fn extracts_model_quotas() {
        let body = json!({
            "userStatus": {
                "cascadeModelConfigData": {
                    "clientModelConfigs": [
                        {
                            "modelName": "gemini-2.5-flash",
                            "quotaInfo": {
                                "remainingFraction": 0.42,
                                "resetTime": "2026-06-25T00:00:00Z"
                            }
                        },
                        {"modelName": "missing-quota"}
                    ]
                }
            }
        });
        assert_eq!(
            extract_model_quotas(&body),
            vec![ModelQuota {
                model: Some("gemini-2.5-flash".to_string()),
                remaining_fraction: 0.42,
                reset_time: Some("2026-06-25T00:00:00Z".to_string()),
            }]
        );
    }
}
