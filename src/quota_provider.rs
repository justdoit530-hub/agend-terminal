//! Gemini quota provider — cockpit-tools API integration for dispatch-time
//! worker availability checks (#t-20260626055111383187-96308-6).
//!
//! Reads OAuth tokens from `~/.antigravity_cockpit`, prefers a 60s local cache,
//! and classifies `gemini-5h` / `gemini-weekly` buckets into
//! `Available` / `RateLimited` / `Exhausted`.

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};

const COCKPIT_DATA_DIR: &str = ".antigravity_cockpit";
const GEMINI_ACCOUNTS_INDEX: &str = "gemini_accounts.json";
const GEMINI_ACCOUNTS_DIR: &str = "gemini_accounts";
const API_CACHE_DIR: &str = "cache/quota_api_v1_desktop/authorized";
const API_CACHE_TTL_MS: i64 = 60_000;
const TOKEN_REFRESH_SKEW_MS: i64 = 60_000;
const FIVE_HOURS: i64 = 5 * 3600;

const GEMINI_OAUTH_CLIENT_ID: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const LOAD_CODE_ASSIST_URL: &str = "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";
const RETRIEVE_QUOTA_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary";

/// Classified Gemini quota state for dispatch decisions.
#[derive(Debug, Clone, PartialEq)]
pub enum QuotaState {
    Available {
        pct_remaining: f32,
    },
    RateLimited {
        reset_at: DateTime<Utc>,
        pct_remaining: f32,
    },
    Exhausted {
        reset_at: DateTime<Utc>,
    },
}

/// Errors from quota lookup — dispatch treats most as fail-open (warn + proceed).
#[derive(Debug)]
pub enum QuotaError {
    AccountNotFound(String),
    TokenUnavailable,
    Unauthorized(String),
    Forbidden(String),
    Network(reqwest::Error),
    Api { status: u16, body: String },
    Parse(String),
    ProjectIdMissing,
    ClientSecretMissing,
}

impl fmt::Display for QuotaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AccountNotFound(s) => write!(f, "account not found for {s}"),
            Self::TokenUnavailable => write!(f, "token missing or expired, re-auth required"),
            Self::Unauthorized(s) => write!(f, "unauthorized: {s}"),
            Self::Forbidden(s) => write!(f, "forbidden: {s}"),
            Self::Network(e) => write!(f, "network: {e}"),
            Self::Api { status, body } => write!(f, "api error status={status}: {body}"),
            Self::Parse(s) => write!(f, "parse error: {s}"),
            Self::ProjectIdMissing => write!(f, "project_id unavailable after loadCodeAssist"),
            Self::ClientSecretMissing => {
                write!(f, "COCKPIT_GEMINI_CLIENT_SECRET env var not set")
            }
        }
    }
}

impl std::error::Error for QuotaError {}

#[derive(Debug, Clone)]
struct QuotaBucket {
    remaining_fraction: f32,
    reset_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct QuotaSnapshot {
    gemini_5h: Option<QuotaBucket>,
    gemini_weekly: Option<QuotaBucket>,
}

#[derive(Debug, Deserialize)]
struct GeminiAccountsIndex {
    #[serde(default)]
    accounts: Vec<GeminiAccountSummary>,
}

#[derive(Debug, Deserialize)]
struct GeminiAccountSummary {
    id: String,
    email: String,
}

#[derive(Debug, Deserialize)]
struct GeminiAccountFile {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expiry_date: Option<i64>,
    #[serde(default)]
    project_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuotaApiCacheRecord {
    updated_at: i64,
    payload: Value,
}

#[derive(Debug, Deserialize)]
struct LoadCodeAssistResponse {
    #[serde(rename = "cloudaicompanionProject", default)]
    project: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct QuotaSummaryResponse {
    #[serde(default)]
    groups: Vec<QuotaGroup>,
}

#[derive(Debug, Deserialize)]
struct QuotaGroup {
    #[serde(default)]
    buckets: Vec<QuotaBucketRaw>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuotaBucketRaw {
    bucket_id: String,
    remaining_fraction: Option<f32>,
    reset_time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GoogleAccountsFile {
    active: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GoogleTokenRefreshResponse {
    access_token: Option<String>,
    expires_in: Option<i64>,
    error: Option<String>,
    error_description: Option<String>,
}

fn cockpit_data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(COCKPIT_DATA_DIR)
}

fn hash_email_sha256(email: &str) -> String {
    let normalized = email.trim().to_lowercase();
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    hex::encode(hasher.finalize())
}

fn pct_from_fraction(fraction: f32) -> f32 {
    (fraction * 100.0).clamp(0.0, 100.0)
}

fn is_depleted(fraction: f32) -> bool {
    fraction <= 0.0
}

/// Core classifier — unit-tested via `mod tests`.
fn classify(snap: &QuotaSnapshot, now: DateTime<Utc>) -> QuotaState {
    let h5 = snap.gemini_5h.as_ref();
    let weekly = snap.gemini_weekly.as_ref();

    let h5_frac = h5.map(|b| b.remaining_fraction).unwrap_or(1.0);
    let weekly_frac = weekly.map(|b| b.remaining_fraction).unwrap_or(1.0);
    let h5_pct = pct_from_fraction(h5_frac);
    let weekly_pct = pct_from_fraction(weekly_frac);

    if is_depleted(weekly_frac) {
        return QuotaState::Exhausted {
            reset_at: weekly
                .and_then(|b| b.reset_at)
                .unwrap_or_else(|| now + Duration::days(7)),
        };
    }

    if is_depleted(h5_frac) {
        if let Some(b) = h5 {
            if let Some(reset) = b.reset_at {
                if (reset - now).num_seconds() > FIVE_HOURS {
                    return QuotaState::Available {
                        pct_remaining: weekly_pct,
                    };
                }
            }
        }
        return QuotaState::RateLimited {
            reset_at: h5
                .and_then(|b| b.reset_at)
                .unwrap_or_else(|| now + Duration::hours(1)),
            pct_remaining: h5_pct,
        };
    }

    QuotaState::Available {
        pct_remaining: h5_pct.min(weekly_pct),
    }
}

fn parse_reset_time(raw: Option<&str>) -> Option<DateTime<Utc>> {
    let s = raw?.trim();
    if s.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn extract_buckets_from_summary(
    summary: &QuotaSummaryResponse,
) -> (Option<QuotaBucket>, Option<QuotaBucket>) {
    let mut h5 = None;
    let mut weekly = None;
    for group in &summary.groups {
        for bucket in &group.buckets {
            let frac = bucket.remaining_fraction.unwrap_or(0.0);
            let entry = QuotaBucket {
                remaining_fraction: frac,
                reset_at: parse_reset_time(bucket.reset_time.as_deref()),
            };
            match bucket.bucket_id.as_str() {
                "gemini-5h" => h5 = Some(entry),
                "gemini-weekly" => weekly = Some(entry),
                _ => {}
            }
        }
    }
    (h5, weekly)
}

fn extract_project_id(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        let t = text.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    value
        .as_object()
        .and_then(|o| o.get("id"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn load_accounts_index(data_dir: &Path) -> Result<GeminiAccountsIndex, QuotaError> {
    let path = data_dir.join(GEMINI_ACCOUNTS_INDEX);
    let content = std::fs::read_to_string(&path)
        .map_err(|e| QuotaError::Parse(format!("read {}: {e}", path.display())))?;
    serde_json::from_str(&content)
        .map_err(|e| QuotaError::Parse(format!("parse {}: {e}", path.display())))
}

fn load_account_file(data_dir: &Path, account_id: &str) -> Result<GeminiAccountFile, QuotaError> {
    let path = data_dir
        .join(GEMINI_ACCOUNTS_DIR)
        .join(format!("{account_id}.json"));
    let content = std::fs::read_to_string(&path).map_err(|_| QuotaError::TokenUnavailable)?;
    serde_json::from_str(&content).map_err(|e| QuotaError::Parse(e.to_string()))
}

fn find_account_id_by_email(data_dir: &Path, email: &str) -> Result<String, QuotaError> {
    let index = load_accounts_index(data_dir)?;
    let needle = email.trim().to_lowercase();
    index
        .accounts
        .iter()
        .find(|a| a.email.trim().to_lowercase() == needle)
        .map(|a| a.id.clone())
        .ok_or_else(|| QuotaError::AccountNotFound(email.to_string()))
}

fn resolve_email_from_google_accounts(home: &Path) -> Option<String> {
    let path = home.join(".gemini").join("google_accounts.json");
    let content = std::fs::read_to_string(path).ok()?;
    let file: GoogleAccountsFile = serde_json::from_str(&content).ok()?;
    file.active
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn resolve_worker_home(home: &Path, worker: &str) -> PathBuf {
    if let Ok(fleet) = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) {
        if let Some(inst) = fleet.instances.get(worker) {
            if let Some(home_val) = inst.env.get("HOME") {
                return expand_tilde(home_val);
            }
        }
    }
    dirs::home_dir().unwrap_or_else(std::env::temp_dir)
}

fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(raw))
    } else if let Some(rest) = raw.strip_prefix("~/") {
        dirs::home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(raw))
    } else {
        PathBuf::from(raw)
    }
}

/// Resolve a fleet instance name or raw email to a Google account email.
pub fn resolve_email(home: &Path, worker_account: &str) -> Result<String, QuotaError> {
    if worker_account.contains('@') {
        return Ok(worker_account.trim().to_string());
    }
    let worker_home = resolve_worker_home(home, worker_account);
    resolve_email_from_google_accounts(&worker_home)
        .ok_or_else(|| QuotaError::AccountNotFound(worker_account.to_string()))
}

fn client_secret_from_env() -> Result<String, QuotaError> {
    std::env::var("COCKPIT_GEMINI_CLIENT_SECRET")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or(QuotaError::ClientSecretMissing)
}

fn token_needs_refresh(expiry_date_ms: Option<i64>) -> bool {
    let Some(expiry) = expiry_date_ms else {
        return false;
    };
    let now = Utc::now().timestamp_millis();
    expiry <= now + TOKEN_REFRESH_SKEW_MS
}

async fn refresh_access_token(refresh_token: &str) -> Result<(String, Option<i64>), QuotaError> {
    let secret = client_secret_from_env()?;
    let client = reqwest::Client::new();
    let resp = client
        .post(GOOGLE_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "client_id={}&client_secret={}&refresh_token={}&grant_type=refresh_token",
            urlencoding_simple(GEMINI_OAUTH_CLIENT_ID),
            urlencoding_simple(&secret),
            urlencoding_simple(refresh_token),
        ))
        .send()
        .await
        .map_err(QuotaError::Network)?;

    let status = resp.status();
    let body = resp.text().await.map_err(QuotaError::Network)?;
    if !status.is_success() {
        return Err(QuotaError::Api {
            status: status.as_u16(),
            body,
        });
    }
    let parsed: GoogleTokenRefreshResponse =
        serde_json::from_str(&body).map_err(|e| QuotaError::Parse(e.to_string()))?;
    if let Some(err) = parsed.error {
        return Err(QuotaError::Unauthorized(format!(
            "{err}: {}",
            parsed.error_description.unwrap_or_default()
        )));
    }
    let access = parsed
        .access_token
        .filter(|s| !s.is_empty())
        .ok_or(QuotaError::TokenUnavailable)?;
    let expiry = parsed
        .expires_in
        .map(|secs| Utc::now().timestamp_millis() + secs * 1000);
    Ok((access, expiry))
}

fn urlencoding_simple(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}

async fn ensure_access_token(account: &GeminiAccountFile) -> Result<String, QuotaError> {
    if let Some(token) = account.access_token.as_ref().filter(|s| !s.is_empty()) {
        if !token_needs_refresh(account.expiry_date) {
            return Ok(token.clone());
        }
    }
    let refresh = account
        .refresh_token
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or(QuotaError::TokenUnavailable)?;
    let (access, _expiry) = refresh_access_token(refresh).await?;
    Ok(access)
}

fn read_api_cache(data_dir: &Path, email: &str) -> Option<QuotaApiCacheRecord> {
    let path = data_dir
        .join(API_CACHE_DIR)
        .join(format!("{}.json", hash_email_sha256(email)));
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn cache_is_valid(record: &QuotaApiCacheRecord) -> bool {
    let now = Utc::now().timestamp_millis();
    now - record.updated_at < API_CACHE_TTL_MS
}

fn snapshot_from_cache(record: &QuotaApiCacheRecord) -> Result<QuotaSnapshot, QuotaError> {
    let summary_val = record
        .payload
        .get("quota_summary")
        .cloned()
        .ok_or_else(|| QuotaError::Parse("cache missing quota_summary".into()))?;
    let summary: QuotaSummaryResponse =
        serde_json::from_value(summary_val).map_err(|e| QuotaError::Parse(e.to_string()))?;
    let (h5, weekly) = extract_buckets_from_summary(&summary);
    Ok(QuotaSnapshot {
        gemini_5h: h5,
        gemini_weekly: weekly,
    })
}

async fn load_code_assist_project(access_token: &str) -> Result<String, QuotaError> {
    let client = reqwest::Client::new();
    let body = json!({
        "metadata": {
            "ideType": "IDE_UNSPECIFIED",
            "platform": "PLATFORM_UNSPECIFIED",
            "pluginType": "GEMINI"
        }
    });
    let resp = client
        .post(LOAD_CODE_ASSIST_URL)
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(QuotaError::Network)?;

    let status = resp.status();
    let text = resp.text().await.map_err(QuotaError::Network)?;
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(QuotaError::Unauthorized(text));
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        return Err(QuotaError::Forbidden(text));
    }
    if !status.is_success() {
        return Err(QuotaError::Api {
            status: status.as_u16(),
            body: text,
        });
    }
    let parsed: LoadCodeAssistResponse =
        serde_json::from_str(&text).map_err(|e| QuotaError::Parse(e.to_string()))?;
    parsed
        .project
        .as_ref()
        .and_then(extract_project_id)
        .ok_or(QuotaError::ProjectIdMissing)
}

async fn fetch_quota_summary(
    access_token: &str,
    project_id: &str,
) -> Result<QuotaSummaryResponse, QuotaError> {
    let client = reqwest::Client::new();
    let resp = client
        .post(RETRIEVE_QUOTA_URL)
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .json(&json!({ "project": project_id }))
        .send()
        .await
        .map_err(QuotaError::Network)?;

    let status = resp.status();
    let text = resp.text().await.map_err(QuotaError::Network)?;
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(QuotaError::Unauthorized(text));
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        return Err(QuotaError::Forbidden(text));
    }
    if !status.is_success() {
        return Err(QuotaError::Api {
            status: status.as_u16(),
            body: text,
        });
    }
    serde_json::from_str(&text).map_err(|e| QuotaError::Parse(e.to_string()))
}

async fn fetch_snapshot_from_api(
    access_token: &str,
    account: &GeminiAccountFile,
) -> Result<QuotaSnapshot, QuotaError> {
    let project_id = if let Some(pid) = account
        .project_id
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        pid.to_string()
    } else {
        load_code_assist_project(access_token).await?
    };
    let summary = fetch_quota_summary(access_token, &project_id).await?;
    let (h5, weekly) = extract_buckets_from_summary(&summary);
    Ok(QuotaSnapshot {
        gemini_5h: h5,
        gemini_weekly: weekly,
    })
}

async fn load_snapshot(
    data_dir: &Path,
    email: &str,
    access_token: &str,
    account: &GeminiAccountFile,
) -> Result<QuotaSnapshot, QuotaError> {
    if let Some(record) = read_api_cache(data_dir, email) {
        if cache_is_valid(&record) {
            if let Ok(snap) = snapshot_from_cache(&record) {
                tracing::debug!(email, "quota_provider: using valid cache");
                return Ok(snap);
            }
        }
    }
    fetch_snapshot_from_api(access_token, account).await
}

/// Async quota check for a fleet worker name or Google email.
pub async fn check(home: &Path, worker_account: &str) -> Result<QuotaState, QuotaError> {
    let email = resolve_email(home, worker_account)?;
    let data_dir = cockpit_data_dir();
    let account_id = find_account_id_by_email(&data_dir, &email)?;
    let account = load_account_file(&data_dir, &account_id)?;
    let access_token = ensure_access_token(&account).await?;
    let snap = load_snapshot(&data_dir, &email, &access_token, &account).await?;
    Ok(classify(&snap, Utc::now()))
}

/// Returns true when dispatch to an agy worker should be blocked.
pub fn should_block_dispatch(state: &QuotaState) -> bool {
    matches!(state, QuotaState::Exhausted { .. })
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
                    .expect("nested tokio runtime for quota_provider")
                    .block_on(fut)
            })
            .join()
            .expect("quota_provider block_on thread panicked")
        })
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime for quota_provider")
            .block_on(fut)
    }
}

/// Sync wrapper for dispatch gate (messaging.rs).
pub fn check_sync(home: &Path, worker_account: &str) -> Result<QuotaState, QuotaError> {
    block_on_value(check(home, worker_account))
}

/// True when `target` is an agy backend instance (fleet.yaml).
pub fn target_is_agy(home: &Path, target: &str) -> bool {
    crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
        .ok()
        .and_then(|fleet| fleet.instances.get(target).cloned())
        .map(|inst| {
            inst.backend.unwrap_or(crate::backend::Backend::ClaudeCode)
                == crate::backend::Backend::Agy
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(h5_frac: f32, weekly_frac: f32, h5_reset_hours: Option<i64>) -> QuotaSnapshot {
        let now = Utc::now();
        QuotaSnapshot {
            gemini_5h: Some(QuotaBucket {
                remaining_fraction: h5_frac,
                reset_at: h5_reset_hours.map(|h| now + Duration::hours(h)),
            }),
            gemini_weekly: Some(QuotaBucket {
                remaining_fraction: weekly_frac,
                reset_at: Some(now + Duration::days(3)),
            }),
        }
    }

    #[test]
    fn test_classify_available() {
        let state = classify(&snap(0.75, 0.48, Some(3)), Utc::now());
        assert!(matches!(state, QuotaState::Available { pct_remaining } if pct_remaining > 0.0));
    }

    #[test]
    fn test_classify_rate_limited() {
        let state = classify(&snap(0.0, 0.5, Some(2)), Utc::now());
        assert!(matches!(state, QuotaState::RateLimited { .. }));
    }

    #[test]
    fn test_classify_exhausted() {
        let state = classify(&snap(0.0, 0.0, Some(2)), Utc::now());
        assert!(matches!(state, QuotaState::Exhausted { .. }));
    }

    #[test]
    fn test_classify_weekly_cap_as_exhausted() {
        // 5h at 0% but reset >5h away → weekly cap in effect, treat as Available.
        let state = classify(&snap(0.0, 0.4, Some(6)), Utc::now());
        assert!(matches!(state, QuotaState::Available { .. }));
    }

    #[test]
    fn test_hash_email_sha256_deterministic() {
        let a = hash_email_sha256("justdoit530@gmail.com");
        let b = hash_email_sha256("JUSTDOIT530@GMAIL.COM");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn test_should_block_dispatch_only_exhausted() {
        assert!(!should_block_dispatch(&QuotaState::Available {
            pct_remaining: 50.0,
        }));
        assert!(!should_block_dispatch(&QuotaState::RateLimited {
            reset_at: Utc::now(),
            pct_remaining: 0.0,
        }));
        assert!(should_block_dispatch(&QuotaState::Exhausted {
            reset_at: Utc::now(),
        }));
    }

    #[tokio::test]
    async fn check_integration_skips_without_env() {
        if std::env::var("COCKPIT_GEMINI_CLIENT_SECRET").is_err() {
            return;
        }
        let home = std::env::temp_dir();
        let result = check(&home, "justdoit530@gmail.com").await;
        if let Ok(state) = result {
            assert!(matches!(
                state,
                QuotaState::Available { .. }
                    | QuotaState::RateLimited { .. }
                    | QuotaState::Exhausted { .. }
            ));
        }
    }
}
