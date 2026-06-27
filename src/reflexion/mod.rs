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
}

/// List all solidified rules for a specific agent.
pub fn list_rules(home: &Path, agent_name: &str) -> Vec<Rule> {
    let rules_dir = home.join("rules");
    let Ok(entries) = fs::read_dir(&rules_dir) else {
        return vec![];
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .filter_map(|content| serde_json::from_str::<Rule>(&content).ok())
        .filter(|rule| rule.agent_name == agent_name)
        .collect()
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
        regex::Regex::new(r"(?i)(suzuke/agend-terminal|upstream|base branch|suzuke)")
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

    None
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
        _ => "NEVER repeat this mistake category",
    }
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

    // Resolve the actual agent name from parent message sender if possible
    let mut real_agent_name = agent_name.to_string();
    if let Some(ref p_msg) = parent_msg {
        let from = p_msg.from.trim();
        if let Some(stripped) = from.strip_prefix("from:") {
            real_agent_name = stripped.to_string();
        } else {
            real_agent_name = from.to_string();
        }
    }

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
    };

    let filepath = mistakes_dir.join(format!("{}.json", mistake.id));
    if let Ok(serialized) = serde_json::to_string_pretty(&mistake) {
        if let Err(e) = fs::write(&filepath, serialized) {
            tracing::warn!(?e, ?filepath, "failed to write mistake file");
        }
    }

    // Count mistakes of same agent and category
    let mut count = 0;
    if let Ok(entries) = fs::read_dir(&mistakes_dir) {
        for entry in entries.filter_map(Result::ok) {
            if entry.path().extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    if let Ok(m) = serde_json::from_str::<Mistake>(&content) {
                        if m.agent_name == real_agent_name && m.category == category {
                            count += 1;
                        }
                    }
                }
            }
        }
    }

    // Solidify rule if threshold reached
    if count >= 3 {
        return solidify_rule(home, &real_agent_name, &category, count);
    }

    None
}

/// Persist a rule after repeated mistakes, inject it into AGENTS.md, and sync it to Mem0.
pub fn solidify_rule(
    home: &Path,
    agent_name: &str,
    category: &str,
    trigger_count: usize,
) -> Option<String> {
    let rules_dir = home.join("rules");
    if let Err(e) = fs::create_dir_all(&rules_dir) {
        tracing::warn!(?e, "failed to create rules directory");
        return None;
    }

    let rule_id = format!("rule_{}_{}", agent_name, category);
    let rule_text = get_rule_text(category);
    let rule = Rule {
        id: rule_id.clone(),
        agent_name: agent_name.to_string(),
        category: category.to_string(),
        rule_text: rule_text.to_string(),
        trigger_count,
        created_at: chrono::Utc::now().to_rfc3339(),
    };

    let rule_path = rules_dir.join(format!("{}.json", rule_id));
    if let Ok(serialized) = serde_json::to_string_pretty(&rule) {
        if let Err(e) = fs::write(&rule_path, serialized) {
            tracing::warn!(?e, ?rule_path, "failed to write rule file");
        }
    }

    // Inject rule into agent's .agents/AGENTS.md
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
            }
        }
    }

    spawn_mem0_sync(&rule);
    Some(rule_id)
}

fn spawn_mem0_sync(rule: &Rule) {
    if tokio::runtime::Handle::try_current().is_err() {
        tracing::warn!("Mem0 sync skipped: no Tokio runtime available");
        return;
    }

    let rule_text = rule.rule_text.clone();
    let agent_name = rule.agent_name.clone();
    let category = rule.category.clone();
    let count = rule.trigger_count;
    let url = mem0_sync_url();
    // fire-and-forget: sync rule to Mem0, non-critical
    tokio::spawn(async move {
        let body = mem0_sync_body(&agent_name, &rule_text, &category, count);
        if let Err(e) = reqwest::Client::new()
            .post(url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            tracing::warn!("Mem0 sync failed (non-fatal): {}", e);
        }
    });
}

fn mem0_sync_body(
    agent_name: &str,
    rule_text: &str,
    category: &str,
    trigger_count: usize,
) -> serde_json::Value {
    serde_json::json!({
        "content": format!(
            "Agent {} 的規則：{}（來源：Reflexion Loop，分類：{}，觸發次數：{}）",
            agent_name, rule_text, category, trigger_count
        ),
        "user_id": "reflexion"
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
        let rejection1 = "Do not target suzuke/agend-terminal.";
        assert_eq!(
            classify_mistake(rejection1, None),
            Some("wrong_branch_target")
        );

        let rejection2 = "PR base is suzuke/agend-terminal upstream instead of fork";
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
        assert_eq!(
            rule.rule_text,
            "NEVER report VERIFIED without running cargo test"
        );
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

    #[tokio::test]
    async fn test_solidify_triggers_mem0_sync() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
        let rule_id = solidify_rule(&home, "test-agent", "lint_failure", 4)
            .expect("expected rule to solidify");
        assert_eq!(rule_id, "rule_test-agent_lint_failure");

        let request = tokio::time::timeout(std::time::Duration::from_secs(5), request)
            .await
            .expect("timed out waiting for Mem0 sync request")
            .expect("request task failed");

        assert!(request.starts_with("POST /add HTTP/1.1"));
        assert!(request.contains("\"user_id\":\"reflexion\""));
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
        };
        let rule_b = Rule {
            id: "rule_agent_b_cat".to_string(),
            agent_name: "Agent-B".to_string(),
            category: "lint_failure".to_string(),
            rule_text: "No lint warnings".to_string(),
            created_at: "2026-06-26T12:05:00Z".to_string(),
            trigger_count: 4,
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
        assert_eq!(
            rule.rule_text,
            "NEVER open a PR to suzuke/agend-terminal; always use justdoit530-hub/agend-terminal"
        );

        std::fs::remove_dir_all(&home).ok();
    }
}
