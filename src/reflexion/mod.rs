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
    static PR_REPO_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static EVIDENCE_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static SECRET_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static FIRE_AND_FORGET_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static BRANCH_BASE_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();

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
    let rule_id = if count >= 3 {
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
    if recent.len() < 3 {
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
}

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

    let rules_dir = home.join("rules");
    if let Err(e) = fs::create_dir_all(&rules_dir) {
        tracing::warn!(?e, "failed to create rules directory");
        return None;
    }

    let rule_id = format!("rule_{}_{}", agent_name, category);
    let rule_text = synthesize_rule_text(category, &recent_mistakes);
    let rule = Rule {
        id: rule_id.clone(),
        agent_name: agent_name.to_string(),
        category: category.to_string(),
        rule_text: rule_text.clone(),
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
    inject_rule_to_agents_md_for_binding(home, agent_name, category, &rule_text);

    spawn_mem0_sync(&rule);
    let vault = obsidian_vault_path();
    inject_rule_to_obsidian(&vault, agent_name, category, &rule_text, trigger_count);
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

    #[serial_test::serial(mem0_sync)]
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
    }

    #[serial_test::serial(mem0_sync)]
    #[test]
    fn test_spawn_mem0_sync_posts_without_existing_tokio_runtime() {
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
        assert_eq!(
            rule.rule_text,
            "NEVER open a PR to suzuke/agend-terminal; always use justdoit530-hub/agend-terminal"
        );

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
}
