use crate::mcp::handlers::comms_gates::{detect_verdict, Verdict};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mistake {
    pub id: String,
    pub task_id: Option<String>,
    pub agent_name: String,
    pub category: String,
    pub rejection_reason: String,
    pub timestamp: String,
    #[serde(default)]
    pub corrected_at: Option<String>,
}

pub fn mark_mistake_corrected<'a>(
    home: &Path,
    agent_name: &str,
    category: impl Into<Option<&'a str>>,
) {
    let category = category.into().filter(|category| !category.is_empty());
    let mistakes_dir = home.join("mistakes");
    let Ok(entries) = fs::read_dir(&mistakes_dir) else {
        return;
    };
    let now = chrono::Utc::now().to_rfc3339();
    let mut latest_uncategorized_match: Option<(
        chrono::DateTime<chrono::Utc>,
        std::path::PathBuf,
    )> = None;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(mut m) = serde_json::from_str::<Mistake>(&content) {
                    if m.agent_name != agent_name || m.corrected_at.is_some() {
                        continue;
                    }
                    if category.is_some_and(|category| m.category == category) {
                        m.corrected_at = Some(now.clone());
                        if let Ok(serialized) = serde_json::to_string_pretty(&m) {
                            let _ = fs::write(&path, serialized);
                        }
                    } else if category.is_none() {
                        let Ok(timestamp) = chrono::DateTime::parse_from_rfc3339(&m.timestamp)
                            .map(|timestamp| timestamp.with_timezone(&chrono::Utc))
                        else {
                            continue;
                        };
                        if latest_uncategorized_match
                            .as_ref()
                            .is_none_or(|(latest, _)| timestamp > *latest)
                        {
                            latest_uncategorized_match = Some((timestamp, path));
                        }
                    }
                }
            }
        }
    }

    if let Some((_, path)) = latest_uncategorized_match {
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(mut m) = serde_json::from_str::<Mistake>(&content) {
                m.corrected_at = Some(now);
                if let Ok(serialized) = serde_json::to_string_pretty(&m) {
                    let _ = fs::write(path, serialized);
                }
            }
        }
    }
}

pub fn auto_correct_on_ci_pass(home: &Path, agent_name: &str, category_hint: Option<&str>) {
    if let Some(cat) = category_hint {
        mark_mistake_corrected(home, agent_name, cat);
    } else {
        let mistakes_dir = home.join("mistakes");
        let Ok(entries) = fs::read_dir(&mistakes_dir) else {
            return;
        };
        let mut categories = std::collections::HashSet::new();
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(m) = serde_json::from_str::<Mistake>(&content) {
                        if m.agent_name == agent_name && m.corrected_at.is_none() {
                            categories.insert(m.category);
                        }
                    }
                }
            }
        }
        for cat in categories {
            mark_mistake_corrected(home, agent_name, cat.as_str());
        }
    }
}

pub fn has_cargo_test_pass_evidence(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    let has_test = b.contains("cargo test") || b.contains("test result: ok");
    if !has_test {
        return false;
    }
    let has_ok = b.contains("test result: ok")
        || b.contains("passed")
        || b.contains("success")
        || b.contains("green");
    if !has_ok {
        return false;
    }
    let has_fail = b.contains("failed")
        || b.contains("failures")
        || b.contains("error")
        || b.contains("panic");
    if has_fail {
        static ZERO_FAIL_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = ZERO_FAIL_RE.get_or_init(|| {
            regex::Regex::new(r"(?i)(0\s+failed|0\s+failures|no\s+failures|zero\s+failures|zero\s+failed|0\s+errors)")
                .expect("valid zero failures regex")
        });
        if !re.is_match(&b) {
            return false;
        }
    }
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Success {
    pub success_id: String,
    pub agent_name: String,
    pub category: String,
    pub summary: String,
    pub recorded_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    #[serde(alias = "id", rename = "rule_id")]
    pub id: String,
    pub agent_name: String,
    pub category: String,
    pub rule_text: String,
    pub created_at: String,
    #[serde(default)]
    pub trigger_count: usize,
    /// How `rule_text` was produced: `"llm"` or `"template"`. Absent on legacy rules.
    #[serde(default)]
    pub synthesis_method: Option<String>,
}

/// List all solidified rules for a specific agent.
pub fn list_rules(home: &Path, agent_name: &str) -> Vec<Rule> {
    let rules_dir = home.join("rules");
    let mut rules = Vec::new();
    if let Ok(entries) = fs::read_dir(&rules_dir) {
        for entry in entries.filter_map(Result::ok) {
            if entry.path().extension().is_some_and(|ext| ext == "json") {
                if entry.file_name() == "shared.json" {
                    continue;
                }
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    if let Ok(rule) = serde_json::from_str::<Rule>(&content) {
                        if rule.agent_name == agent_name {
                            rules.push(rule);
                        }
                    }
                }
            }
        }
    }

    // Load shared rules from shared.json
    let shared_path = rules_dir.join("shared.json");
    if let Ok(content) = fs::read_to_string(&shared_path) {
        if let Ok(shared_rules) = serde_json::from_str::<Vec<Rule>>(&content) {
            for r in shared_rules {
                if !rules
                    .iter()
                    .any(|existing| existing.id == r.id || existing.rule_text == r.rule_text)
                {
                    rules.push(r);
                }
            }
        }
    }

    rules
}

/// List all solidified rules that belong to agents other than `exclude_agent`.
pub fn list_cross_agent_rules(home: &Path, exclude_agent: &str) -> Vec<Rule> {
    let rules_dir = home.join("rules");
    let mut rules = Vec::new();
    if let Ok(entries) = fs::read_dir(&rules_dir) {
        for entry in entries.filter_map(Result::ok) {
            if entry.path().extension().is_some_and(|ext| ext == "json") {
                if entry.file_name() == "shared.json" {
                    continue;
                }
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    if let Ok(rule) = serde_json::from_str::<Rule>(&content) {
                        if rule.agent_name != exclude_agent {
                            rules.push(rule);
                        }
                    }
                }
            }
        }
    }
    rules
}

const MEM0_SYNC_URL: &str = "http://localhost:5174/add";

/// Classify a mistake using regex matching on the rejection text and parent message.
pub fn classify_mistake(rejection_text: &str, parent_text: Option<&str>) -> Option<&'static str> {
    // 1. missing_test_execution
    // Check if the parent report has a verdict of VERIFIED or REJECTED but didn't run cargo test.
    if let Some(p_text) = parent_text {
        if let Some(verdict) = detect_verdict(p_text) {
            if (verdict == Verdict::Verified || verdict == Verdict::Rejected)
                && !p_text.contains("cargo test")
            {
                return Some("missing_test_execution");
            }
        }
    }
    // Fallback regex matching for test run missing in the rejection text
    static TEST_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static MISSING_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static BRANCH_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static LINT_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static PR_REPO_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static EVIDENCE_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static SECRET_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static FIRE_AND_FORGET_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static BRANCH_BASE_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static INCOMPLETE_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static TEST_FAILURE_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static MISSING_PR_DESC_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();

    let test_re = TEST_RE.get_or_init(|| {
        regex::Regex::new(r"(?i)(cargo test|test suite|unit test)").expect("valid test regex")
    });
    let missing_re = MISSING_RE.get_or_init(|| {
        regex::Regex::new(r"(?i)(missing|omit|forgot|no |not run|failed to)")
            .expect("valid missing regex")
    });
    if test_re.is_match(rejection_text) && missing_re.is_match(rejection_text) {
        return Some("missing_test_execution");
    }

    // 2. wrong_branch_target
    // PR base is suzuke/agend-terminal upstream instead of fork
    let branch_re = BRANCH_RE.get_or_init(|| {
        regex::Regex::new(r"(?i)(upstream|branch target|target.*fork|target.*branch)")
            .expect("valid branch regex")
    });
    if branch_re.is_match(rejection_text) {
        return Some("wrong_branch_target");
    }

    // 3. lint_failure
    // Rejection reason contains clippy/lint warnings
    let lint_re = LINT_RE.get_or_init(|| {
        regex::Regex::new(r"(?i)(clippy|lint|warnings|cargo clippy)").expect("valid lint regex")
    });
    if lint_re.is_match(rejection_text) {
        return Some("lint_failure");
    }

    // 4. wrong_pr_repo
    let pr_repo_re = PR_REPO_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)((pr create|pull request|gh pr).*(suzuke/agend-terminal|wrong repo|incorrect repo|--repo suzuke)|(suzuke/agend-terminal|wrong repo|incorrect repo|--repo suzuke).*(pr create|pull request|gh pr))",
        )
        .expect("valid PR repo regex")
    });
    if pr_repo_re.is_match(rejection_text) {
        return Some("wrong_pr_repo");
    }

    // 5. missing_evidence
    let evidence_re = EVIDENCE_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(without evidence|no evidence|missing.*evidence|evidence.*missing|missing.*### evidence|no.*### evidence|without.*### evidence|missing.*cited:|missing.*ran:)",
        )
        .expect("valid evidence regex")
    });
    if evidence_re.is_match(rejection_text) {
        return Some("missing_evidence");
    }

    // 6. hardcoded_secret
    let secret_re = SECRET_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(hardcode|hard-code|hardcoded|secret|api.?key|token|credential|env var|environment variable)",
        )
        .expect("valid secret regex")
    });
    if secret_re.is_match(rejection_text) {
        return Some("hardcoded_secret");
    }

    // 7. missing_fire_and_forget
    let fire_and_forget_re = FIRE_AND_FORGET_RE.get_or_init(|| {
        regex::Regex::new(r"(?i)(fire.and.forget|tokio::spawn|spawn.*comment|// fire)")
            .expect("valid fire-and-forget regex")
    });
    if fire_and_forget_re.is_match(rejection_text) {
        return Some("missing_fire_and_forget");
    }

    // 8. wrong_branch_base
    let branch_base_re = BRANCH_BASE_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(base branch|wrong base|stale base|branched from|checkout.*main|based on.*main)",
        )
        .expect("valid branch base regex")
    });
    if branch_base_re.is_match(rejection_text) {
        return Some("wrong_branch_base");
    }

    // 9. incomplete_implementation
    let incomplete_re = INCOMPLETE_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(incomplete implementation|functionally incomplete|stub implementation|left a todo|todo left|partial logic|partial implementation|not implemented|unimplemented|placeholder implementation|\btodo\b|\bfixme\b)",
        )
        .expect("valid incomplete implementation regex")
    });
    if incomplete_re.is_match(rejection_text) {
        return Some("incomplete_implementation");
    }

    // 10. test_failure
    let test_failure_re = TEST_FAILURE_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(test failure|tests fail|failing test|tests? (are )?failing|broke.*tests?|broken tests?|tests? broken|test regression|assertion failed|tests? did not pass|cargo test.*fail)",
        )
        .expect("valid test failure regex")
    });
    if test_failure_re.is_match(rejection_text) {
        return Some("test_failure");
    }

    // 11. missing_pr_description
    let missing_pr_desc_re = MISSING_PR_DESC_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(empty pr (description|body)|missing pr (description|body)|trivial pr (description|body)|no pr description|pr (description|body) (is )?(empty|missing|trivial)|empty pull request (description|body)|missing pull request description)",
        )
        .expect("valid missing PR description regex")
    });
    if missing_pr_desc_re.is_match(rejection_text) {
        return Some("missing_pr_description");
    }

    Some("unclassified")
}

/// Retrieve the rule text for a given category.
pub fn get_rule_text(category: &str) -> &'static str {
    match category {
        "missing_test_execution" => "NEVER report VERIFIED without running cargo test",
        "wrong_branch_target" => "NEVER open a PR targeting the upstream suzuke/agend-terminal repo; always target your own fork justdoit530-hub/agend-terminal",
        "lint_failure" => "NEVER submit code with clippy warnings or lint failures; run cargo clippy before submitting",
        "missing_evidence" => "NEVER report VERIFIED/REJECTED without an ### Evidence block",
        "missing_fire_and_forget" => "NEVER use tokio::spawn without // fire-and-forget: <reason> comment",
        "wrong_pr_repo" => "NEVER open a PR to suzuke/agend-terminal; always use justdoit530-hub/agend-terminal",
        "hardcoded_secret" => "NEVER hardcode secrets or API keys in source code; read from environment variables",
        "wrong_branch_base" => "NEVER base a feature branch on a stale or wrong base branch",
        "incomplete_implementation" => "NEVER submit functionally incomplete code (stubs, TODOs, or partial logic); finish the implementation before opening a PR",
        "test_failure" => "NEVER commit code that breaks existing tests; run cargo test and fix failures before submitting",
        "missing_pr_description" => "NEVER open a PR with an empty or trivial description; write a meaningful PR body explaining what changed and why",
        _ => "NEVER repeat this mistake category",
    }
}

fn first_meaningful_line(text: &str) -> Option<&str> {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("###")
            || trimmed.starts_with("REJECTED")
            || trimmed.starts_with("VERIFIED")
            || trimmed.starts_with("ran:")
            || trimmed.starts_with("cited:")
        {
            continue;
        }
        let trunc_len = trimmed
            .char_indices()
            .nth(120)
            .map(|(i, _)| i)
            .unwrap_or(trimmed.len());
        let start = line.find(trimmed)?;
        return Some(&line[start..start + trunc_len]);
    }
    None
}

#[cfg(test)]
static CLAUDE_API_RESPONSE_MOCK: std::sync::Mutex<Option<Result<String, String>>> =
    std::sync::Mutex::new(None);

#[allow(clippy::unwrap_used, clippy::expect_used)]
fn call_claude_api(
    api_key: &str,
    system_prompt: &str,
    user_prompt: &str,
) -> Result<String, String> {
    #[cfg(test)]
    {
        let lock = CLAUDE_API_RESPONSE_MOCK.lock().unwrap();
        if let Some(mock) = &*lock {
            return mock.clone();
        }
    }

    let api_key = api_key.to_string();
    let system_prompt = system_prompt.to_string();
    let user_prompt = user_prompt.to_string();

    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("Failed to build runtime: {}", e))?;

        rt.block_on(async {
            let client = reqwest::Client::new();
            let payload = serde_json::json!({
                "model": "claude-haiku-4-5-20251001",
                "max_tokens": 1024,
                "system": system_prompt,
                "messages": [
                    {
                        "role": "user",
                        "content": user_prompt
                    }
                ]
            });

            let response = client
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&payload)
                .timeout(std::time::Duration::from_secs(15))
                .send()
                .await
                .map_err(|e| format!("Request failed: {}", e))?;

            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                return Err(format!("API error status {}: {}", status, text));
            }

            let res_json: serde_json::Value = response
                .json()
                .await
                .map_err(|e| format!("Failed to parse JSON response: {}", e))?;

            let text = res_json["content"]
                .as_array()
                .and_then(|arr| arr.first())
                .and_then(|obj| obj["text"].as_str())
                .ok_or_else(|| format!("Invalid response format: {:?}", res_json))?;

            Ok(text.to_string())
        })
    });

    handle.join().map_err(|_| "Thread panicked".to_string())?
}

fn sanitize_llm_rule(text: &str) -> String {
    let mut cleaned = text
        .replace('\r', "")
        .replace('\n', "; ")
        .trim()
        .to_string();

    while cleaned.starts_with("- ") || cleaned.starts_with("* ") {
        cleaned = cleaned[2..].trim().to_string();
    }
    if cleaned.starts_with("1. ") {
        cleaned = cleaned[3..].trim().to_string();
    }

    while cleaned.contains(";;") {
        cleaned = cleaned.replace(";;", ";");
    }
    while cleaned.contains("; ;") {
        cleaned = cleaned.replace("; ;", ";");
    }
    while cleaned.contains("  ") {
        cleaned = cleaned.replace("  ", " ");
    }

    cleaned = cleaned.trim_end_matches(';').trim().to_string();
    cleaned
}

const SYNTHESIS_METHOD_LLM: &str = "llm";
const SYNTHESIS_METHOD_TEMPLATE: &str = "template";

fn synthesize_rule_text_with_llm_fallback(
    home: &Path,
    agent_name: &str,
    category: &str,
    mistakes: &[Mistake],
) -> (String, String) {
    let mut rejection_lines = Vec::new();
    for mistake in mistakes {
        if let Some(line) = first_meaningful_line(&mistake.rejection_reason) {
            if !rejection_lines.contains(&line) {
                rejection_lines.push(line);
            }
            if rejection_lines.len() >= 3 {
                break;
            }
        }
    }

    let successes_path = home.join("successes").join(format!("{agent_name}.json"));
    let mut success_summaries = Vec::new();
    if let Ok(content) = fs::read_to_string(&successes_path) {
        if let Ok(mut list) = serde_json::from_str::<Vec<Success>>(&content) {
            list.sort_by(|a, b| {
                let ts_a = chrono::DateTime::parse_from_rfc3339(&a.recorded_at)
                    .map(|ts| ts.with_timezone(&chrono::Utc))
                    .unwrap_or(chrono::DateTime::UNIX_EPOCH);
                let ts_b = chrono::DateTime::parse_from_rfc3339(&b.recorded_at)
                    .map(|ts| ts.with_timezone(&chrono::Utc))
                    .unwrap_or(chrono::DateTime::UNIX_EPOCH);
                ts_b.cmp(&ts_a)
            });
            for s in list {
                if s.category == category && !success_summaries.contains(&s.summary) {
                    success_summaries.push(s.summary);
                }
            }
        }
    }

    if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
        if !api_key.trim().is_empty() {
            let bullets = rejection_lines
                .iter()
                .map(|line| format!("- {line}"))
                .collect::<Vec<_>>()
                .join("\n");
            let success_summary = if success_summaries.is_empty() {
                "No matching success summary recorded yet.".to_string()
            } else {
                success_summaries.join("; ")
            };

            let system_prompt = "You are an expert programming assistant. Write 1-3 concise actionable rules starting with NEVER or ALWAYS to prevent recurrence of the programming mistakes described by the user. Keep it brief and return ONLY the rules themselves on a single line (no newlines, use semicolons to separate rules if there are multiple). Do not write any introduction, markdown, numbering, bullet points, or conclusion.";
            let user_prompt = format!(
                "Mistake category: {}\n\nGiven these failures:\n{}\n\nAnd this success:\n{}\n\nWrite the rules:",
                category, bullets, success_summary
            );

            tracing::info!(
                "Synthesizing rule via Claude LLM path for category: {}",
                category
            );
            match call_claude_api(&api_key, system_prompt, &user_prompt) {
                Ok(raw_text) => {
                    let cleaned = sanitize_llm_rule(&raw_text);
                    if !cleaned.is_empty() {
                        return (cleaned, SYNTHESIS_METHOD_LLM.to_string());
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Claude API synthesis failed, falling back to template: {}",
                        e
                    );
                }
            }
        }
    }

    (
        synthesize_rule_text(category, mistakes),
        SYNTHESIS_METHOD_TEMPLATE.to_string(),
    )
}

fn synthesize_rule_text(category: &str, mistakes: &[Mistake]) -> String {
    let base_rule = get_rule_text(category);
    let mut rejection_lines: Vec<&str> = Vec::new();
    for mistake in mistakes {
        if let Some(line) = first_meaningful_line(&mistake.rejection_reason) {
            if !rejection_lines.contains(&line) {
                rejection_lines.push(line);
            }
            if rejection_lines.len() >= 3 {
                break;
            }
        }
    }
    if rejection_lines.is_empty() {
        return base_rule.to_string();
    }
    let bullets = rejection_lines
        .iter()
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{base_rule}\n\nRecurring failures:\n{bullets}")
}

fn solidify_threshold() -> usize {
    std::env::var("AGEND_SOLIDIFY_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(3)
}

/// Main entry point to record a mistake, check threshold, and inject rule if needed.
pub fn record_mistake(
    home: &Path,
    reporter: &str,
    agent_name: &str,
    summary: &str,
    args: &Value,
    category_hint: Option<&str>,
) -> Option<String> {
    let _ = reporter; // Keep for signature compatibility
    let parent_id = args["parent_id"].as_str();
    let parent_msg = parent_id.and_then(|pid| crate::inbox::find_message(home, pid));
    let parent_text = parent_msg.as_ref().map(|m| m.text.as_str());

    let real_agent_name = agent_name.to_string();

    let rejection_text = format!("{}\n{}", summary, args["artifacts"].as_str().unwrap_or(""));
    let category = match category_hint {
        Some(cat) => cat.to_string(),
        None => classify_mistake(&rejection_text, parent_text)?.to_string(),
    };

    let mistakes_dir = home.join("mistakes");
    if let Err(e) = fs::create_dir_all(&mistakes_dir) {
        tracing::warn!(?e, "failed to create mistakes directory");
        return None;
    }

    let mistake_id = format!(
        "mstk_{}_{}",
        chrono::Utc::now().timestamp_millis(),
        uuid::Uuid::new_v4().simple()
    );
    let mistake = Mistake {
        id: mistake_id.clone(),
        task_id: args["correlation_id"].as_str().map(str::to_string),
        agent_name: real_agent_name.clone(),
        category: category.to_string(),
        rejection_reason: rejection_text,
        timestamp: chrono::Utc::now().to_rfc3339(),
        corrected_at: None,
    };

    let filepath = mistakes_dir.join(format!("{}.json", mistake.id));
    if let Ok(serialized) = serde_json::to_string_pretty(&mistake) {
        if let Err(e) = fs::write(&filepath, serialized) {
            tracing::warn!(?e, ?filepath, "failed to write mistake file");
        }
    }

    // Count mistakes of same agent and category within 30 days
    let mut count = 0;
    let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
    if let Ok(entries) = fs::read_dir(&mistakes_dir) {
        for entry in entries.filter_map(Result::ok) {
            if entry.path().extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    if let Ok(m_val) = serde_json::from_str::<serde_json::Value>(&content) {
                        if m_val["orphaned"].as_bool() == Some(true) {
                            continue;
                        }
                    }
                    if let Ok(m) = serde_json::from_str::<Mistake>(&content) {
                        if m.agent_name == real_agent_name && m.category == category {
                            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&m.timestamp) {
                                if ts.with_timezone(&chrono::Utc) >= cutoff {
                                    count += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Solidify rule if threshold reached
    let rule_id = if count >= solidify_threshold() {
        solidify_rule(home, &real_agent_name, &category, count)
    } else {
        None
    };

    cleanup_old_mistakes(home);

    rule_id
}

pub fn record_success(
    home: &Path,
    _reporter: &str,
    agent_name: &str,
    summary: &str,
    category: &str,
) -> Option<String> {
    let successes_dir = home.join("successes");
    if let Err(e) = fs::create_dir_all(&successes_dir) {
        tracing::warn!(?e, "failed to create successes directory");
        return None;
    }

    let path = successes_dir.join(format!("{agent_name}.json"));
    let mut successes: Vec<Success> = fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default();
    let success_id = format!(
        "s-{}-{}",
        chrono::Utc::now().timestamp_millis(),
        uuid::Uuid::new_v4().simple()
    );
    successes.push(Success {
        success_id: success_id.clone(),
        agent_name: agent_name.to_string(),
        category: category.to_string(),
        summary: summary.to_string(),
        recorded_at: chrono::Utc::now().to_rfc3339(),
    });

    let serialized = serde_json::to_string_pretty(&successes).ok()?;
    if let Err(e) = fs::write(&path, serialized) {
        tracing::warn!(?e, ?path, "failed to write success file");
        return None;
    }

    tracing::info!(agent_name, category, "success recorded");
    solidify_success_pattern(home, agent_name, category);
    Some(success_id)
}

/// Extract the `### Evidence` section from a VERIFIED report body.
pub fn extract_evidence_from_report(text: &str) -> String {
    const MARKER: &str = "### Evidence";
    if let Some(idx) = text.find(MARKER) {
        return text[idx + MARKER.len()..].trim().to_string();
    }
    text.lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("ran:") || trimmed.starts_with("cited:")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a completed task is complex enough to auto-extract a reusable skill.
pub fn should_create_skill(summary: &str, evidence: &str, category: &str) -> bool {
    if matches!(category, "general" | "unclassified") {
        return false;
    }
    let complexity = evidence
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("ran:") || trimmed.starts_with("cited:")
        })
        .count();
    if complexity < 4 {
        return false;
    }
    if summary.len() < 50 {
        return false;
    }
    true
}

fn skill_auto_dir(_home: &Path) -> Option<std::path::PathBuf> {
    if let Ok(override_dir) = std::env::var("AGEND_SKILL_AUTO_DIR") {
        return Some(std::path::PathBuf::from(override_dir));
    }
    dirs::home_dir().map(|home| home.join(".claude/skills/auto"))
}

/// Generate a Markdown skill file under `~/.claude/skills/auto/` for complex successes.
/// Fails silently when the Claude API is unavailable or returns an error.
pub fn maybe_create_skill(
    agent_name: &str,
    category: &str,
    summary: &str,
    evidence: &str,
    home: &Path,
) {
    let Some(auto_dir) = skill_auto_dir(home) else {
        tracing::debug!("skill auto dir unavailable; skipping maybe_create_skill");
        return;
    };

    let api_key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(key) if !key.trim().is_empty() => key,
        _ => return,
    };

    let timestamp = chrono::Utc::now().to_rfc3339();
    let date = chrono::Utc::now().format("%Y%m%d").to_string();
    let user_prompt = format!(
        "You are extracting a reusable procedure from a completed agent task.\n\n\
         Task summary: {summary}\n\
         Category: {category}\n\
         Agent: {agent_name}\n\
         Evidence:\n{evidence}\n\n\
         Generate a concise skill file in this exact format:\n\
         ---\n\
         name: {category}-{agent_name}-procedure\n\
         description: <one sentence: when to use this skill>\n\
         metadata:\n\
           category: {category}\n\
           agent: {agent_name}\n\
           created_at: {timestamp}\n\
           source: auto_reflexion\n\
         ---\n\n\
         # <Short Title>\n\n\
         ## When to Use\n\
         - <condition 1>\n\
         - <condition 2>\n\n\
         ## Steps\n\
         1. <reusable step>\n\
         2. <reusable step>\n\
         ...\n\n\
         ## Key Commands\n\
         ```\n\
         <important commands extracted from evidence>\n\
         ```\n\n\
         Focus on REUSABLE PROCEDURE. Extract the HOW, not the specific task content.\n\
         Respond with ONLY the markdown file content, no explanation."
    );

    let system_prompt =
        "You write concise, reusable agent skill files in Markdown with YAML frontmatter.";

    let content = match call_claude_api(&api_key, system_prompt, &user_prompt) {
        Ok(text) if !text.trim().is_empty() => text,
        Ok(_) => return,
        Err(e) => {
            tracing::debug!(?e, "maybe_create_skill: Claude API failed; skipping");
            return;
        }
    };

    if let Err(e) = fs::create_dir_all(&auto_dir) {
        tracing::warn!(?e, ?auto_dir, "failed to create skill auto dir");
        return;
    }

    let filename = format!("{category}_{agent_name}_{date}.md");
    let path = auto_dir.join(filename);
    if let Err(e) = fs::write(&path, content) {
        tracing::warn!(?e, ?path, "failed to write auto skill file");
    } else {
        tracing::info!(?path, agent_name, category, "auto skill file created");
    }
}

pub fn solidify_success_pattern(home: &Path, agent_name: &str, category: &str) -> Option<String> {
    let path = home.join("successes").join(format!("{agent_name}.json"));
    let successes: Vec<Success> = fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default();
    let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
    let recent = successes
        .iter()
        .filter(|success| success.category == category)
        .filter(|success| {
            chrono::DateTime::parse_from_rfc3339(&success.recorded_at)
                .map(|recorded_at| recorded_at.with_timezone(&chrono::Utc) > cutoff)
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    if recent.len() < solidify_threshold() {
        return None;
    }

    let rule_category = format!("success_{category}");
    let rule_id = format!("sp-{agent_name}-{category}");
    let rule_path = home
        .join("rules")
        .join(format!("{agent_name}_{rule_category}.json"));
    if rule_path.exists() {
        return None;
    }

    let rule_text = format!("PATTERN: {category} — {}", recent.last()?.summary);
    let rule = Rule {
        id: rule_id.clone(),
        agent_name: agent_name.to_string(),
        category: rule_category.clone(),
        rule_text: rule_text.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        trigger_count: recent.len(),
        synthesis_method: None,
    };

    if let Some(parent) = rule_path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            tracing::warn!(?e, ?parent, "failed to create success rules directory");
            return None;
        }
    }
    let serialized = serde_json::to_string_pretty(&rule).ok()?;
    if let Err(e) = fs::write(&rule_path, serialized) {
        tracing::warn!(?e, ?rule_path, "failed to write success rule");
        return None;
    }

    // Write/merge to shared.json
    let shared_path = home.join("rules").join("shared.json");
    if let Err(e) = merge_to_shared_rules(&shared_path, &rule) {
        tracing::warn!(?e, "failed to merge success rule to shared.json");
    }

    // Inject success pattern rule into agent's .agents/AGENTS.md
    inject_rule_to_agents_md_for_binding(home, agent_name, &rule_category, &rule_text);

    let vault = obsidian_vault_path();
    inject_rule_to_obsidian(&vault, agent_name, &rule_category, &rule_text, recent.len());
    spawn_mem0_sync(&rule);
    tracing::info!(agent_name, category, "success pattern solidified");
    Some(rule_id)
}

/// Delete mistake files older than 90 days to prevent unbounded growth.
pub fn cleanup_old_mistakes(home: &Path) {
    let mistakes_dir = home.join("mistakes");
    let Ok(entries) = fs::read_dir(&mistakes_dir) else {
        return;
    };
    let cutoff = chrono::Utc::now() - chrono::Duration::days(90);
    for entry in entries.filter_map(Result::ok) {
        if entry.path().extension().and_then(|s| s.to_str()) == Some("json") {
            if let Ok(content) = fs::read_to_string(entry.path()) {
                if let Ok(m) = serde_json::from_str::<Mistake>(&content) {
                    if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&m.timestamp) {
                        if ts.with_timezone(&chrono::Utc) < cutoff {
                            if let Err(e) = fs::remove_file(entry.path()) {
                                tracing::warn!(?e, path = ?entry.path(), "failed to delete old mistake file");
                            }
                        }
                    }
                }
            }
        }
    }
    sweep_orphan_mistakes(home);
}

pub fn sweep_orphan_mistakes(home: &Path) {
    let fleet_path = crate::fleet::fleet_yaml_path(home);
    let Ok(config) = crate::fleet::FleetConfig::load(&fleet_path) else {
        tracing::warn!(?fleet_path, "failed to load fleet.yaml, skipping sweep");
        return;
    };
    let active_agents: std::collections::HashSet<String> =
        config.instances.keys().cloned().collect();

    let mistakes_dir = home.join("mistakes");
    let Ok(entries) = fs::read_dir(&mistakes_dir) else {
        return;
    };

    let cutoff = chrono::Utc::now() - chrono::Duration::days(30);

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(mut m_val) = serde_json::from_str::<serde_json::Value>(&content) {
                    let mut is_orphan = false;
                    let mut agent_name = String::new();
                    if let Some(name) = m_val["agent_name"].as_str() {
                        if !active_agents.contains(name) {
                            if let Some(timestamp_str) = m_val["timestamp"].as_str() {
                                if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(timestamp_str)
                                {
                                    if ts.with_timezone(&chrono::Utc) < cutoff {
                                        is_orphan = true;
                                        agent_name = name.to_string();
                                    }
                                }
                            }
                        }
                    }
                    if is_orphan && m_val["orphaned"].as_bool() != Some(true) {
                        m_val["orphaned"] = serde_json::Value::Bool(true);
                        if let Ok(serialized) = serde_json::to_string_pretty(&m_val) {
                            if let Err(e) = fs::write(&path, serialized) {
                                tracing::warn!(?e, ?path, "failed to write orphaned mistake file");
                            } else {
                                copy_orphaned_agent_rules_to_shared(home, &agent_name);
                            }
                        }
                    }
                }
            }
        }
    }
}

fn copy_orphaned_agent_rules_to_shared(home: &Path, agent_name: &str) {
    let rules_dir = home.join("rules");
    let Ok(entries) = fs::read_dir(&rules_dir) else {
        return;
    };
    let shared_path = rules_dir.join("shared.json");
    for entry in entries.filter_map(Result::ok) {
        if entry.path().extension().is_some_and(|ext| ext == "json") {
            if entry.file_name() == "shared.json" {
                continue;
            }
            if let Ok(content) = fs::read_to_string(entry.path()) {
                if let Ok(rule) = serde_json::from_str::<Rule>(&content) {
                    if rule.agent_name == agent_name {
                        if let Err(e) = merge_to_shared_rules(&shared_path, &rule) {
                            tracing::warn!(
                                ?e,
                                ?shared_path,
                                "failed to merge orphaned agent rule to shared"
                            );
                        }
                    }
                }
            }
        }
    }
}

fn merge_to_shared_rules(shared_path: &Path, new_rule: &Rule) -> std::io::Result<()> {
    if let Some(parent) = shared_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut shared_rules: Vec<Rule> = if shared_path.exists() {
        let content = fs::read_to_string(shared_path)?;
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut found = false;
    for rule in &mut shared_rules {
        if rule.category == new_rule.category {
            if new_rule.trigger_count > rule.trigger_count {
                rule.rule_text = new_rule.rule_text.clone();
                rule.trigger_count = new_rule.trigger_count;
                rule.id = new_rule.id.clone();
                rule.agent_name = new_rule.agent_name.clone();
            }
            found = true;
            break;
        }
    }

    if !found {
        shared_rules.push(new_rule.clone());
    }

    let serialized = serde_json::to_string_pretty(&shared_rules)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    fs::write(shared_path, serialized)?;

    Ok(())
}

/// Minimum increase in `trigger_count` since the last write before re-synthesizing
/// `rule_text` (below this delta, only the count is bumped).
const RESOLIDIFY_DELTA: usize = 3;

/// Persist a rule after repeated mistakes, inject it into AGENTS.md, and sync it to Mem0.
pub fn solidify_rule(
    home: &Path,
    agent_name: &str,
    category: &str,
    trigger_count: usize,
) -> Option<String> {
    let mistakes_dir = home.join("mistakes");
    let mut recent_mistakes = Vec::new();
    if let Ok(entries) = fs::read_dir(&mistakes_dir) {
        for entry in entries.filter_map(Result::ok) {
            if entry.path().extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    if let Ok(m_val) = serde_json::from_str::<serde_json::Value>(&content) {
                        if m_val["orphaned"].as_bool() == Some(true) {
                            continue;
                        }
                    }
                    if let Ok(m) = serde_json::from_str::<Mistake>(&content) {
                        if m.agent_name == agent_name && m.category == category {
                            recent_mistakes.push(m);
                        }
                    }
                }
            }
        }
    }

    let has_correction = recent_mistakes.iter().any(|m| m.corrected_at.is_some());
    if !has_correction {
        tracing::debug!(
            agent_name,
            category,
            "solidify skipped: no successful correction yet"
        );
        return None;
    }

    recent_mistakes.sort_by(|a, b| {
        let ts_a = chrono::DateTime::parse_from_rfc3339(&a.timestamp)
            .map(|ts| ts.with_timezone(&chrono::Utc))
            .unwrap_or(chrono::DateTime::UNIX_EPOCH);
        let ts_b = chrono::DateTime::parse_from_rfc3339(&b.timestamp)
            .map(|ts| ts.with_timezone(&chrono::Utc))
            .unwrap_or(chrono::DateTime::UNIX_EPOCH);
        ts_b.cmp(&ts_a)
    });

    let rules_dir = home.join("rules");
    if let Err(e) = fs::create_dir_all(&rules_dir) {
        tracing::warn!(?e, "failed to create rules directory");
        return None;
    }

    let rule_id = format!("rule_{}_{}", agent_name, category);
    let rule_path = rules_dir.join(format!("{}.json", rule_id));
    let existing_rule: Option<Rule> = fs::read_to_string(&rule_path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok());

    let (rule_text, synthesis_method, created_at, refresh_external) = match existing_rule {
        Some(existing) => {
            let delta = trigger_count.saturating_sub(existing.trigger_count);
            if delta >= RESOLIDIFY_DELTA {
                let (text, method) = synthesize_rule_text_with_llm_fallback(
                    home,
                    agent_name,
                    category,
                    &recent_mistakes,
                );
                (text, Some(method), existing.created_at, true)
            } else {
                (
                    existing.rule_text,
                    existing.synthesis_method,
                    existing.created_at,
                    false,
                )
            }
        }
        None => {
            let (text, method) =
                synthesize_rule_text_with_llm_fallback(home, agent_name, category, &recent_mistakes);
            (text, Some(method), chrono::Utc::now().to_rfc3339(), true)
        }
    };

    let rule = Rule {
        id: rule_id.clone(),
        agent_name: agent_name.to_string(),
        category: category.to_string(),
        rule_text: rule_text.clone(),
        trigger_count,
        created_at,
        synthesis_method,
    };

    if let Ok(serialized) = serde_json::to_string_pretty(&rule) {
        if let Err(e) = fs::write(&rule_path, serialized) {
            tracing::warn!(?e, ?rule_path, "failed to write rule file");
        }
    }

    // Write/merge to shared.json
    let shared_path = rules_dir.join("shared.json");
    if let Err(e) = merge_to_shared_rules(&shared_path, &rule) {
        tracing::warn!(?e, "failed to merge rule to shared.json");
    }

    if refresh_external {
        inject_rule_to_agents_md_for_binding(home, agent_name, category, &rule_text);
        spawn_mem0_sync(&rule);
        let vault = obsidian_vault_path();
        inject_rule_to_obsidian(&vault, agent_name, category, &rule_text, trigger_count);
    }

    Some(rule_id)
}

/// Inject a rule/pattern into the agent's AGENTS.md files,
/// either via the active binding or scanning all fallback worktrees.
pub fn inject_rule_to_agents_md_for_binding(
    home: &Path,
    agent_name: &str,
    category: &str,
    rule_text: &str,
) {
    let mut injected = false;
    if let Some(binding) = crate::binding::read(home, agent_name) {
        if let Some(worktree_path) = binding["worktree"].as_str() {
            let agents_md_path = Path::new(worktree_path).join(".agents").join("AGENTS.md");
            if let Err(e) = inject_rule_to_agents_md(&agents_md_path, category, rule_text) {
                tracing::warn!(?e, ?agents_md_path, "failed to inject rule to AGENTS.md");
            } else {
                tracing::info!(
                    ?agents_md_path,
                    category,
                    "solidified rule injected to AGENTS.md"
                );
                injected = true;
            }
        }
    }

    if !injected {
        // Fallback: scan ~/.agend/worktrees/<agent_name>/
        let worktrees_base = home.join("worktrees").join(agent_name);
        if let Ok(entries) = fs::read_dir(&worktrees_base) {
            for entry in entries.filter_map(Result::ok) {
                let agents_md = entry.path().join(".agents").join("AGENTS.md");
                if agents_md.exists() {
                    if let Err(e) = inject_rule_to_agents_md(&agents_md, category, rule_text) {
                        tracing::warn!(
                            ?e,
                            ?agents_md,
                            "failed to inject rule to AGENTS.md via fallback"
                        );
                    } else {
                        tracing::info!(
                            ?agents_md,
                            category,
                            "solidified rule injected to AGENTS.md via fallback"
                        );
                    }
                }
            }
        }
    }
}

fn obsidian_vault_path() -> std::path::PathBuf {
    std::env::var("AGEND_OBSIDIAN_VAULT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(
                "/Users/neo/Library/Mobile Documents/iCloud~md~obsidian/Documents/agend-terminal",
            )
        })
}

fn yaml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn inject_rule_to_obsidian(
    vault: &Path,
    agent_name: &str,
    category: &str,
    rule_text: &str,
    trigger_count: usize,
) {
    let rules_dir = vault.join("Rules");
    if let Err(e) = fs::create_dir_all(&rules_dir) {
        tracing::warn!(?e, "failed to create Obsidian Rules dir");
        return;
    }
    let filename = format!("{agent_name}_{category}.md");
    let quoted_rule = rule_text.replace('\n', "\n> ");
    let content = format!(
        "---\nagent: {}\ncategory: {}\ntrigger_count: {trigger_count}\nupdated_at: {}\n---\n\n# Rule: {category}\n\n**Agent:** {agent_name}\n**Category:** {category}\n**Triggered:** {trigger_count} times\n\n## Rule\n\n> {quoted_rule}\n",
        yaml_quote(agent_name),
        yaml_quote(category),
        chrono::Utc::now().to_rfc3339()
    );
    if let Err(e) = fs::write(rules_dir.join(&filename), content) {
        tracing::warn!(?e, filename, "failed to write Obsidian rule");
    } else {
        tracing::info!(filename, "rule synced to Obsidian");
    }
}

fn spawn_mem0_sync(rule: &Rule) {
    let rule_text = rule.rule_text.clone();
    let agent_name = rule.agent_name.clone();
    let category = rule.category.clone();
    let count = rule.trigger_count;
    let url = mem0_sync_url();

    // fire-and-forget: sync rule to Mem0, non-critical
    if let Err(e) = std::thread::Builder::new()
        .name("agend-mem0-rule-sync".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::warn!(error = %e, "Mem0 rule sync: failed to build runtime");
                    return;
                }
            };
            rt.block_on(async move {
                let body = mem0_sync_body(&agent_name, &rule_text, &category, count);
                if let Err(e) = reqwest::Client::new()
                    .post(url)
                    .json(&body)
                    .timeout(std::time::Duration::from_secs(5))
                    .send()
                    .await
                {
                    tracing::warn!(error = %e, "Mem0 rule sync failed (non-fatal)");
                }
            });
        })
    {
        tracing::warn!(error = %e, "Mem0 rule sync: failed to spawn worker thread");
    }
}

fn mem0_sync_body(
    agent_name: &str,
    rule_text: &str,
    category: &str,
    trigger_count: usize,
) -> serde_json::Value {
    let user_id = std::env::var("MEM0_USER_ID").unwrap_or_else(|_| "neo".to_string());
    serde_json::json!({
        "content": format!(
            "Agent {} 的規則：{}（來源：Reflexion Loop，分類：{}，觸發次數：{}）",
            agent_name, rule_text, category, trigger_count
        ),
        "user_id": user_id
    })
}

#[cfg(not(test))]
fn mem0_sync_url() -> String {
    MEM0_SYNC_URL.to_string()
}

#[cfg(test)]
fn mem0_sync_url() -> String {
    MEM0_SYNC_URL_OVERRIDE
        .lock()
        .expect("mem0 sync url override mutex poisoned")
        .clone()
        .unwrap_or_else(|| MEM0_SYNC_URL.to_string())
}

#[cfg(test)]
static MEM0_SYNC_URL_OVERRIDE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Inject the solidified rule into the target AGENTS.md file inside the marker block.
pub fn inject_rule_to_agents_md(
    agents_md_path: &Path,
    category: &str,
    rule_text: &str,
) -> std::io::Result<()> {
    let mut content = if agents_md_path.exists() {
        fs::read_to_string(agents_md_path)?
    } else {
        String::new()
    };

    let start_marker = "<!-- agend-rules:start -->";
    let end_marker = "<!-- agend-rules:end -->";

    let rule_entry = format!("- **{}**: {}", category, rule_text);

    if let (Some(start_idx), Some(end_idx)) = (content.find(start_marker), content.find(end_marker))
    {
        if start_idx < end_idx {
            let before = &content[..start_idx];
            let after = &content[end_idx + end_marker.len()..];
            let inner = &content[start_idx + start_marker.len()..end_idx];

            let mut lines: Vec<String> = inner
                .lines()
                .map(|s| s.to_string())
                .filter(|s| !s.trim().is_empty())
                .collect();

            let prefix = format!("- **{}**:", category);
            let mut found = false;
            for line in &mut lines {
                if line.trim().starts_with(&prefix) {
                    *line = rule_entry.clone();
                    found = true;
                    break;
                }
            }
            if !found {
                lines.push(rule_entry);
            }

            let mut new_inner = String::from("\n## Solidified Rules (MistakeNotebook)\n");
            for line in lines {
                if !line.contains("## Solidified Rules") {
                    new_inner.push_str(&line);
                    new_inner.push('\n');
                }
            }
            content = format!(
                "{}{}{}{}{}",
                before, start_marker, new_inner, end_marker, after
            );
        }
    } else {
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(start_marker);
        content.push_str("\n## Solidified Rules (MistakeNotebook)\n");
        content.push_str(&rule_entry);
        content.push('\n');
        content.push_str(end_marker);
        content.push('\n');
    }

    if let Some(parent) = agents_md_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(agents_md_path, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_home(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-reflexion-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn test_classify_mistake_missing_test_execution() {
        let rejection = "You didn't do it right.";
        let parent = "VERIFIED\nEvidence:\nran: cargo check -> success\ncited: mod.rs:110";
        assert_eq!(
            classify_mistake(rejection, Some(parent)),
            Some("missing_test_execution")
        );

        let parent2 = "REJECTED\nEvidence:\nran: cargo check\ncited: mod.rs:110";
        assert_eq!(
            classify_mistake(rejection, Some(parent2)),
            Some("missing_test_execution")
        );

        let parent3 = "VERIFIED\nEvidence:\nran: cargo test\ncited: mod.rs:110";
        assert_ne!(
            classify_mistake(rejection, Some(parent3)),
            Some("missing_test_execution")
        );

        let rejection2 = "You forgot to run cargo test.";
        assert_eq!(
            classify_mistake(rejection2, None),
            Some("missing_test_execution")
        );
    }

    #[test]
    fn test_classify_mistake_wrong_branch_target() {
        let rejection1 = "The PR base branch points at upstream instead of the fork.";
        assert_eq!(
            classify_mistake(rejection1, None),
            Some("wrong_branch_target")
        );

        let rejection2 = "The branch target is the upstream base branch, not the fork.";
        assert_eq!(
            classify_mistake(rejection2, None),
            Some("wrong_branch_target")
        );
    }

    #[test]
    fn test_classify_mistake_lint_failure() {
        let rejection1 = "Clippy failed with warning.";
        assert_eq!(classify_mistake(rejection1, None), Some("lint_failure"));

        let rejection2 = "Run cargo clippy before submitting.";
        assert_eq!(classify_mistake(rejection2, None), Some("lint_failure"));
    }

    #[test]
    fn test_classify_mistake_wrong_pr_repo() {
        let rejection = "gh pr create used --repo suzuke/agend-terminal for this pull request.";
        assert_eq!(classify_mistake(rejection, None), Some("wrong_pr_repo"));
    }

    #[test]
    fn test_classify_mistake_missing_evidence() {
        let rejection = "VERIFIED report was sent without evidence and no ### Evidence block.";
        assert_eq!(classify_mistake(rejection, None), Some("missing_evidence"));
    }

    #[test]
    fn test_classify_mistake_hardcoded_secret() {
        let rejection = "This hardcoded API key should be read from an environment variable.";
        assert_eq!(classify_mistake(rejection, None), Some("hardcoded_secret"));
    }

    #[test]
    fn test_classify_mistake_missing_fire_and_forget() {
        let rejection = "tokio::spawn is missing the required fire-and-forget comment.";
        assert_eq!(
            classify_mistake(rejection, None),
            Some("missing_fire_and_forget")
        );
    }

    #[test]
    fn test_classify_mistake_wrong_branch_base() {
        let rejection = "This feature branch was branched from a stale base.";
        assert_eq!(classify_mistake(rejection, None), Some("wrong_branch_base"));
    }

    #[test]
    fn test_classify_mistake_incomplete_implementation() {
        let rejection1 = "The change is functionally incomplete and still has a TODO left in.";
        assert_eq!(
            classify_mistake(rejection1, None),
            Some("incomplete_implementation")
        );

        let rejection2 = "This is a stub implementation with partial logic only.";
        assert_eq!(
            classify_mistake(rejection2, None),
            Some("incomplete_implementation")
        );
    }

    #[test]
    fn test_classify_mistake_test_failure() {
        let rejection1 = "cargo test failed with a regression in the handler tests.";
        assert_eq!(classify_mistake(rejection1, None), Some("test_failure"));

        let rejection2 = "The commit broke the existing test suite.";
        assert_eq!(classify_mistake(rejection2, None), Some("test_failure"));
    }

    #[test]
    fn test_classify_mistake_missing_pr_description() {
        let rejection1 = "The PR body is empty — please add a meaningful description.";
        assert_eq!(
            classify_mistake(rejection1, None),
            Some("missing_pr_description")
        );

        let rejection2 = "You opened a pull request with a trivial PR description.";
        assert_eq!(
            classify_mistake(rejection2, None),
            Some("missing_pr_description")
        );
    }

    #[test]
    fn test_classify_mistake_unknown_falls_back_to_unclassified() {
        let rejection = "The report missed the operational nuance in this workflow.";
        assert_eq!(classify_mistake(rejection, None), Some("unclassified"));
    }

    #[test]
    fn test_record_mistake_keeps_agent_name_when_parent_sender_is_coordinator() {
        let home = tmp_home("record_mistake_parent_sender_test");
        let agent = "worker-agent";
        let parent_id = "parent-msg-coordinator";
        let parent_msg = crate::inbox::InboxMessage {
            id: Some(parent_id.to_string()),
            from: "from:general".to_string(),
            text: "UNVERIFIED\noperator note without matching category".to_string(),
            kind: Some("task".to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        };
        let inbox_dir = home.join("inbox");
        std::fs::create_dir_all(&inbox_dir).expect("failed to create inbox dir");
        let inbox_file = inbox_dir.join(format!("{agent}.jsonl"));
        std::fs::write(
            &inbox_file,
            format!(
                "{}\n",
                serde_json::to_string(&parent_msg).expect("failed to serialize parent msg")
            ),
        )
        .expect("failed to write parent msg inbox file");

        let args = json!({
            "parent_id": parent_id,
            "correlation_id": "task-parent-agent",
            "artifacts": "unclassified reviewer concern"
        });
        let rule_id = record_mistake(
            &home,
            "general",
            agent,
            "REJECTED\n### Evidence\ncited: src/lib.rs:1 -- concern",
            &args,
            None,
        );
        assert!(rule_id.is_none(), "single mistake should not solidify");

        let mistake_file = std::fs::read_dir(home.join("mistakes"))
            .expect("read mistakes dir")
            .next()
            .expect("one mistake file")
            .expect("mistake dir entry")
            .path();
        let mistake: Mistake = serde_json::from_str(
            &std::fs::read_to_string(mistake_file).expect("read mistake file"),
        )
        .expect("deserialize mistake");
        assert_eq!(mistake.agent_name, agent);
        assert_eq!(mistake.category, "unclassified");

        std::fs::remove_dir_all(&home).ok();
    }

    #[serial_test::serial(mem0_sync)]
    #[test]
    fn test_record_mistake_and_solidify_threshold() {
        let home = tmp_home("record_mistake_test");
        let agent = "test-agent";

        let worktree_dir = home.join("mock_worktree");
        std::fs::create_dir_all(&worktree_dir).expect("failed to create mock worktree");
        let binding_dir = home.join("runtime").join(agent);
        std::fs::create_dir_all(&binding_dir).expect("failed to create binding dir");
        let binding_json = json!({
            "worktree": worktree_dir.to_str().expect("invalid worktree path"),
            "branch": "feat/mock-branch"
        });
        std::fs::write(
            binding_dir.join("binding.json"),
            serde_json::to_string(&binding_json).expect("failed to serialize binding json"),
        )
        .expect("failed to write binding.json");

        let parent_id = "parent-msg-123";
        let parent_msg = crate::inbox::InboxMessage {
            id: Some(parent_id.to_string()),
            from: format!("from:{}", agent),
            text: "VERIFIED\nEvidence:\nran: cargo check -> success\ncited: mod.rs:110".to_string(),
            kind: Some("report".to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        };
        let inbox_dir = home.join("inbox");
        std::fs::create_dir_all(&inbox_dir).expect("failed to create inbox dir");
        let inbox_file = inbox_dir.join(format!("{}.jsonl", agent));
        std::fs::write(
            &inbox_file,
            format!(
                "{}\n",
                serde_json::to_string(&parent_msg).expect("failed to serialize parent msg")
            ),
        )
        .expect("failed to write parent msg inbox file");

        let mistakes_dir = home.join("mistakes");
        assert!(!mistakes_dir.exists());

        let args = json!({
            "parent_id": parent_id,
            "correlation_id": "task-abc",
            "artifacts": "evidence of failure"
        });
        let rule_id = record_mistake(
            &home,
            "general",
            agent,
            "REJECTED: no cargo test executed",
            &args,
            None,
        );
        assert!(rule_id.is_none(), "Threshold not reached yet");

        let rule_id = record_mistake(
            &home,
            "general",
            agent,
            "REJECTED: missing test run",
            &args,
            None,
        );
        assert!(rule_id.is_none(), "Threshold not reached yet");

        mark_mistake_corrected(&home, agent, "missing_test_execution");

        let rule_id = record_mistake(
            &home,
            "general",
            agent,
            "REJECTED: did not run cargo test",
            &args,
            None,
        );
        assert!(
            rule_id.is_some(),
            "Threshold reached; rule should be solidified"
        );
        let rule_id_str = rule_id.expect("expected rule id");
        assert_eq!(
            rule_id_str,
            format!("rule_{}_missing_test_execution", agent)
        );

        let files = std::fs::read_dir(&mistakes_dir).expect("failed to read mistakes dir");
        assert_eq!(files.count(), 3);

        let rule_path = home.join("rules").join(format!("{}.json", rule_id_str));
        assert!(rule_path.exists());
        let rule_content = std::fs::read_to_string(&rule_path).expect("failed to read rule file");
        let rule: Rule = serde_json::from_str(&rule_content).expect("failed to deserialize rule");
        assert!(rule
            .rule_text
            .starts_with("NEVER report VERIFIED without running cargo test"));
        assert!(rule.rule_text.contains("Recurring failures:"));
        assert!(rule.rule_text.contains("- evidence of failure"));
        assert_eq!(rule.trigger_count, 3);

        let agents_md_path = worktree_dir.join(".agents").join("AGENTS.md");
        assert!(agents_md_path.exists());
        let agents_md_content =
            std::fs::read_to_string(&agents_md_path).expect("failed to read AGENTS.md");
        assert!(agents_md_content.contains("<!-- agend-rules:start -->"));
        assert!(agents_md_content.contains("NEVER report VERIFIED without running cargo test"));
        assert!(agents_md_content.contains("<!-- agend-rules:end -->"));

        std::fs::remove_dir_all(&home).ok();
    }

    fn with_solidify_threshold_env<R>(value: Option<&str>, f: impl FnOnce() -> R) -> R {
        let _guard = crate::daemon::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("AGEND_SOLIDIFY_THRESHOLD").ok();
        unsafe {
            match value {
                Some(v) => std::env::set_var("AGEND_SOLIDIFY_THRESHOLD", v),
                None => std::env::remove_var("AGEND_SOLIDIFY_THRESHOLD"),
            }
        }
        let r = f();
        unsafe {
            match prior {
                Some(v) => std::env::set_var("AGEND_SOLIDIFY_THRESHOLD", v),
                None => std::env::remove_var("AGEND_SOLIDIFY_THRESHOLD"),
            }
        }
        r
    }

    #[test]
    fn test_solidify_threshold_env_var() {
        with_solidify_threshold_env(None, || {
            assert_eq!(solidify_threshold(), 3);
        });
        with_solidify_threshold_env(Some("0"), || {
            assert_eq!(solidify_threshold(), 3);
        });
        with_solidify_threshold_env(Some("2"), || {
            assert_eq!(solidify_threshold(), 2);
        });
    }

    #[serial_test::serial(mem0_sync)]
    #[test]
    fn test_solidify_threshold_two_mistakes_triggers_early() {
        with_solidify_threshold_env(Some("2"), || {
            let home = tmp_home("solidify_threshold_two_test");
            let agent = "threshold-agent";

            let worktree_dir = home.join("mock_worktree");
            std::fs::create_dir_all(&worktree_dir).expect("failed to create mock worktree");
            let binding_dir = home.join("runtime").join(agent);
            std::fs::create_dir_all(&binding_dir).expect("failed to create binding dir");
            let binding_json = json!({
                "worktree": worktree_dir.to_str().expect("invalid worktree path"),
                "branch": "feat/mock-branch"
            });
            std::fs::write(
                binding_dir.join("binding.json"),
                serde_json::to_string(&binding_json).expect("failed to serialize binding json"),
            )
            .expect("failed to write binding.json");

            let parent_id = "parent-msg-threshold";
            let parent_msg = crate::inbox::InboxMessage {
                id: Some(parent_id.to_string()),
                from: format!("from:{}", agent),
                text: "VERIFIED\nEvidence:\nran: cargo check -> success\ncited: mod.rs:110"
                    .to_string(),
                kind: Some("report".to_string()),
                timestamp: chrono::Utc::now().to_rfc3339(),
                ..Default::default()
            };
            let inbox_dir = home.join("inbox");
            std::fs::create_dir_all(&inbox_dir).expect("failed to create inbox dir");
            let inbox_file = inbox_dir.join(format!("{}.jsonl", agent));
            std::fs::write(
                &inbox_file,
                format!(
                    "{}\n",
                    serde_json::to_string(&parent_msg).expect("failed to serialize parent msg")
                ),
            )
            .expect("failed to write parent msg inbox file");

            let args = json!({
                "parent_id": parent_id,
                "correlation_id": "task-threshold",
                "artifacts": "evidence of failure"
            });

            let rule_id = record_mistake(
                &home,
                "general",
                agent,
                "REJECTED: no cargo test executed",
                &args,
                None,
            );
            assert!(rule_id.is_none(), "first mistake should not solidify");

            mark_mistake_corrected(&home, agent, "missing_test_execution");

            let rule_id = record_mistake(
                &home,
                "general",
                agent,
                "REJECTED: did not run cargo test",
                &args,
                None,
            );
            assert!(
                rule_id.is_some(),
                "second mistake should solidify with threshold=2"
            );

            std::fs::remove_dir_all(&home).ok();
        });
    }

    fn with_mem0_user_id_cleared<R>(f: impl FnOnce() -> R) -> R {
        let _guard = crate::daemon::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("MEM0_USER_ID").ok();
        unsafe {
            std::env::remove_var("MEM0_USER_ID");
        }
        let result = f();
        unsafe {
            match previous {
                Some(value) => std::env::set_var("MEM0_USER_ID", value),
                None => std::env::remove_var("MEM0_USER_ID"),
            }
        }
        result
    }

    #[serial_test::serial(mem0_sync)]
    #[tokio::test]
    async fn test_solidify_triggers_mem0_sync() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let previous_mem0_user = {
            let _guard = crate::daemon::test_env_lock()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var("MEM0_USER_ID").ok();
            unsafe {
                std::env::remove_var("MEM0_USER_ID");
            }
            previous
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind test listener");
        let addr = listener.local_addr().expect("failed to read listener addr");
        {
            let mut override_url = MEM0_SYNC_URL_OVERRIDE
                .lock()
                .expect("mem0 sync url override mutex poisoned");
            *override_url = Some(format!("http://{addr}/add"));
        }

        let request = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("failed to accept request");
            let mut buf = vec![0_u8; 8192];
            let mut total = 0;
            loop {
                let n = socket
                    .read(&mut buf[total..])
                    .await
                    .expect("failed to read request");
                assert_ne!(n, 0, "connection closed before request body arrived");
                total += n;

                let request = String::from_utf8_lossy(&buf[..total]);
                if let Some(header_end) = request.find("\r\n\r\n") {
                    let content_length = request
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length: ")
                                .or_else(|| line.strip_prefix("Content-Length: "))
                        })
                        .and_then(|value| value.parse::<usize>().ok())
                        .expect("request should include content-length");
                    if total >= header_end + 4 + content_length {
                        socket
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                            .await
                            .expect("failed to write response");
                        return String::from_utf8_lossy(&buf[..total]).to_string();
                    }
                }
            }
        });

        let home = tmp_home("mem0_sync_test");
        let mistakes_dir = home.join("mistakes");
        std::fs::create_dir_all(&mistakes_dir).expect("failed to create mistakes dir");
        let mock_mistake = Mistake {
            id: "mock_mstk_1".to_string(),
            task_id: None,
            agent_name: "test-agent".to_string(),
            category: "lint_failure".to_string(),
            rejection_reason: "mock".to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            corrected_at: Some(chrono::Utc::now().to_rfc3339()),
        };
        std::fs::write(
            mistakes_dir.join("mock_mstk_1.json"),
            serde_json::to_string(&mock_mistake).expect("failed to serialize mock mistake"),
        )
        .expect("failed to write mock mistake");

        let rule_id = solidify_rule(&home, "test-agent", "lint_failure", 4)
            .expect("expected rule to solidify");
        assert_eq!(rule_id, "rule_test-agent_lint_failure");

        let request = tokio::time::timeout(std::time::Duration::from_secs(5), request)
            .await
            .expect("timed out waiting for Mem0 sync request")
            .expect("request task failed");

        assert!(request.starts_with("POST /add HTTP/1.1"));
        assert!(request.contains("\"user_id\":\"neo\""));
        assert!(request.contains("Agent test-agent"));
        assert!(request.contains("NEVER submit code with clippy warnings or lint failures"));
        assert!(request.contains("Reflexion Loop"));
        assert!(request.contains("lint_failure"));
        assert!(request.contains("4"));

        {
            let mut override_url = MEM0_SYNC_URL_OVERRIDE
                .lock()
                .expect("mem0 sync url override mutex poisoned");
            *override_url = None;
        }

        {
            let _guard = crate::daemon::test_env_lock()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            unsafe {
                match previous_mem0_user {
                    Some(value) => std::env::set_var("MEM0_USER_ID", value),
                    None => std::env::remove_var("MEM0_USER_ID"),
                }
            }
        }
    }

    #[serial_test::serial(mem0_sync)]
    #[test]
    fn test_spawn_mem0_sync_posts_without_existing_tokio_runtime() {
        with_mem0_user_id_cleared(|| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")
                .expect("failed to bind Mem0 sync test listener");
            listener
                .set_nonblocking(true)
                .expect("failed to set listener nonblocking");
            let addr = listener.local_addr().expect("failed to read listener addr");
            {
                let mut override_url = MEM0_SYNC_URL_OVERRIDE
                    .lock()
                    .expect("mem0 sync url override mutex poisoned");
                *override_url = Some(format!("http://{addr}/add"));
            }

            let server = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                loop {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let request = read_http_request(&mut stream);
                            use std::io::Write;
                            stream
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                                .expect("failed to write response");
                            return Some(request);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if std::time::Instant::now() >= deadline {
                                return None;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(e) => panic!("failed to accept request: {e}"),
                    }
                }
            });

            let rule = Rule {
                id: "rule_mem0_plain_thread".to_string(),
                agent_name: "plain-agent".to_string(),
                category: "lint_failure".to_string(),
                rule_text: "No lint warnings".to_string(),
                created_at: chrono::Utc::now().to_rfc3339(),
                trigger_count: 2,
                synthesis_method: None,
            };

            spawn_mem0_sync(&rule);

            let request = server
                .join()
                .expect("Mem0 sync test server panicked")
                .expect("Mem0 sync should issue a request without an ambient Tokio runtime");
            assert!(request.starts_with("POST /add HTTP/1.1"));
            assert!(request.contains("\"user_id\":\"neo\""));
            assert!(request.contains("Agent plain-agent"));

            {
                let mut override_url = MEM0_SYNC_URL_OVERRIDE
                    .lock()
                    .expect("mem0 sync url override mutex poisoned");
                *override_url = None;
            }
        });
    }

    #[test]
    fn test_mem0_sync_body_uses_mem0_user_id_env() {
        let _guard = crate::daemon::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("MEM0_USER_ID").ok();
        unsafe {
            std::env::remove_var("MEM0_USER_ID");
        }
        let default_body = mem0_sync_body("agent", "rule", "category", 1);
        assert_eq!(default_body["user_id"], "neo");

        unsafe {
            std::env::set_var("MEM0_USER_ID", "custom-user");
        }
        let custom_body = mem0_sync_body("agent", "rule", "category", 1);
        assert_eq!(custom_body["user_id"], "custom-user");

        unsafe {
            match previous {
                Some(value) => std::env::set_var("MEM0_USER_ID", value),
                None => std::env::remove_var("MEM0_USER_ID"),
            }
        }
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read;

        let mut buf = Vec::new();
        let mut tmp = [0; 1024];
        loop {
            let read = stream.read(&mut tmp).expect("failed to read request");
            assert_ne!(read, 0, "connection closed before request completed");
            buf.extend_from_slice(&tmp[..read]);
            if let Some(header_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                let full_len = header_end + 4 + content_length;
                if buf.len() >= full_len {
                    return String::from_utf8_lossy(&buf[..full_len]).into_owned();
                }
            }
        }
    }

    #[test]
    fn test_list_rules_filters_by_agent() {
        let home = tmp_home("list_rules_test");
        let rules_dir = home.join("rules");
        std::fs::create_dir_all(&rules_dir).expect("failed to create rules dir");

        let rule_a = Rule {
            id: "rule_agent_a_cat".to_string(),
            agent_name: "Agent-A".to_string(),
            category: "missing_test_execution".to_string(),
            rule_text: "Don't forget tests".to_string(),
            created_at: "2026-06-26T12:00:00Z".to_string(),
            trigger_count: 3,
            synthesis_method: None,
        };
        let rule_b = Rule {
            id: "rule_agent_b_cat".to_string(),
            agent_name: "Agent-B".to_string(),
            category: "lint_failure".to_string(),
            rule_text: "No lint warnings".to_string(),
            created_at: "2026-06-26T12:05:00Z".to_string(),
            trigger_count: 4,
            synthesis_method: None,
        };

        std::fs::write(
            rules_dir.join("rule_agent_a.json"),
            serde_json::to_string(&rule_a).expect("failed to serialize rule A"),
        )
        .expect("failed to write rule A");
        std::fs::write(
            rules_dir.join("rule_agent_b.json"),
            serde_json::to_string(&rule_b).expect("failed to serialize rule B"),
        )
        .expect("failed to write rule B");

        let rules_a = list_rules(&home, "Agent-A");
        assert_eq!(rules_a.len(), 1);
        assert_eq!(rules_a[0].id, "rule_agent_a_cat");
        assert_eq!(rules_a[0].agent_name, "Agent-A");
        assert_eq!(rules_a[0].trigger_count, 3);

        let rules_b = list_rules(&home, "Agent-B");
        assert_eq!(rules_b.len(), 1);
        assert_eq!(rules_b[0].id, "rule_agent_b_cat");
        assert_eq!(rules_b[0].agent_name, "Agent-B");
        assert_eq!(rules_b[0].trigger_count, 4);

        let rules_c = list_rules(&home, "Agent-C");
        assert!(rules_c.is_empty());

        std::fs::remove_dir_all(&home).ok();
    }

    #[serial_test::serial(mem0_sync)]
    #[test]
    fn test_record_mistake_with_category_hint() {
        let home = tmp_home("record_mistake_hint_test");
        let agent = "test-agent-hint";

        let worktree_dir = home.join("mock_worktree");
        std::fs::create_dir_all(&worktree_dir).expect("failed to create mock worktree");
        let binding_dir = home.join("runtime").join(agent);
        std::fs::create_dir_all(&binding_dir).expect("failed to create binding dir");
        let binding_json = json!({
            "worktree": worktree_dir.to_str().expect("invalid worktree path"),
            "branch": "feat/mock-branch-hint"
        });
        std::fs::write(
            binding_dir.join("binding.json"),
            serde_json::to_string(&binding_json).expect("failed to serialize binding json"),
        )
        .expect("failed to write binding.json");

        let args = json!({
            "correlation_id": "task-abc-hint",
            "artifacts": "evidence of wrong repo"
        });

        // 3 mistakes are needed to trigger solidification
        let rule_id = record_mistake(
            &home,
            "general",
            agent,
            "REJECTED: opened PR to wrong repo",
            &args,
            Some("wrong_pr_repo"),
        );
        assert!(rule_id.is_none());
        let rule_id = record_mistake(
            &home,
            "general",
            agent,
            "REJECTED: wrong pr repo",
            &args,
            Some("wrong_pr_repo"),
        );
        assert!(rule_id.is_none());
        mark_mistake_corrected(&home, agent, "wrong_pr_repo");

        let rule_id = record_mistake(
            &home,
            "general",
            agent,
            "REJECTED: pr to wrong repo again",
            &args,
            Some("wrong_pr_repo"),
        );

        let rule_id_str = rule_id.expect("expected solidified rule ID");
        assert_eq!(rule_id_str, format!("rule_{}_wrong_pr_repo", agent));

        let rule_path = home.join("rules").join(format!("{}.json", rule_id_str));
        assert!(rule_path.exists());
        let rule_content = std::fs::read_to_string(&rule_path).expect("failed to read rule file");
        let rule: Rule = serde_json::from_str(&rule_content).expect("failed to deserialize rule");
        assert!(rule.rule_text.starts_with(
            "NEVER open a PR to suzuke/agend-terminal; always use justdoit530-hub/agend-terminal"
        ));
        assert!(rule.rule_text.contains("Recurring failures:"));
        assert!(rule.rule_text.contains("- evidence of wrong repo"));

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_record_success_no_solidify_before_threshold() {
        let home = tmp_home("record_success_before_threshold_test");
        let agent = "success-agent";

        let first = record_success(
            &home,
            "reviewer",
            agent,
            "First clean review",
            "clean_review",
        );
        assert!(first.is_some(), "first success should be recorded");
        let second = record_success(
            &home,
            "reviewer",
            agent,
            "Second clean review",
            "clean_review",
        );
        assert!(second.is_some(), "second success should be recorded");

        let successes_path = home.join("successes").join(format!("{agent}.json"));
        let successes: Vec<Success> = serde_json::from_str(
            &std::fs::read_to_string(&successes_path).expect("read successes"),
        )
        .expect("deserialize successes");
        assert_eq!(successes.len(), 2);
        assert!(!home
            .join("rules")
            .join(format!("{agent}_success_clean_review.json"))
            .exists());

        std::fs::remove_dir_all(&home).ok();
    }

    #[serial_test::serial(mem0_sync)]
    #[test]
    fn test_solidify_success_pattern_at_threshold() {
        let home = tmp_home("solidify_success_pattern_test");
        let agent = "success-agent-threshold";

        // Setup mock fallback worktree
        let worktree_dir = home.join("worktrees").join(agent).join("mock_worktree_1");
        std::fs::create_dir_all(worktree_dir.join(".agents")).expect("failed to create agents dir");
        let agents_md_path = worktree_dir.join(".agents").join("AGENTS.md");
        std::fs::write(
            &agents_md_path,
            "<!-- agend-rules:start -->\n<!-- agend-rules:end -->",
        )
        .expect("failed to init AGENTS.md");

        assert!(record_success(&home, "reviewer", agent, "First pass", "clean_review").is_some());
        assert!(record_success(&home, "reviewer", agent, "Second pass", "clean_review").is_some());
        assert!(record_success(&home, "reviewer", agent, "Third pass", "clean_review").is_some());

        let rule_path = home
            .join("rules")
            .join(format!("{agent}_success_clean_review.json"));
        assert!(rule_path.exists(), "third success should solidify a rule");
        let rule: Rule =
            serde_json::from_str(&std::fs::read_to_string(rule_path).expect("read success rule"))
                .expect("deserialize success rule");
        assert_eq!(rule.id, format!("sp-{agent}-clean_review"));
        assert_eq!(rule.agent_name, agent);
        assert_eq!(rule.category, "success_clean_review");
        assert_eq!(rule.trigger_count, 3);
        assert!(rule.rule_text.contains("PATTERN: clean_review"));
        assert!(rule.rule_text.contains("Third pass"));

        // Verify rule injected to mock worktree's AGENTS.md
        let agents_md_content =
            std::fs::read_to_string(&agents_md_path).expect("failed to read AGENTS.md");
        assert!(agents_md_content.contains("<!-- agend-rules:start -->"));
        assert!(agents_md_content.contains("success_clean_review"));
        assert!(agents_md_content.contains("PATTERN: clean_review"));
        assert!(agents_md_content.contains("<!-- agend-rules:end -->"));

        std::fs::remove_dir_all(&home).ok();
    }

    #[serial_test::serial(mem0_sync)]
    #[test]
    fn test_solidify_rule_fallback_worktree_scan() {
        let home = tmp_home("solidify_fallback_test");
        let agent = "fallback-agent";

        // Create the worktree folder under ~/.agend/worktrees/<agent_name>/some_worktree/
        let worktree_dir = home.join("worktrees").join(agent).join("mock_worktree_1");
        std::fs::create_dir_all(worktree_dir.join(".agents")).expect("failed to create agents dir");

        let agents_md_path = worktree_dir.join(".agents").join("AGENTS.md");
        std::fs::write(
            &agents_md_path,
            "<!-- agend-rules:start -->\n<!-- agend-rules:end -->",
        )
        .expect("failed to init AGENTS.md");

        // No active binding is set up.
        let mistakes_dir = home.join("mistakes");
        std::fs::create_dir_all(&mistakes_dir).expect("failed to create mistakes dir");
        let mock_mistake = Mistake {
            id: "mock_mstk_2".to_string(),
            task_id: None,
            agent_name: agent.to_string(),
            category: "missing_test_execution".to_string(),
            rejection_reason: "mock".to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            corrected_at: Some(chrono::Utc::now().to_rfc3339()),
        };
        std::fs::write(
            mistakes_dir.join("mock_mstk_2.json"),
            serde_json::to_string(&mock_mistake).expect("failed to serialize mock mistake 2"),
        )
        .expect("failed to write mock mistake 2");

        let rule_id = solidify_rule(&home, agent, "missing_test_execution", 3)
            .expect("expected solidified rule ID");
        assert_eq!(rule_id, format!("rule_{}_missing_test_execution", agent));

        // The rule should still be written to rules/
        let rule_path = home.join("rules").join(format!("{}.json", rule_id));
        assert!(rule_path.exists());

        // And it should have fallback-injected into the mock worktree's AGENTS.md
        let agents_md_content =
            std::fs::read_to_string(&agents_md_path).expect("failed to read AGENTS.md");
        assert!(agents_md_content.contains("<!-- agend-rules:start -->"));
        assert!(agents_md_content.contains("NEVER report VERIFIED without running cargo test"));
        assert!(agents_md_content.contains("<!-- agend-rules:end -->"));

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_re_solidify_bumps_count_only_when_delta_below_threshold() {
        let home = tmp_home("re_solidify_count_only_test");
        let agent = "re-solidify-agent";
        let category = "lint_failure";
        let mistakes_dir = home.join("mistakes");
        std::fs::create_dir_all(&mistakes_dir).expect("failed to create mistakes dir");

        for (idx, reason) in [
            "Clippy warning on handler",
            "Lint failure in dispatch",
            "More clippy warnings",
        ]
        .iter()
        .enumerate()
        {
            let mistake = Mistake {
                id: format!("mock_mstk_{idx}"),
                task_id: None,
                agent_name: agent.to_string(),
                category: category.to_string(),
                rejection_reason: (*reason).to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                corrected_at: Some(chrono::Utc::now().to_rfc3339()),
            };
            std::fs::write(
                mistakes_dir.join(format!("mock_mstk_{idx}.json")),
                serde_json::to_string(&mistake).expect("failed to serialize mock mistake"),
            )
            .expect("failed to write mock mistake");
        }

        solidify_rule(&home, agent, category, 3).expect("initial solidify should succeed");
        let rule_path = home
            .join("rules")
            .join(format!("rule_{agent}_{category}.json"));
        let initial_rule: Rule =
            serde_json::from_str(&std::fs::read_to_string(&rule_path).expect("read rule"))
                .expect("deserialize rule");

        solidify_rule(&home, agent, category, 5).expect("count-only update should succeed");
        let updated_rule: Rule =
            serde_json::from_str(&std::fs::read_to_string(&rule_path).expect("read updated rule"))
                .expect("deserialize updated rule");

        assert_eq!(updated_rule.trigger_count, 5);
        assert_eq!(updated_rule.rule_text, initial_rule.rule_text);
        assert_eq!(updated_rule.created_at, initial_rule.created_at);

        std::fs::remove_dir_all(&home).ok();
    }

    #[serial_test::serial(mem0_sync)]
    #[test]
    fn test_re_solidify_count_only_does_not_trigger_external_sync() {
        with_mem0_user_id_cleared(|| {
            let home = tmp_home("re_solidify_no_external_sync_test");
            let agent = "re-solidify-no-sync-agent";
            let category = "lint_failure";
            let obsidian_vault = home.join("obsidian_vault");
            std::fs::create_dir_all(&obsidian_vault).expect("failed to create obsidian vault");

            let previous_obsidian_vault = std::env::var("AGEND_OBSIDIAN_VAULT").ok();
            unsafe {
                std::env::set_var(
                    "AGEND_OBSIDIAN_VAULT",
                    obsidian_vault
                        .to_str()
                        .expect("invalid obsidian vault path"),
                );
            }

            let worktree_dir = home.join("worktrees").join(agent).join("mock_worktree_1");
            std::fs::create_dir_all(worktree_dir.join(".agents"))
                .expect("failed to create agents dir");
            let agents_md_path = worktree_dir.join(".agents").join("AGENTS.md");
            std::fs::write(
                &agents_md_path,
                "<!-- agend-rules:start -->\n<!-- agend-rules:end -->",
            )
            .expect("failed to init AGENTS.md");

            let mistakes_dir = home.join("mistakes");
            std::fs::create_dir_all(&mistakes_dir).expect("failed to create mistakes dir");
            for (idx, reason) in [
                "Clippy warning on handler",
                "Lint failure in dispatch",
                "More clippy warnings",
            ]
            .iter()
            .enumerate()
            {
                let mistake = Mistake {
                    id: format!("mock_mstk_{idx}"),
                    task_id: None,
                    agent_name: agent.to_string(),
                    category: category.to_string(),
                    rejection_reason: (*reason).to_string(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    corrected_at: Some(chrono::Utc::now().to_rfc3339()),
                };
                std::fs::write(
                    mistakes_dir.join(format!("mock_mstk_{idx}.json")),
                    serde_json::to_string(&mistake).expect("failed to serialize mock mistake"),
                )
                .expect("failed to write mock mistake");
            }

            let mem0_listener = std::net::TcpListener::bind("127.0.0.1:0")
                .expect("failed to bind Mem0 sync test listener");
            mem0_listener
                .set_nonblocking(true)
                .expect("failed to set listener nonblocking");
            let mem0_addr = mem0_listener
                .local_addr()
                .expect("failed to read listener addr");
            {
                let mut override_url = MEM0_SYNC_URL_OVERRIDE
                    .lock()
                    .expect("mem0 sync url override mutex poisoned");
                *override_url = Some(format!("http://{mem0_addr}/add"));
            }

            let initial_mem0_server = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                loop {
                    match mem0_listener.accept() {
                        Ok((mut stream, _)) => {
                            let request = read_http_request(&mut stream);
                            use std::io::Write;
                            stream
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                                .expect("failed to write response");
                            return Some(request);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if std::time::Instant::now() >= deadline {
                                return None;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(e) => panic!("failed to accept initial Mem0 request: {e}"),
                    }
                }
            });

            solidify_rule(&home, agent, category, 3).expect("initial solidify should succeed");
            initial_mem0_server
                .join()
                .expect("initial Mem0 server panicked")
                .expect("initial solidify should trigger Mem0 sync");

            let rule_path = home
                .join("rules")
                .join(format!("rule_{agent}_{category}.json"));
            let initial_rule: Rule =
                serde_json::from_str(&std::fs::read_to_string(&rule_path).expect("read rule"))
                    .expect("deserialize rule");
            let agents_md_after_initial =
                std::fs::read_to_string(&agents_md_path).expect("read AGENTS.md after initial");
            let obsidian_md_path = obsidian_vault
                .join("Rules")
                .join(format!("{agent}_{category}.md"));
            assert!(
                obsidian_md_path.exists(),
                "initial solidify should write Obsidian rule"
            );
            let obsidian_after_initial = std::fs::read_to_string(&obsidian_md_path)
                .expect("read Obsidian rule after initial");

            let mem0_listener = std::net::TcpListener::bind("127.0.0.1:0")
                .expect("failed to bind second Mem0 sync test listener");
            mem0_listener
                .set_nonblocking(true)
                .expect("failed to set listener nonblocking");
            let mem0_addr = mem0_listener
                .local_addr()
                .expect("failed to read second listener addr");
            {
                let mut override_url = MEM0_SYNC_URL_OVERRIDE
                    .lock()
                    .expect("mem0 sync url override mutex poisoned");
                *override_url = Some(format!("http://{mem0_addr}/add"));
            }

            let count_only_mem0_server = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
                loop {
                    match mem0_listener.accept() {
                        Ok(_) => panic!("count-only re-solidify must not trigger Mem0 sync"),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if std::time::Instant::now() >= deadline {
                                return;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(e) => panic!("failed while polling Mem0 listener: {e}"),
                    }
                }
            });

            solidify_rule(&home, agent, category, 4).expect("count-only update should succeed");
            count_only_mem0_server
                .join()
                .expect("count-only Mem0 server panicked");

            let updated_rule: Rule = serde_json::from_str(
                &std::fs::read_to_string(&rule_path).expect("read updated rule"),
            )
            .expect("deserialize updated rule");
            assert_eq!(updated_rule.trigger_count, 4);
            assert_eq!(updated_rule.rule_text, initial_rule.rule_text);
            assert_eq!(
                std::fs::read_to_string(&agents_md_path).expect("read AGENTS.md after count-only"),
                agents_md_after_initial
            );
            assert_eq!(
                std::fs::read_to_string(&obsidian_md_path)
                    .expect("read Obsidian rule after count-only"),
                obsidian_after_initial
            );

            {
                let mut override_url = MEM0_SYNC_URL_OVERRIDE
                    .lock()
                    .expect("mem0 sync url override mutex poisoned");
                *override_url = None;
            }
            unsafe {
                match previous_obsidian_vault {
                    Some(value) => std::env::set_var("AGEND_OBSIDIAN_VAULT", value),
                    None => std::env::remove_var("AGEND_OBSIDIAN_VAULT"),
                }
            }
            std::fs::remove_dir_all(&home).ok();
        });
    }

    #[test]
    fn test_re_solidify_refreshes_rule_text_when_delta_reaches_threshold() {
        let home = tmp_home("re_solidify_refresh_test");
        let agent = "re-solidify-refresh-agent";
        let category = "lint_failure";
        let mistakes_dir = home.join("mistakes");
        std::fs::create_dir_all(&mistakes_dir).expect("failed to create mistakes dir");

        let base_time = chrono::Utc::now();
        let initial_reasons = [
            "Clippy warning on handler",
            "Lint failure in dispatch",
            "More clippy warnings",
        ];
        for (idx, reason) in initial_reasons.iter().enumerate() {
            let mistake = Mistake {
                id: format!("mock_mstk_{idx}"),
                task_id: None,
                agent_name: agent.to_string(),
                category: category.to_string(),
                rejection_reason: (*reason).to_string(),
                timestamp: (base_time - chrono::Duration::seconds(10 - idx as i64)).to_rfc3339(),
                corrected_at: Some(chrono::Utc::now().to_rfc3339()),
            };
            std::fs::write(
                mistakes_dir.join(format!("mock_mstk_{idx}.json")),
                serde_json::to_string(&mistake).expect("failed to serialize mock mistake"),
            )
            .expect("failed to write mock mistake");
        }

        solidify_rule(&home, agent, category, 3).expect("initial solidify should succeed");
        let rule_path = home
            .join("rules")
            .join(format!("rule_{agent}_{category}.json"));
        let initial_rule: Rule =
            serde_json::from_str(&std::fs::read_to_string(&rule_path).expect("read rule"))
                .expect("deserialize rule");

        let extra_reasons = [
            "New regression: await_holding_lock clippy error",
            "Another lint failure in reflexion tests",
            "cargo clippy still reports warnings",
        ];
        for (idx, reason) in extra_reasons.iter().enumerate() {
            let mistake = Mistake {
                id: format!("mock_mstk_extra_{idx}"),
                task_id: None,
                agent_name: agent.to_string(),
                category: category.to_string(),
                rejection_reason: (*reason).to_string(),
                timestamp: (base_time + chrono::Duration::seconds(idx as i64 + 1)).to_rfc3339(),
                corrected_at: Some(chrono::Utc::now().to_rfc3339()),
            };
            std::fs::write(
                mistakes_dir.join(format!("mock_mstk_extra_{idx}.json")),
                serde_json::to_string(&mistake).expect("failed to serialize extra mock mistake"),
            )
            .expect("failed to write extra mock mistake");
        }

        solidify_rule(&home, agent, category, 6).expect("re-solidify should succeed");
        let refreshed_rule: Rule = serde_json::from_str(
            &std::fs::read_to_string(&rule_path).expect("read refreshed rule"),
        )
        .expect("deserialize refreshed rule");

        assert_eq!(refreshed_rule.trigger_count, 6);
        assert_eq!(refreshed_rule.created_at, initial_rule.created_at);
        assert_ne!(refreshed_rule.rule_text, initial_rule.rule_text);
        assert!(refreshed_rule
            .rule_text
            .contains("await_holding_lock clippy error"));

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_inject_rule_to_obsidian() {
        let tmp = tmp_home("obsidian_inject_test");
        inject_rule_to_obsidian(
            &tmp,
            "test-agent",
            "lint_failure",
            "NEVER submit clippy warnings",
            3,
        );
        let md = tmp.join("Rules").join("test-agent_lint_failure.md");
        assert!(md.exists());
        let content = std::fs::read_to_string(md).expect("failed to read Obsidian rule file");
        assert!(content.contains("NEVER submit clippy warnings"));
        assert!(content.contains("agent: \"test-agent\""));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[serial_test::serial(mem0_sync)]
    #[test]
    fn test_mistakes_30_day_cutoff_and_90_day_cleanup() {
        let home = tmp_home("expiry_test");
        let agent = "test-agent-expiry";

        let worktree_dir = home.join("mock_worktree");
        std::fs::create_dir_all(&worktree_dir).expect("failed to create mock worktree");
        let binding_dir = home.join("runtime").join(agent);
        std::fs::create_dir_all(&binding_dir).expect("failed to create binding dir");
        let binding_json = json!({
            "worktree": worktree_dir.to_str().expect("invalid worktree path"),
            "branch": "feat/mock-branch-expiry"
        });
        std::fs::write(
            binding_dir.join("binding.json"),
            serde_json::to_string(&binding_json).expect("failed to serialize binding json"),
        )
        .expect("failed to write binding.json");

        let args = json!({
            "correlation_id": "task-expiry",
            "artifacts": "evidence of failure"
        });

        // 1. Record first mistake (now)
        let rule_id = record_mistake(
            &home,
            "general",
            agent,
            "REJECTED: clippy warnings",
            &args,
            Some("lint_failure"),
        );
        assert!(rule_id.is_none());

        // 2. Manually write a mistake from 35 days ago
        let old_mistake_id = "mstk_old_35_days";
        let old_timestamp = (chrono::Utc::now() - chrono::Duration::days(35)).to_rfc3339();
        let old_mistake = Mistake {
            id: old_mistake_id.to_string(),
            task_id: Some("task-expiry".to_string()),
            agent_name: agent.to_string(),
            category: "lint_failure".to_string(),
            rejection_reason: "old failure".to_string(),
            timestamp: old_timestamp,
            corrected_at: None,
        };
        let mistakes_dir = home.join("mistakes");
        std::fs::create_dir_all(&mistakes_dir).expect("create mistakes dir");
        std::fs::write(
            mistakes_dir.join(format!("{}.json", old_mistake_id)),
            serde_json::to_string_pretty(&old_mistake).expect("serialize old mistake"),
        )
        .expect("write old mistake");

        // 3. Record second mistake (now)
        let rule_id = record_mistake(
            &home,
            "general",
            agent,
            "REJECTED: clippy fails",
            &args,
            Some("lint_failure"),
        );
        assert!(
            rule_id.is_none(),
            "Count should be 2 within 30 days (excluding 35-day-old one)"
        );

        // 4. Record third mistake (now)
        mark_mistake_corrected(&home, agent, "lint_failure");

        let rule_id = record_mistake(
            &home,
            "general",
            agent,
            "REJECTED: cargo clippy failed",
            &args,
            Some("lint_failure"),
        );
        assert!(
            rule_id.is_some(),
            "Count should reach 3 within 30 days (excluding 35-day-old one)"
        );

        // 5. Manually write a mistake from 95 days ago
        let very_old_mistake_id = "mstk_very_old_95_days";
        let very_old_timestamp = (chrono::Utc::now() - chrono::Duration::days(95)).to_rfc3339();
        let very_old_mistake = Mistake {
            id: very_old_mistake_id.to_string(),
            task_id: Some("task-expiry".to_string()),
            agent_name: agent.to_string(),
            category: "lint_failure".to_string(),
            rejection_reason: "very old failure".to_string(),
            timestamp: very_old_timestamp,
            corrected_at: None,
        };
        let very_old_path = mistakes_dir.join(format!("{}.json", very_old_mistake_id));
        std::fs::write(
            &very_old_path,
            serde_json::to_string_pretty(&very_old_mistake).expect("serialize very old mistake"),
        )
        .expect("write very old mistake");

        assert!(very_old_path.exists());

        // 6. Call cleanup_old_mistakes
        cleanup_old_mistakes(&home);

        // 7. Verify very old mistake is deleted, but 35-day-old mistake remains
        assert!(
            !very_old_path.exists(),
            "95-day-old mistake should be cleaned up"
        );
        assert!(
            mistakes_dir
                .join(format!("{}.json", old_mistake_id))
                .exists(),
            "35-day-old mistake should NOT be cleaned up"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_disconfirming_gate_blocks_without_correction() {
        let home = tmp_home("gate_blocks_test");
        let agent = "test-agent-gate";
        let category = "lint_failure";

        let mistakes_dir = home.join("mistakes");
        std::fs::create_dir_all(&mistakes_dir).expect("failed to create mistakes dir");

        for i in 1..=3 {
            let m = Mistake {
                id: format!("mstk_{}", i),
                task_id: None,
                agent_name: agent.to_string(),
                category: category.to_string(),
                rejection_reason: "clippy warning".to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                corrected_at: None,
            };
            std::fs::write(
                mistakes_dir.join(format!("{}.json", m.id)),
                serde_json::to_string(&m).expect("failed to serialize mistake"),
            )
            .expect("failed to write mistake");
        }

        let rule_id = solidify_rule(&home, agent, category, 3);
        assert!(
            rule_id.is_none(),
            "Should be blocked because there is no correction"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[serial_test::serial(mem0_sync)]
    #[test]
    fn test_disconfirming_gate_allows_after_correction() {
        let home = tmp_home("gate_allows_test");
        let agent = "test-agent-gate";
        let category = "lint_failure";

        let mistakes_dir = home.join("mistakes");
        std::fs::create_dir_all(&mistakes_dir).expect("failed to create mistakes dir");

        for i in 1..=3 {
            let m = Mistake {
                id: format!("mstk_{}", i),
                task_id: None,
                agent_name: agent.to_string(),
                category: category.to_string(),
                rejection_reason: "clippy warning".to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                corrected_at: None,
            };
            std::fs::write(
                mistakes_dir.join(format!("{}.json", m.id)),
                serde_json::to_string(&m).expect("failed to serialize mistake"),
            )
            .expect("failed to write mistake");
        }

        mark_mistake_corrected(&home, agent, category);

        // Solidify should now succeed
        let rule_id = solidify_rule(&home, agent, category, 3);
        assert!(
            rule_id.is_some(),
            "Should solidify rule since mistake is corrected"
        );
        let rule_id_str = rule_id.expect("expected solidified rule");
        assert_eq!(rule_id_str, format!("rule_{}_{}", agent, category));

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_first_meaningful_line_skips_metadata_lines() {
        let text = "### Evidence\nREJECTED\nran: cargo test\ncited: mod.rs:10\nActual clippy warning on line 42";
        assert_eq!(
            first_meaningful_line(text),
            Some("Actual clippy warning on line 42")
        );
    }

    #[test]
    fn test_synthesize_rule_text_empty_mistakes_returns_base_rule() {
        let text = synthesize_rule_text("lint_failure", &[]);
        assert_eq!(text, get_rule_text("lint_failure"));
        assert!(!text.contains("Recurring failures:"));
    }

    #[test]
    fn test_synthesize_rule_text_with_mistakes_includes_recurring_failures() {
        let mistakes = vec![
            Mistake {
                id: "mstk_1".to_string(),
                task_id: None,
                agent_name: "agent".to_string(),
                category: "lint_failure".to_string(),
                rejection_reason: "REJECTED\nClippy reported unused variable".to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                corrected_at: None,
            },
            Mistake {
                id: "mstk_2".to_string(),
                task_id: None,
                agent_name: "agent".to_string(),
                category: "lint_failure".to_string(),
                rejection_reason: "Missing derive annotation on struct".to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                corrected_at: None,
            },
        ];
        let text = synthesize_rule_text("lint_failure", &mistakes);
        assert!(text.contains("Recurring failures:"));
        assert!(text.contains("- Clippy reported unused variable"));
        assert!(text.contains("- Missing derive annotation on struct"));
        assert!(text.starts_with(get_rule_text("lint_failure")));
    }

    #[test]
    fn test_cross_worker_shared_rules_propagation() {
        let home = tmp_home("cross_worker_rules_test");
        let rules_dir = home.join("rules");
        std::fs::create_dir_all(&rules_dir).expect("failed to create rules dir");

        let rule_a = Rule {
            id: "rule_agent-a_lint_failure".to_string(),
            agent_name: "Agent-A".to_string(),
            category: "lint_failure".to_string(),
            rule_text: "Don't forget tests".to_string(),
            created_at: "2026-06-26T12:00:00Z".to_string(),
            trigger_count: 3,
            synthesis_method: None,
        };

        // 1. Merge to shared rules
        merge_to_shared_rules(&rules_dir.join("shared.json"), &rule_a).expect("merge failed");

        // 2. list_rules for Agent-B (should return Agent-A's rule because it is shared)
        let rules_b = list_rules(&home, "Agent-B");
        assert_eq!(rules_b.len(), 1, "Agent-B should see the shared rule");
        assert_eq!(rules_b[0].id, rule_a.id);
        assert_eq!(rules_b[0].rule_text, rule_a.rule_text);

        // 3. Merge a new rule with higher trigger count and updated text for same category
        let rule_b = Rule {
            id: "rule_agent-b_lint_failure".to_string(),
            agent_name: "Agent-B".to_string(),
            category: "lint_failure".to_string(),
            rule_text: "Don't forget tests - clippy clean required".to_string(),
            created_at: "2026-06-26T13:00:00Z".to_string(),
            trigger_count: 5,
            synthesis_method: None,
        };
        merge_to_shared_rules(&rules_dir.join("shared.json"), &rule_b).expect("merge failed");

        // 4. list_rules for Agent-A (should see the updated shared rule)
        let rules_a = list_rules(&home, "Agent-A");
        assert_eq!(
            rules_a.len(),
            1,
            "Agent-A should see exactly one shared rule"
        );
        assert_eq!(rules_a[0].id, rule_b.id);
        assert_eq!(rules_a[0].rule_text, rule_b.rule_text);
        assert_eq!(rules_a[0].trigger_count, 5);

        // 5. list_cross_agent_rules for Agent-A should skip shared.json
        let cross_rules_a = list_cross_agent_rules(&home, "Agent-A");
        assert!(
            cross_rules_a.is_empty(),
            "cross_rules should be empty because we only have shared.json"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_auto_correct_on_ci_pass() {
        let home = tmp_home("auto_correct_ci_test");
        let agent = "test-agent-ci";
        let category = "test_failure";
        let mistakes_dir = home.join("mistakes");
        std::fs::create_dir_all(&mistakes_dir).expect("create mistakes dir");

        // Write a mistake
        let mistake = Mistake {
            id: "mstk_ci_1".to_string(),
            task_id: Some("t-ci-1".to_string()),
            agent_name: agent.to_string(),
            category: category.to_string(),
            rejection_reason: "Tests failed".to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            corrected_at: None,
        };
        std::fs::write(
            mistakes_dir.join(format!("{}.json", mistake.id)),
            serde_json::to_string(&mistake).expect("serialize mistake"),
        )
        .expect("write mistake");

        // Call auto_correct_on_ci_pass
        auto_correct_on_ci_pass(&home, agent, None);

        // Verify mistake is marked corrected
        let content = std::fs::read_to_string(mistakes_dir.join(format!("{}.json", mistake.id)))
            .expect("read mistake file");
        let updated: Mistake = serde_json::from_str(&content).expect("deserialize mistake");
        assert!(updated.corrected_at.is_some());

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_has_cargo_test_pass_evidence() {
        assert!(has_cargo_test_pass_evidence("ran: cargo test -> passed"));
        assert!(has_cargo_test_pass_evidence("ran: cargo test -> success"));
        assert!(has_cargo_test_pass_evidence(
            "test result: ok. 45 passed; 0 failed"
        ));
        assert!(!has_cargo_test_pass_evidence("ran: cargo test -> failed"));
        assert!(!has_cargo_test_pass_evidence(
            "test result: ok. 45 passed; 3 failed"
        ));
        assert!(!has_cargo_test_pass_evidence("just normal text"));
    }

    #[serial_test::serial(mem0_sync)]
    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn test_solidify_rule_llm_synthesis_success() {
        let home = tmp_home("solidify_llm_success");
        let agent = "llm-agent";
        let category = "missing_test_execution";

        // Setup mock response
        {
            let mut mock = CLAUDE_API_RESPONSE_MOCK.lock().unwrap();
            *mock = Some(Ok("ALWAYS run cargo test before submitting".to_string()));
        }
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test-key");

        // Write a success
        let successes_dir = home.join("successes");
        std::fs::create_dir_all(&successes_dir).unwrap();
        let success = Success {
            success_id: "s-1".to_string(),
            agent_name: agent.to_string(),
            category: category.to_string(),
            summary: "Ran cargo test successfully".to_string(),
            recorded_at: chrono::Utc::now().to_rfc3339(),
        };
        std::fs::write(
            successes_dir.join(format!("{agent}.json")),
            serde_json::to_string(&vec![success]).unwrap(),
        )
        .unwrap();

        // Write a mistake
        let mistakes_dir = home.join("mistakes");
        std::fs::create_dir_all(&mistakes_dir).unwrap();
        let mock_mistake = Mistake {
            id: "m-1".to_string(),
            task_id: None,
            agent_name: agent.to_string(),
            category: category.to_string(),
            rejection_reason: "cargo test was not run".to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            corrected_at: Some(chrono::Utc::now().to_rfc3339()),
        };
        std::fs::write(
            mistakes_dir.join("m-1.json"),
            serde_json::to_string(&mock_mistake).unwrap(),
        )
        .unwrap();

        let rule_id = solidify_rule(&home, agent, category, 3).expect("rule id");
        let rule_path = home.join("rules").join(format!("{}.json", rule_id));
        let content = std::fs::read_to_string(&rule_path).unwrap();
        let rule: Rule = serde_json::from_str(&content).unwrap();

        assert_eq!(rule.rule_text, "ALWAYS run cargo test before submitting");
        assert_eq!(
            rule.synthesis_method.as_deref(),
            Some(SYNTHESIS_METHOD_LLM)
        );

        // Clean up
        {
            let mut mock = CLAUDE_API_RESPONSE_MOCK.lock().unwrap();
            *mock = None;
        }
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::fs::remove_dir_all(&home).ok();
    }

    #[serial_test::serial(mem0_sync)]
    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn test_solidify_rule_llm_synthesis_fallback_on_api_error() {
        let home = tmp_home("solidify_llm_api_error");
        let agent = "llm-agent";
        let category = "missing_test_execution";

        // Setup mock error response
        {
            let mut mock = CLAUDE_API_RESPONSE_MOCK.lock().unwrap();
            *mock = Some(Err("Rate Limit Exceeded".to_string()));
        }
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test-key");

        // Write a mistake
        let mistakes_dir = home.join("mistakes");
        std::fs::create_dir_all(&mistakes_dir).unwrap();
        let mock_mistake = Mistake {
            id: "m-1".to_string(),
            task_id: None,
            agent_name: agent.to_string(),
            category: category.to_string(),
            rejection_reason: "cargo test was not run".to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            corrected_at: Some(chrono::Utc::now().to_rfc3339()),
        };
        std::fs::write(
            mistakes_dir.join("m-1.json"),
            serde_json::to_string(&mock_mistake).unwrap(),
        )
        .unwrap();

        let rule_id = solidify_rule(&home, agent, category, 3).expect("rule id");
        let rule_path = home.join("rules").join(format!("{}.json", rule_id));
        let content = std::fs::read_to_string(&rule_path).unwrap();
        let rule: Rule = serde_json::from_str(&content).unwrap();

        // Should fall back to the base rule text from get_rule_text + bullets
        assert!(rule
            .rule_text
            .contains("NEVER report VERIFIED without running cargo test"));
        assert!(rule.rule_text.contains("cargo test was not run"));
        assert_eq!(
            rule.synthesis_method.as_deref(),
            Some(SYNTHESIS_METHOD_TEMPLATE)
        );

        // Clean up
        {
            let mut mock = CLAUDE_API_RESPONSE_MOCK.lock().unwrap();
            *mock = None;
        }
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::fs::remove_dir_all(&home).ok();
    }

    #[serial_test::serial(mem0_sync)]
    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn test_solidify_rule_llm_synthesis_fallback_on_missing_api_key() {
        let home = tmp_home("solidify_llm_missing_key");
        let agent = "llm-agent";
        let category = "missing_test_execution";

        // Setup mock response (should NOT be called)
        {
            let mut mock = CLAUDE_API_RESPONSE_MOCK.lock().unwrap();
            *mock = Some(Ok("ALWAYS run cargo test before submitting".to_string()));
        }
        std::env::remove_var("ANTHROPIC_API_KEY");

        // Write a mistake
        let mistakes_dir = home.join("mistakes");
        std::fs::create_dir_all(&mistakes_dir).unwrap();
        let mock_mistake = Mistake {
            id: "m-1".to_string(),
            task_id: None,
            agent_name: agent.to_string(),
            category: category.to_string(),
            rejection_reason: "cargo test was not run".to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            corrected_at: Some(chrono::Utc::now().to_rfc3339()),
        };
        std::fs::write(
            mistakes_dir.join("m-1.json"),
            serde_json::to_string(&mock_mistake).unwrap(),
        )
        .unwrap();

        let rule_id = solidify_rule(&home, agent, category, 3).expect("rule id");
        let rule_path = home.join("rules").join(format!("{}.json", rule_id));
        let content = std::fs::read_to_string(&rule_path).unwrap();
        let rule: Rule = serde_json::from_str(&content).unwrap();

        // Should fall back to the base rule text from get_rule_text + bullets
        assert!(rule
            .rule_text
            .contains("NEVER report VERIFIED without running cargo test"));
        assert_eq!(
            rule.synthesis_method.as_deref(),
            Some(SYNTHESIS_METHOD_TEMPLATE)
        );

        // Clean up
        {
            let mut mock = CLAUDE_API_RESPONSE_MOCK.lock().unwrap();
            *mock = None;
        }
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_should_create_skill_triggers_on_complex_evidence() {
        let summary = "VERIFIED\n\n### Summary\nCompleted reflexion synthesis_method Layer 2 implementation with full test coverage.";
        let evidence = "### Evidence\n\
            ran: cargo test --bin agend-terminal -- reflexion → 42 passed\n\
            ran: cargo clippy --all-targets -- -D warnings → clean\n\
            cited: src/reflexion/mod.rs:155 — synthesis_method field\n\
            cited: src/reflexion/mod.rs:1038 — solidify_rule wiring";
        assert!(should_create_skill(
            summary,
            evidence,
            "missing_test_execution"
        ));
    }

    #[test]
    fn test_should_create_skill_skips_general_category() {
        let summary = "VERIFIED\n\n### Summary\nCompleted reflexion synthesis_method Layer 2 implementation with full test coverage.";
        let evidence = "### Evidence\n\
            ran: cargo test → ok\n\
            ran: cargo clippy → ok\n\
            cited: src/reflexion/mod.rs:1 — field\n\
            cited: src/reflexion/mod.rs:2 — wiring";
        assert!(!should_create_skill(summary, evidence, "general"));
        assert!(!should_create_skill(summary, evidence, "unclassified"));
    }

    #[test]
    fn test_should_create_skill_skips_simple_evidence() {
        let summary = "VERIFIED\n\n### Summary\nCompleted reflexion synthesis_method Layer 2 implementation with full test coverage.";
        let evidence = "### Evidence\n\
            ran: cargo test → ok\n\
            ran: cargo clippy → ok";
        assert!(!should_create_skill(
            summary,
            evidence,
            "missing_test_execution"
        ));
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn test_skill_file_written_to_auto_dir() {
        let home = tmp_home("skill_auto_dir_test");
        let auto_dir = home.join("skills_auto");
        std::env::set_var(
            "AGEND_SKILL_AUTO_DIR",
            auto_dir.to_str().expect("invalid auto dir path"),
        );
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test-key");

        let mock_skill = "---\n\
            name: missing_test_execution-skill-agent-procedure\n\
            description: Run cargo test before reporting VERIFIED\n\
            metadata:\n\
              category: missing_test_execution\n\
              agent: skill-agent\n\
              source: auto_reflexion\n\
            ---\n\n\
            # Verify Before Report\n";
        {
            let mut mock = CLAUDE_API_RESPONSE_MOCK.lock().unwrap();
            *mock = Some(Ok(mock_skill.to_string()));
        }

        let summary = "VERIFIED\n\n### Summary\nCompleted reflexion synthesis_method Layer 2 implementation with full test coverage.";
        let evidence = "### Evidence\n\
            ran: cargo test --bin agend-terminal -- reflexion → 42 passed\n\
            ran: cargo clippy --all-targets -- -D warnings → clean\n\
            cited: src/reflexion/mod.rs:155 — synthesis_method field\n\
            cited: src/reflexion/mod.rs:1038 — solidify_rule wiring";

        maybe_create_skill(
            "skill-agent",
            "missing_test_execution",
            summary,
            evidence,
            &home,
        );

        let date = chrono::Utc::now().format("%Y%m%d").to_string();
        let expected_path = auto_dir.join(format!("missing_test_execution_skill-agent_{date}.md"));
        assert!(
            expected_path.exists(),
            "expected skill file at {}",
            expected_path.display()
        );
        let content = std::fs::read_to_string(&expected_path).expect("read skill file");
        assert!(content.contains("name: missing_test_execution-skill-agent-procedure"));
        assert!(content.contains("# Verify Before Report"));

        {
            let mut mock = CLAUDE_API_RESPONSE_MOCK.lock().unwrap();
            *mock = None;
        }
        std::env::remove_var("AGEND_SKILL_AUTO_DIR");
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_rule_deserializes_without_synthesis_method() {
        let legacy_json = r#"{
            "rule_id": "rule_legacy-agent_lint_failure",
            "agent_name": "legacy-agent",
            "category": "lint_failure",
            "rule_text": "NEVER submit code with clippy warnings",
            "created_at": "2026-06-01T00:00:00Z",
            "trigger_count": 3
        }"#;
        let rule: Rule = serde_json::from_str(legacy_json).expect("legacy rule should parse");
        assert_eq!(rule.agent_name, "legacy-agent");
        assert!(rule.synthesis_method.is_none());
    }

    #[test]
    fn test_sweep_orphan_mistakes() {
        let home = tmp_home("sweep_orphan_mistakes_test");
        let mistakes_dir = home.join("mistakes");
        let rules_dir = home.join("rules");
        std::fs::create_dir_all(&mistakes_dir).expect("failed to create mistakes dir");
        std::fs::create_dir_all(&rules_dir).expect("failed to create rules dir");

        // Create fleet.yaml
        let fleet_yaml = r#"
instances:
  agy-worker-4:
    backend: agy
    id: 208500a4-df14-420a-8f14-4ff9fa66e7d0
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), fleet_yaml)
            .expect("failed to write fleet.yaml");

        // 1. Mistake for active agent (agy-worker-4), > 30 days old -> should NOT be orphaned
        let time_old = (chrono::Utc::now() - chrono::Duration::days(31)).to_rfc3339();
        let m_active = Mistake {
            id: "mstk_active".to_string(),
            task_id: None,
            agent_name: "agy-worker-4".to_string(),
            category: "lint_failure".to_string(),
            rejection_reason: "unused variable".to_string(),
            timestamp: time_old.clone(),
            corrected_at: None,
        };
        std::fs::write(
            mistakes_dir.join(format!("{}.json", m_active.id)),
            serde_json::to_string(&m_active).expect("failed to serialize active mistake"),
        )
        .expect("failed to write active mistake");

        // 2. Mistake for orphan agent (test-dynamic), > 30 days old -> should be orphaned
        let m_orphan_old = Mistake {
            id: "mstk_orphan_old".to_string(),
            task_id: None,
            agent_name: "test-dynamic".to_string(),
            category: "lint_failure".to_string(),
            rejection_reason: "clippy warning".to_string(),
            timestamp: time_old.clone(),
            corrected_at: None,
        };
        std::fs::write(
            mistakes_dir.join(format!("{}.json", m_orphan_old.id)),
            serde_json::to_string(&m_orphan_old).expect("failed to serialize old orphan mistake"),
        )
        .expect("failed to write old orphan mistake");

        // 3. Mistake for orphan agent (test-dynamic), < 30 days old -> should NOT be orphaned
        let time_new = (chrono::Utc::now() - chrono::Duration::days(10)).to_rfc3339();
        let m_orphan_new = Mistake {
            id: "mstk_orphan_new".to_string(),
            task_id: None,
            agent_name: "test-dynamic".to_string(),
            category: "lint_failure".to_string(),
            rejection_reason: "clippy warning 2".to_string(),
            timestamp: time_new,
            corrected_at: None,
        };
        std::fs::write(
            mistakes_dir.join(format!("{}.json", m_orphan_new.id)),
            serde_json::to_string(&m_orphan_new).expect("failed to serialize new orphan mistake"),
        )
        .expect("failed to write new orphan mistake");

        // 4. Solidified rule for orphan agent (test-dynamic)
        let rule_orphan = Rule {
            id: "rule_test-dynamic_lint_failure".to_string(),
            agent_name: "test-dynamic".to_string(),
            category: "lint_failure".to_string(),
            rule_text: "Clean compile required for test-dynamic".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            trigger_count: 3,
            synthesis_method: None,
        };
        std::fs::write(
            rules_dir.join(format!("{}.json", rule_orphan.id)),
            serde_json::to_string(&rule_orphan).expect("failed to serialize orphan rule"),
        )
        .expect("failed to write orphan rule");

        // Run sweep
        sweep_orphan_mistakes(&home);

        // Read and assert results
        let m_active_read: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(mistakes_dir.join(format!("{}.json", m_active.id)))
                .expect("failed to read active mistake"),
        )
        .expect("failed to parse active mistake");
        assert_eq!(m_active_read["orphaned"].as_bool(), None);

        let m_orphan_old_read: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(mistakes_dir.join(format!("{}.json", m_orphan_old.id)))
                .expect("failed to read old orphan mistake"),
        )
        .expect("failed to parse old orphan mistake");
        assert_eq!(m_orphan_old_read["orphaned"].as_bool(), Some(true));

        let m_orphan_new_read: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(mistakes_dir.join(format!("{}.json", m_orphan_new.id)))
                .expect("failed to read new orphan mistake"),
        )
        .expect("failed to parse new orphan mistake");
        assert_eq!(m_orphan_new_read["orphaned"].as_bool(), None);

        // Verify rule copied to shared.json
        let shared_path = rules_dir.join("shared.json");
        assert!(shared_path.exists());
        let shared_rules: Vec<Rule> = serde_json::from_str(
            &std::fs::read_to_string(&shared_path).expect("failed to read shared rules"),
        )
        .expect("failed to parse shared rules");
        assert_eq!(shared_rules.len(), 1);
        assert_eq!(shared_rules[0].agent_name, "test-dynamic");
        assert_eq!(shared_rules[0].id, rule_orphan.id);

        std::fs::remove_dir_all(&home).ok();
    }
}
