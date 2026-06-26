use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::fs;
use crate::mcp::handlers::comms_gates::{detect_verdict, Verdict};

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
    pub id: String,
    pub agent_name: String,
    pub category: String,
    pub rule_text: String,
    pub created_at: String,
}

/// Classify a mistake using regex matching on the rejection text and parent message.
pub fn classify_mistake(rejection_text: &str, parent_text: Option<&str>) -> Option<&'static str> {
    // 1. missing_test_execution
    // Check if the parent report has a verdict of VERIFIED or REJECTED but didn't run cargo test.
    if let Some(p_text) = parent_text {
        if let Some(verdict) = detect_verdict(p_text) {
            if (verdict == Verdict::Verified || verdict == Verdict::Rejected) && !p_text.contains("cargo test") {
                return Some("missing_test_execution");
            }
        }
    }
    // Fallback regex matching for test run missing in the rejection text
    let test_re = regex::Regex::new(r"(?i)(cargo test|test suite|unit test)").unwrap();
    let missing_re = regex::Regex::new(r"(?i)(missing|omit|forgot|no |not run|failed to)").unwrap();
    if test_re.is_match(rejection_text) && missing_re.is_match(rejection_text) {
        return Some("missing_test_execution");
    }

    // 2. wrong_branch_target
    // PR base is suzuke/agend-terminal upstream instead of fork
    let branch_re = regex::Regex::new(r"(?i)(suzuke/agend-terminal|upstream|base branch|suzuke)").unwrap();
    if branch_re.is_match(rejection_text) {
        return Some("wrong_branch_target");
    }

    // 3. lint_failure
    // Rejection reason contains clippy/lint warnings
    let lint_re = regex::Regex::new(r"(?i)(clippy|lint|warnings|cargo clippy)").unwrap();
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
        _ => "NEVER repeat this mistake category",
    }
}

/// Main entry point to record a mistake, check threshold, and inject rule if needed.
pub fn record_mistake(
    home: &Path,
    sender: &str,
    target: &str,
    summary: &str,
    args: &Value,
) -> Option<String> {
    let _ = sender; // Unused but kept for signature
    let parent_id = args["parent_id"].as_str();
    let parent_text = parent_id.and_then(|pid| crate::inbox::find_message(home, pid)).map(|m| m.text);
    
    let rejection_text = format!("{}\n{}", summary, args["artifacts"].as_str().unwrap_or(""));
    let category = classify_mistake(&rejection_text, parent_text.as_deref())?;

    let mistakes_dir = home.join("mistakes");
    if let Err(e) = fs::create_dir_all(&mistakes_dir) {
        tracing::warn!(?e, "failed to create mistakes directory");
        return None;
    }

    let mistake_id = format!("mstk_{}_{}", chrono::Utc::now().timestamp_millis(), uuid::Uuid::new_v4().simple());
    let mistake = Mistake {
        id: mistake_id.clone(),
        task_id: args["correlation_id"].as_str().map(str::to_string),
        agent_name: target.to_string(),
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
                        if m.agent_name == target && m.category == category {
                            count += 1;
                        }
                    }
                }
            }
        }
    }

    // Solidify rule if threshold reached
    if count >= 3 {
        let rules_dir = home.join("rules");
        if let Err(e) = fs::create_dir_all(&rules_dir) {
            tracing::warn!(?e, "failed to create rules directory");
            return None;
        }

        let rule_id = format!("rule_{}_{}", target, category);
        let rule_text = get_rule_text(category);
        let rule = Rule {
            id: rule_id.clone(),
            agent_name: target.to_string(),
            category: category.to_string(),
            rule_text: rule_text.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
        };

        let rule_path = rules_dir.join(format!("{}.json", rule_id));
        if let Ok(serialized) = serde_json::to_string_pretty(&rule) {
            if let Err(e) = fs::write(&rule_path, serialized) {
                tracing::warn!(?e, ?rule_path, "failed to write rule file");
            }
        }

        // Inject rule into agent's .agents/AGENTS.md
        if let Some(binding) = crate::binding::read(home, target) {
            if let Some(worktree_path) = binding["worktree"].as_str() {
                let agents_md_path = Path::new(worktree_path).join(".agents").join("AGENTS.md");
                if let Err(e) = inject_rule_to_agents_md(&agents_md_path, category, rule_text) {
                    tracing::warn!(?e, ?agents_md_path, "failed to inject rule to AGENTS.md");
                } else {
                    tracing::info!(?agents_md_path, category, "solidified rule injected to AGENTS.md");
                }
            }
        }

        return Some(rule_id);
    }

    None
}

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

    if let (Some(start_idx), Some(end_idx)) = (content.find(start_marker), content.find(end_marker)) {
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
            content = format!("{}{}{}{}{}", before, start_marker, new_inner, end_marker, after);
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
        assert_eq!(classify_mistake(rejection, Some(parent)), Some("missing_test_execution"));

        let parent2 = "REJECTED\nEvidence:\nran: cargo check\ncited: mod.rs:110";
        assert_eq!(classify_mistake(rejection, Some(parent2)), Some("missing_test_execution"));

        let parent3 = "VERIFIED\nEvidence:\nran: cargo test\ncited: mod.rs:110";
        assert_ne!(classify_mistake(rejection, Some(parent3)), Some("missing_test_execution"));

        let rejection2 = "You forgot to run cargo test.";
        assert_eq!(classify_mistake(rejection2, None), Some("missing_test_execution"));
    }

    #[test]
    fn test_classify_mistake_wrong_branch_target() {
        let rejection1 = "Do not target suzuke/agend-terminal.";
        assert_eq!(classify_mistake(rejection1, None), Some("wrong_branch_target"));

        let rejection2 = "PR base is suzuke/agend-terminal upstream instead of fork";
        assert_eq!(classify_mistake(rejection2, None), Some("wrong_branch_target"));
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
        std::fs::create_dir_all(&worktree_dir).unwrap();
        let binding_dir = home.join("runtime").join(agent);
        std::fs::create_dir_all(&binding_dir).unwrap();
        let binding_json = json!({
            "worktree": worktree_dir.to_str().unwrap(),
            "branch": "feat/mock-branch"
        });
        std::fs::write(binding_dir.join("binding.json"), serde_json::to_string(&binding_json).unwrap()).unwrap();

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
        std::fs::create_dir_all(&inbox_dir).unwrap();
        let inbox_file = inbox_dir.join(format!("{}.jsonl", agent));
        std::fs::write(&inbox_file, format!("{}\n", serde_json::to_string(&parent_msg).unwrap())).unwrap();

        let mistakes_dir = home.join("mistakes");
        assert!(!mistakes_dir.exists());

        let args = json!({
            "parent_id": parent_id,
            "correlation_id": "task-abc",
            "artifacts": "evidence of failure"
        });
        let rule_id = record_mistake(&home, "general", agent, "REJECTED: no cargo test executed", &args);
        assert!(rule_id.is_none(), "Threshold not reached yet");

        let rule_id = record_mistake(&home, "general", agent, "REJECTED: missing test run", &args);
        assert!(rule_id.is_none(), "Threshold not reached yet");

        let rule_id = record_mistake(&home, "general", agent, "REJECTED: did not run cargo test", &args);
        assert!(rule_id.is_some(), "Threshold reached; rule should be solidified");
        let rule_id_str = rule_id.unwrap();
        assert_eq!(rule_id_str, format!("rule_{}_missing_test_execution", agent));

        let files = std::fs::read_dir(&mistakes_dir).unwrap();
        assert_eq!(files.count(), 3);

        let rule_path = home.join("rules").join(format!("{}.json", rule_id_str));
        assert!(rule_path.exists());
        let rule_content = std::fs::read_to_string(&rule_path).unwrap();
        let rule: Rule = serde_json::from_str(&rule_content).unwrap();
        assert_eq!(rule.rule_text, "NEVER report VERIFIED without running cargo test");

        let agents_md_path = worktree_dir.join(".agents").join("AGENTS.md");
        assert!(agents_md_path.exists());
        let agents_md_content = std::fs::read_to_string(&agents_md_path).unwrap();
        assert!(agents_md_content.contains("<!-- agend-rules:start -->"));
        assert!(agents_md_content.contains("NEVER report VERIFIED without running cargo test"));
        assert!(agents_md_content.contains("<!-- agend-rules:end -->"));

        std::fs::remove_dir_all(&home).ok();
    }
}
