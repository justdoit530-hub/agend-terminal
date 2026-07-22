//! #817 daemon-side stale local branch cleanup.
//!
//! Operator-triggered hygiene sweep that categorizes local branches
//! into 4 buckets (`clean_merged`, `squash_merged`, `stale_idle`,
//! `active_unknown`) and offers a dry-run + confirm-subset workflow
//! to delete only the proven-safe ones. Mirrors the `tasks::sweep_impl` pattern
//! from #806 — same `dry-run + confirm_ids + system identity +
//! audit_reason` shape — but operates on local git refs instead of
//! the task board.
//!
//! Local Git provides the primary evidence; a configured GitHub remote is
//! queried only for the apply-time open-PR preservation gate. Cache layer
//! (in-memory `HashMap` per sweep) dedups repeated cherry calls when branches
//! share ancestry.
//!
//! Safety stack (mirrors #806 + force-delete-specific layers):
//! - `system:branch_sweep` identity (allow-list at tasks.rs:485)
//! - dry-run default; apply requires explicit `apply=true`
//! - `confirm_ids` MUST be a subset of the current dry-run inventory; only
//!   proven terminal IDs can pass the apply classifier
//! - `audit_reason` required, non-empty
//! - `active_unknown` bucket is always preserved, even when an operator
//!   includes its ID in `confirm_ids`; it remains visible for follow-up
//! - `event_log.jsonl` records `branch=<name> source=<sha>` so an
//!   operator can `git branch <name> <sha>` to restore

use std::path::Path;

/// Threshold for `stale_idle` category. Branches whose tip commit
/// committer-date is older than this AND not merged AND not squash-
/// merged land in `stale_idle`. Operator can override via
/// `min_age_days` arg on the MCP call. Dead-code allow lifts at C3
/// when the MCP handler reads the default.
pub(crate) const STALE_IDLE_DEFAULT_DAYS: i64 = 90;

/// Lightweight enumeration of a local branch — what `git for-each-ref`
/// returns. The category is computed separately via per-branch
/// `git cherry` / `git branch --merged` checks.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct BranchInfo {
    pub name: String,
    pub tip_sha: String,
    /// RFC3339 committer date of the branch tip.
    pub committer_date: String,
}

/// Categorization bucket. Each non-terminal local branch lands in
/// exactly one bucket (first match wins, order: clean_merged →
/// squash_merged → stale_idle → active_unknown).
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Candidate {
    pub name: String,
    pub tip_sha: String,
    pub reason: String,
}

#[derive(Debug, Default, serde::Serialize)]
pub(crate) struct Categories {
    pub clean_merged: Vec<Candidate>,
    pub squash_merged: Vec<Candidate>,
    pub stale_idle: Vec<Candidate>,
    pub active_unknown: Vec<Candidate>,
    /// #852 PR-C: reviewer-checkout residue. Naming patterns
    /// `tmp.*` / `pr\d+_head` / `review/.*` that historically
    /// accumulated when reviewer agents `cd canonical && git
    /// checkout <sha>` (the bug PR-A documented and PR-B
    /// enforced at the shim). These branches have no legitimate
    /// purpose and land in the default delete list — but the
    /// daemon boot sweep is dry-run-only for r0 so operator can
    /// validate the regex against their real residue before any
    /// destructive action.
    pub reviewer_checkout: Vec<Candidate>,
}

impl Categories {
    /// Concatenated sorted list of all candidate branch names across
    /// the deletable buckets (clean_merged + squash_merged +
    /// stale_idle + #852 PR-C reviewer_checkout). `active_unknown` is
    /// NOT in this default list — the operator must explicitly pick
    /// those IDs by their bucket.
    pub fn deletable_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .clean_merged
            .iter()
            .chain(self.squash_merged.iter())
            .chain(self.stale_idle.iter())
            .chain(self.reviewer_checkout.iter())
            .map(|c| c.name.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Total IDs including the non-deletable `active_unknown` bucket. The
    /// handler uses this inventory to reject stale IDs while keeping unknown
    /// branches visible; the lifecycle classifier still preserves them.
    pub fn all_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .clean_merged
            .iter()
            .chain(self.squash_merged.iter())
            .chain(self.stale_idle.iter())
            .chain(self.reviewer_checkout.iter())
            .chain(self.active_unknown.iter())
            .map(|c| c.name.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn total(&self) -> usize {
        self.all_ids().len()
    }
}

/// Enumerate local branches via `git for-each-ref`, parsing name +
/// tip SHA + ISO-8601 committerdate per line.
fn enumerate_branches(repo: &Path) -> Result<Vec<BranchInfo>, String> {
    // W1.2: git_cmd = always-bypass + bounded + trimmed stdout; its GitError
    // covers both the spawn-fail and non-zero-exit branches this used to handle
    // separately (same semantics, more structured message).
    let stdout = crate::git_helpers::git_cmd(
        repo,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)|%(objectname)|%(committerdate:iso8601-strict)",
            "refs/heads/",
        ],
    )
    .map_err(|e| format!("git for-each-ref: {e}"))?;
    let branches: Vec<BranchInfo> = stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '|');
            let name = parts.next()?.trim().to_string();
            let tip_sha = parts.next()?.trim().to_string();
            let committer_date = parts.next()?.trim().to_string();
            if name.is_empty() || tip_sha.is_empty() {
                return None;
            }
            Some(BranchInfo {
                name,
                tip_sha,
                committer_date,
            })
        })
        .collect();
    Ok(branches)
}

/// Returns true if `branch` is reachable from `base` via a merge
/// commit (`git branch --merged base` includes it). Used to detect
/// the `clean_merged` category.
fn is_clean_merged(repo: &Path, base: &str, branch: &str) -> bool {
    // W1.2: git_cmd → trimmed stdout on success; both the spawn-error and
    // non-zero-exit `return false` branches collapse to the `Err → false` arm.
    let Ok(stdout) = crate::git_helpers::git_cmd(repo, &["branch", "--merged", base]) else {
        return false;
    };
    stdout
        .lines()
        .map(|line| {
            line.trim_start_matches(|ch| {
                // `git branch` prefixes the current checkout with `*`; the
                // fleet-managed git shim additionally uses `+` for a branch held by
                // another worktree. Both prefixes still identify the branch name.
                ch == '*' || ch == '+'
            })
            .trim()
        })
        .any(|line| line == branch)
}

/// Returns true if every commit on `branch` is already applied to
/// `base` as an equivalent patch (squash-merged). `git cherry base
/// branch` output prefix per commit: `-` means present in base, `+`
/// means missing. All-`-` (and at least one line) ⇒ squash-merged.
///
/// #1280: Falls back to tree-diff comparison when `git cherry` misses
/// GitHub-style squash merges (single squashed commit has a different
/// patch-id than the individual commits). The fallback checks if the
/// diff from merge-base to the branch tip is empty against base HEAD
/// (i.e., all changes are already incorporated).
// #1750-B3: pub(crate) so the automatic per-tick GC
// (`worktree_cleanup::prune_orphaned_branches`) reuses the SAME squash-merge
// detection the operator-triggered sweep uses — the squash-blind `git branch
// --merged` in the auto path missed 95/99 squash-orphan branches.
pub(crate) fn is_squash_merged(repo: &Path, base: &str, branch: &str) -> bool {
    // Method 1: git cherry (works for cherry-picked commits).
    if is_squash_merged_cherry(repo, base, branch) {
        return true;
    }
    // Method 2: tree-diff comparison (works for GitHub squash-merge).
    is_squash_merged_diff(repo, base, branch)
}

/// `git cherry` based detection.
fn is_squash_merged_cherry(repo: &Path, base: &str, branch: &str) -> bool {
    // W1.2: git_cmd → trimmed stdout on success; spawn-error + non-zero-exit
    // both collapse to the `Err → false` arm.
    let Ok(stdout) = crate::git_helpers::git_cmd(repo, &["cherry", base, branch]) else {
        return false;
    };
    let mut had_any = false;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        had_any = true;
        if !trimmed.starts_with('-') {
            return false;
        }
    }
    had_any
}

/// Tri-state result of the PR-based (authoritative) merge check. `Unknown`
/// means the check could NOT run — no github remote, `extract_github_repo`
/// returned `None`, the tip couldn't be resolved, or the `gh`/scm call errored
/// — as distinct from `NotMerged` (the check ran and found no matching merged
/// PR). #P3 (branch-residue): callers that treat a merged PR as monotonic proof
/// (delete NOW, no age gate) act ONLY on `Merged`; `Unknown` fails CLOSED
/// (treated as not-merged) everywhere, so a gh outage never reaps a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrMergeStatus {
    Merged,
    NotMerged,
    Unknown,
}

/// Tri-state open-PR probe used by branch lifecycle retirement. A repository
/// without a GitHub remote has no open-PR surface to query (`NotOpen`), while
/// a GitHub/SCM lookup failure is `Unknown` and must fail closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenPrStatus {
    Open,
    NotOpen,
    Unknown,
}

/// One bounded open-PR inventory for a repository/sweep.  The daemon cleanup
/// path must not issue one SCM lookup per branch; a failed inventory remains
/// `Unknown` for every affected branch so the lifecycle classifier preserves
/// them all.
#[derive(Debug, Clone, Default)]
pub(crate) struct OpenPrSnapshot {
    open_branches: Option<std::collections::HashSet<String>>,
}

const OPEN_PR_SNAPSHOT_CAP: usize = 1000;

impl OpenPrSnapshot {
    pub(crate) fn status_for(&self, branch: &str) -> OpenPrStatus {
        match &self.open_branches {
            Some(open) if open.contains(branch) => OpenPrStatus::Open,
            Some(_) => OpenPrStatus::NotOpen,
            None => OpenPrStatus::Unknown,
        }
    }
}

/// Gather the repository's open-PR inventory once.  `headRefName` is the only
/// field needed by the lifecycle gate; the explicit limit keeps this external
/// call bounded even for a repository with a large review backlog.
pub(crate) fn open_pr_snapshot(repo: &Path, _base: &str) -> OpenPrSnapshot {
    let remote_url = match crate::git_helpers::git_cmd(repo, &["remote", "get-url", "origin"]) {
        Ok(url) => url,
        Err(crate::git_helpers::GitError::NonZero { stderr, .. })
            if stderr.contains("No such remote") =>
        {
            return OpenPrSnapshot {
                open_branches: Some(std::collections::HashSet::new()),
            };
        }
        Err(_) => return OpenPrSnapshot::default(),
    };
    let Some(gh_repo) = extract_github_repo(&remote_url) else {
        return OpenPrSnapshot::default();
    };
    let Ok(prs) = crate::scm::make_scm_provider(&gh_repo, None).pr_list(
        &gh_repo,
        &crate::scm::ListFilter {
            state: Some("open"),
            // Open PRs targeting any base protect the branch. Request one
            // beyond the bounded inventory so truncation is distinguishable
            // from a complete result and remains fail-closed.
            base: None,
            limit: Some((OPEN_PR_SNAPSHOT_CAP + 1) as u32),
            ..Default::default()
        },
        &["headRefName"],
        None,
    ) else {
        return OpenPrSnapshot::default();
    };
    if prs.len() > OPEN_PR_SNAPSHOT_CAP {
        return OpenPrSnapshot::default();
    }
    let mut open_branches = std::collections::HashSet::new();
    for pr in prs {
        let Some(branch) = pr.head_ref else {
            // A malformed/partial provider response is not proof that the
            // branch has no open PR; preserve all candidates on this snapshot.
            return OpenPrSnapshot::default();
        };
        open_branches.insert(branch);
    }
    OpenPrSnapshot {
        open_branches: Some(open_branches),
    }
}

/// Resolve whether `branch` still has an open PR. The lifecycle classifier
/// owns the fail direction; this helper only gathers the SCM evidence.
pub(crate) fn open_pr_status(repo: &Path, _base: &str, branch: &str) -> OpenPrStatus {
    let remote_url = match crate::git_helpers::git_cmd(repo, &["remote", "get-url", "origin"]) {
        Ok(url) => url,
        Err(crate::git_helpers::GitError::NonZero { stderr, .. })
            if stderr.contains("No such remote") =>
        {
            // No remote is a deterministic local-fixture/no-PR state, not a
            // transient SCM outage.
            return OpenPrStatus::NotOpen;
        }
        Err(_) => return OpenPrStatus::Unknown,
    };
    let Some(gh_repo) = extract_github_repo(&remote_url) else {
        // A configured non-GitHub remote is an unresolved SCM surface, not
        // evidence that the branch has no open review. Keep the lifecycle
        // decision fail-closed until a provider can answer it.
        return OpenPrStatus::Unknown;
    };
    let Ok(prs) = crate::scm::make_scm_provider(&gh_repo, None).pr_list(
        &gh_repo,
        &crate::scm::ListFilter {
            state: Some("open"),
            head: Some(branch.to_string()),
            // A branch remains protected by an open PR regardless of target
            // base; do not narrow this apply-time probe to the repository's
            // default branch.
            base: None,
            ..Default::default()
        },
        &["number"],
        None,
    ) else {
        return OpenPrStatus::Unknown;
    };
    if prs.is_empty() {
        OpenPrStatus::NotOpen
    } else {
        OpenPrStatus::Open
    }
}

pub(crate) fn pr_merge_status(repo: &Path, base: &str, branch: &str) -> PrMergeStatus {
    // Resolve owner/repo from git remote origin.
    // W1.2 class-2: git_cmd always adds AGEND_GIT_BYPASS + trims stdout (this
    // site previously ran raw `git` — the forgot-bypass latent class #821/#1463).
    let Ok(remote_url) = crate::git_helpers::git_cmd(repo, &["remote", "get-url", "origin"]) else {
        return PrMergeStatus::Unknown;
    };
    let Some(gh_repo) = extract_github_repo(&remote_url) else {
        return PrMergeStatus::Unknown;
    };
    // Get local branch tip SHA.
    let Ok(local_sha) = crate::git_helpers::git_cmd(repo, &["rev-parse", branch]) else {
        return PrMergeStatus::Unknown;
    };
    // #PR-D: `gh pr list` via ScmProvider. argv is set-equal to the prior
    // inline `pr list --state merged --head B --base BASE --repo R --json
    // headRefOid` — flag ORDER is canonicalized (gh order-insensitive) per
    // decision d-20260601151209762922-0; same flags+values. Uses --repo
    // (gh_repo derived above), no cwd. A gh/scm error → `Unknown` (fail-closed).
    let Ok(prs) = crate::scm::make_scm_provider(&gh_repo, None).pr_list(
        &gh_repo,
        &crate::scm::ListFilter {
            state: Some("merged"),
            head: Some(branch.to_string()),
            base: Some(base.to_string()),
            ..Default::default()
        },
        &["headRefOid"],
        None,
    ) else {
        return PrMergeStatus::Unknown;
    };
    // Merged iff any merged PR's HEAD SHA matches the local branch tip, or the
    // local tip is a strict ancestor of that HEAD SHA — see
    // `local_sha_matches_merged_head` for why the ancestor case matters.
    let merged = prs.iter().any(|s| {
        s.head_ref_oid
            .as_deref()
            .is_some_and(|oid| local_sha_matches_merged_head(repo, &local_sha, oid))
    });
    if merged {
        PrMergeStatus::Merged
    } else {
        PrMergeStatus::NotMerged
    }
}

/// Method-2 wrapper for [`is_squash_merged`]: `Merged → true`, else false.
/// `Unknown` maps to NOT squash-merged — byte-identical to the pre-#P3
/// `is_squash_merged_diff` (every non-`Merged` outcome was already `false`).
fn is_squash_merged_diff(repo: &Path, base: &str, branch: &str) -> bool {
    matches!(pr_merge_status(repo, base, branch), PrMergeStatus::Merged)
}

/// True iff `head_ref_oid` (a merged PR's recorded HEAD SHA) equals
/// `local_sha`, or `local_sha` is a strict ancestor of it.
fn local_sha_matches_merged_head(repo: &Path, local_sha: &str, head_ref_oid: &str) -> bool {
    head_ref_oid == local_sha
        || crate::git_helpers::git_ok(
            repo,
            &["merge-base", "--is-ancestor", local_sha, head_ref_oid],
        )
}

#[allow(dead_code)] // wired in upstream #2807 but intentionally unconnected here
pub(crate) fn extract_github_repo_for_intent(url: &str) -> Option<String> {
    extract_github_repo(url)
}

/// Return the PR number of a merged PR whose head matches the local branch tip.
/// Used by cleanup intent sweep to independently verify PR generation.
pub(crate) fn merged_pr_number(repo: &Path, base: &str, branch: &str) -> Option<u64> {
    let remote_url = crate::git_helpers::git_cmd(repo, &["remote", "get-url", "origin"]).ok()?;
    let gh_repo = extract_github_repo(&remote_url)?;
    let local_sha = crate::git_helpers::git_cmd(repo, &["rev-parse", branch]).ok()?;
    let prs = crate::scm::make_scm_provider(&gh_repo, None)
        .pr_list(
            &gh_repo,
            &crate::scm::ListFilter {
                state: Some("merged"),
                head: Some(branch.to_string()),
                base: Some(base.to_string()),
                ..Default::default()
            },
            &["headRefOid", "number"],
            None,
        )
        .ok()?;
    prs.iter()
        .find(|pr| {
            pr.head_ref_oid
                .as_deref()
                .is_some_and(|oid| local_sha_matches_merged_head(repo, &local_sha, oid))
        })
        .map(|pr| pr.number)
}

/// Extract "owner/repo" from a GitHub remote URL.
fn extract_github_repo(url: &str) -> Option<String> {
    // Handles: https://github.com/owner/repo.git, git@github.com:owner/repo.git
    let stripped = url.trim().trim_end_matches('/').trim_end_matches(".git");
    if stripped.contains("github.com") {
        if let Some(path) = stripped.strip_prefix("git@github.com:") {
            return Some(path.to_string());
        }
        // https://github.com/owner/repo
        if let Some(idx) = stripped.find("github.com/") {
            return Some(stripped[idx + "github.com/".len()..].to_string());
        }
    }
    None
}

/// #817 scan local branches and categorize into the 4 buckets.
/// `now` parameterized so `stale_idle` threshold testing isn't
/// flaky around day boundaries. Dead-code allow lifts at C3 when
/// the MCP handler wires the call site.
/// #852 PR-C: classify reviewer-checkout residue by name. Pattern
/// covers the three observed pollution shapes:
/// - `tmp.*` — operator's `tmp_pr_review` / `tmp/abc1234` style
/// - `pr\d+_head` — `gh pr fetch`-style `pr123_head` refs
/// - `review/.*` — explicit `review/<n>` namespace
///
/// First-match wins. Conservative — empty / `main` / `master` /
/// genuine branch prefixes never match. Uses an inline anchored
/// regex (`^` anchor explicit, full-string `is_match` semantics on
/// the regex crate) so prefix-match-only is the contract.
pub(crate) fn is_reviewer_checkout(name: &str) -> bool {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    // SAFETY: regex literal is compile-time-validated by the test
    // suite (the pattern's anchor + alternations are exercised by
    // the four `reviewer_checkout_pattern_*` unit tests). `.unwrap`
    // here is the established crate convention for build-time
    // patterns (see `state.rs::StatePatterns::for_backend`).
    #[allow(clippy::unwrap_used)]
    let re = RE.get_or_init(|| regex::Regex::new(r"^(tmp.*|pr\d+_head|review/.*)$").unwrap());
    re.is_match(name)
}

pub(crate) fn scan(
    repo: &Path,
    base: &str,
    min_age_days: i64,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Categories, String> {
    let branches = enumerate_branches(repo)?;
    let mut cats = Categories::default();
    for b in &branches {
        if b.name == base {
            continue;
        }
        // 0. reviewer_checkout (#852 PR-C) — naming-pattern residue.
        // Checked FIRST so reviewer-pollution branches that happen to
        // also satisfy clean_merged / squash_merged conditions still
        // surface in the dedicated bucket (operator can audit them
        // separately from the regular merge-based categories).
        if is_reviewer_checkout(&b.name) {
            cats.reviewer_checkout.push(Candidate {
                name: b.name.clone(),
                tip_sha: b.tip_sha.clone(),
                reason: "reviewer-checkout residue (tmp.* / pr*_head / review/*)".to_string(),
            });
            continue;
        }
        // 1. clean_merged — reachable from base via merge commit.
        if is_clean_merged(repo, base, &b.name) {
            cats.clean_merged.push(Candidate {
                name: b.name.clone(),
                tip_sha: b.tip_sha.clone(),
                reason: format!("merged into {base}"),
            });
            continue;
        }
        // 2. squash_merged — all commits already in base by patch-id.
        if is_squash_merged(repo, base, &b.name) {
            cats.squash_merged.push(Candidate {
                name: b.name.clone(),
                tip_sha: b.tip_sha.clone(),
                reason: format!("all commits squash-applied to {base}"),
            });
            continue;
        }
        // 3. stale_idle — committer date older than threshold.
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&b.committer_date) {
            let age = now.signed_duration_since(dt.with_timezone(&chrono::Utc));
            if age > chrono::Duration::days(min_age_days) {
                cats.stale_idle.push(Candidate {
                    name: b.name.clone(),
                    tip_sha: b.tip_sha.clone(),
                    reason: format!("idle {}d (>{min_age_days}d threshold)", age.num_days()),
                });
                continue;
            }
        }
        // 4. active_unknown — residual.
        cats.active_unknown.push(Candidate {
            name: b.name.clone(),
            tip_sha: b.tip_sha.clone(),
            reason: "unmerged + not squash-applied + within freshness window".to_string(),
        });
    }
    Ok(cats)
}

/// Apply phase — `git branch -D <name>` for each confirm_id under
/// the `system:branch_sweep` identity. Each deletion records a
/// `branch_sweep_apply` entry to `event-log.jsonl` with the source
/// SHA so an operator can `git branch <name> <sha>` to restore.
///
/// Returns the count of successfully deleted branches. A per-branch
/// failure logs the error but does not abort the batch — partial
/// success is observable in the event log.
///
/// Dead-code allow lifts at C3 when the MCP handler wires the call.
#[allow(dead_code)]
pub(crate) fn emit_delete_batch(
    home: &Path,
    repo: &Path,
    categories: &Categories,
    confirm_ids: &std::collections::HashSet<String>,
    audit_reason: &str,
) -> Result<usize, String> {
    emit_delete_batch_with_context(
        Some(home),
        repo,
        "main",
        categories,
        confirm_ids,
        audit_reason,
    )
    .map(|(count, _)| count)
}

/// Apply a branch-sweep confirmation with lifecycle evidence. The legacy
/// wrapper above keeps the existing unit-test seam; production MCP callers
/// pass the explicit base and home so active holders, tasks, and PR state are
/// checked before any branch mutation.
pub(crate) fn emit_delete_batch_with_context(
    home: Option<&Path>,
    repo: &Path,
    base: &str,
    categories: &Categories,
    confirm_ids: &std::collections::HashSet<String>,
    audit_reason: &str,
) -> Result<(usize, Vec<serde_json::Value>), String> {
    // #2011: prune orphaned worktree REGISTRATIONS first, in the same
    // transaction as the branch deletions. A worktree whose physical
    // directory is gone (crashed release, manual rm, pre-prune-era leak)
    // keeps its branch "checked out" in git's eyes → `branch -D` refuses →
    // branches pile up forever (live: 14 stale branches behind 9 prunable
    // registrations, 2026-06-11). Prune is idempotent and cheap; doing it
    // HERE — rather than only at each deletion site — closes the gap
    // regardless of which path leaked the registration (chokepoint
    // principle). Best-effort: a prune failure just leaves the per-branch
    // refusal behavior unchanged (logged below as before).
    if let Err(e) = crate::git_helpers::git_bypass(repo, &["worktree", "prune"]) {
        tracing::warn!(error = %e, "#2011: git worktree prune before branch sweep failed (non-fatal)");
    }
    let mut name_to_candidate: std::collections::HashMap<&str, &Candidate> =
        std::collections::HashMap::new();
    for cand in categories
        .clean_merged
        .iter()
        .chain(categories.squash_merged.iter())
        .chain(categories.stale_idle.iter())
        .chain(categories.active_unknown.iter())
    {
        name_to_candidate.insert(cand.name.as_str(), cand);
    }
    let category_of = |name: &str| -> &'static str {
        if categories.clean_merged.iter().any(|c| c.name == name) {
            "clean_merged"
        } else if categories.squash_merged.iter().any(|c| c.name == name) {
            "squash_merged"
        } else if categories.stale_idle.iter().any(|c| c.name == name) {
            "stale_idle"
        } else {
            "active_unknown"
        }
    };
    let mut deleted = 0usize;
    let mut skipped: Vec<serde_json::Value> = Vec::new();
    for name in confirm_ids {
        let Some(cand) = name_to_candidate.get(name.as_str()) else {
            continue;
        };
        let is_reviewer = categories.reviewer_checkout.iter().any(|c| c.name == *name);
        let provenance = if is_reviewer {
            crate::worktree::disposition::BranchProvenance::ReviewerResidue
        } else if categories.clean_merged.iter().any(|c| c.name == *name) {
            crate::worktree::disposition::BranchProvenance::Merged
        } else if categories.squash_merged.iter().any(|c| c.name == *name) {
            crate::worktree::disposition::BranchProvenance::SquashMerged
        } else {
            // `active_unknown` has no terminal provenance and therefore
            // remains fail-closed in the shared classifier.
            crate::worktree::disposition::BranchProvenance::Unknown
        };
        let binding_active =
            home.and_then(|h| crate::worktree_cleanup::branch_has_active_binding(h, repo, name));
        let active_holder = match (branch_is_checked_out(repo, name), binding_active) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        };
        let task_active = home.and_then(|h| branch_has_active_task(h, name));
        let terminal = !matches!(
            provenance,
            crate::worktree::disposition::BranchProvenance::Unknown
        );
        let open_pr = if terminal {
            match open_pr_status(repo, base, name) {
                OpenPrStatus::Open => Some(true),
                OpenPrStatus::NotOpen => Some(false),
                OpenPrStatus::Unknown => None,
            }
        } else {
            // Unknown provenance is already a KEEP decision; do not probe an
            // external SCM surface for a branch that cannot be deleted.
            Some(false)
        };
        // Reviewer residue is only deleted after a recovery ref is prepared
        // below; all other proven terminal categories are already preserved
        // by their merge/squash provenance.
        let unique_unpreserved_work = Some(false);
        let lifecycle = crate::worktree::disposition::branch_lifecycle_disposition(
            &crate::worktree::disposition::BranchLifecycleInput {
                provenance,
                terminal,
                active_holder,
                task_active,
                open_pr,
                unique_unpreserved_work,
            },
        );
        if !matches!(
            lifecycle,
            crate::worktree::disposition::BranchLifecycleDisposition::Delete
        ) {
            let blocker = first_lifecycle_blocker(
                terminal,
                active_holder,
                task_active,
                open_pr,
                unique_unpreserved_work,
                provenance,
            );
            crate::event_log::log(
                home.unwrap_or(repo),
                "branch_sweep_apply_skipped",
                "system:branch_sweep",
                &format!("branch={name} blocker={blocker}"),
            );
            skipped.push(serde_json::json!({"branch": name, "blocker": blocker}));
            continue;
        }
        let recovery_ref = if is_reviewer {
            Some(prepare_branch_recovery(
                home,
                repo,
                name,
                &cand.tip_sha,
                audit_reason,
            )?)
        } else {
            None
        };
        let _ = recovery_ref;
        // W1.2: git_cmd's GitError preserves the two distinct failure logs this
        // site emits — NonZero carries the trimmed stderr, Spawn carries the io error.
        match crate::git_helpers::git_cmd(repo, &["branch", "-D", name]) {
            Ok(_) => {
                deleted += 1;
                let category = category_of(name);
                crate::event_log::log(
                    home.unwrap_or(repo),
                    "branch_sweep_apply",
                    "system:branch_sweep",
                    &format!(
                        "branch={name} category={category} sha={tip} reason={audit_reason} \
                         restore_hint=`git branch {name} {tip}`",
                        tip = cand.tip_sha
                    ),
                );
            }
            Err(crate::git_helpers::GitError::NonZero { stderr, .. }) => {
                crate::event_log::log(
                    home.unwrap_or(repo),
                    "branch_sweep_apply_failed",
                    "system:branch_sweep",
                    &format!("branch={name} stderr={stderr}"),
                );
            }
            Err(crate::git_helpers::GitError::Spawn(e)) => {
                crate::event_log::log(
                    home.unwrap_or(repo),
                    "branch_sweep_apply_failed",
                    "system:branch_sweep",
                    &format!("branch={name} spawn_error={e}"),
                );
            }
        }
    }
    Ok((deleted, skipped))
}

fn first_lifecycle_blocker(
    terminal: bool,
    active_holder: Option<bool>,
    task_active: Option<bool>,
    open_pr: Option<bool>,
    unique_unpreserved_work: Option<bool>,
    provenance: crate::worktree::disposition::BranchProvenance,
) -> &'static str {
    if !terminal {
        return "non_terminal";
    }
    if active_holder != Some(false) {
        return if active_holder == Some(true) {
            "active_holder"
        } else {
            "active_holder_unknown"
        };
    }
    if task_active != Some(false) {
        return if task_active == Some(true) {
            "task_active"
        } else {
            "task_active_unknown"
        };
    }
    if open_pr != Some(false) {
        return if open_pr == Some(true) {
            "open_pr"
        } else {
            "open_pr_status_unknown"
        };
    }
    if unique_unpreserved_work != Some(false) {
        return if unique_unpreserved_work == Some(true) {
            "unique_unpreserved_work"
        } else {
            "unique_unpreserved_work_unknown"
        };
    }
    if matches!(
        provenance,
        crate::worktree::disposition::BranchProvenance::Unknown
    ) {
        return "provenance_unknown";
    }
    "unknown"
}

fn branch_is_checked_out(repo: &Path, branch: &str) -> Option<bool> {
    let out = crate::git_helpers::git_cmd(repo, &["worktree", "list", "--porcelain"]).ok()?;
    let mut live_worktree = false;
    let mut prunable = false;
    for line in out.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            live_worktree = Path::new(path.trim()).exists();
            prunable = false;
            continue;
        }
        if line.starts_with("prunable ") {
            prunable = true;
            live_worktree = false;
            continue;
        }
        if line
            .strip_prefix("branch refs/heads/")
            .is_some_and(|name| name.trim() == branch)
            && live_worktree
            && !prunable
        {
            return Some(true);
        }
    }
    Some(false)
}

pub(crate) fn branch_has_active_task(home: &Path, branch: &str) -> Option<bool> {
    let tasks = crate::tasks::list_all_strict(home).ok()?;
    Some(tasks.iter().any(|task| {
        task.branch.as_deref() == Some(branch)
            && !matches!(
                task.status,
                crate::task_events::TaskStatus::Done
                    | crate::task_events::TaskStatus::Cancelled
                    | crate::task_events::TaskStatus::Verified
            )
    }))
}

/// Create a durable recovery ref for a reviewer residue before deleting its
/// branch. The source SHA is the CAS identity; the returned ref is the
/// operator-visible recovery/audit identity.
pub(crate) fn prepare_branch_recovery(
    home: Option<&Path>,
    repo: &Path,
    branch: &str,
    tip_sha: &str,
    reason: &str,
) -> Result<String, String> {
    if tip_sha.is_empty() {
        return Err(format!("branch '{branch}' has no source SHA; preserved"));
    }
    let safe_branch: String = branch
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let identity = format!(
        "refs/agend/recovery/branch/{safe_branch}/{}-{}",
        &tip_sha[..tip_sha.len().min(12)],
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
    );
    let out = crate::git_helpers::git_bypass(repo, &["update-ref", &identity, tip_sha])
        .map_err(|e| format!("prepare recovery ref for '{branch}' failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "prepare recovery ref for '{branch}' failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    if let Some(home) = home {
        crate::event_log::log(
            home,
            "branch_cleanup_prepared",
            "system:branch_lifecycle",
            &format!(
                "repo={} branch={branch} source_sha={tip_sha} recovery_ref={identity} reason={reason}",
                repo.display()
            ),
        );
    }
    Ok(identity)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, dead_code)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ── #852 PR-C — reviewer_checkout pattern unit tests ──────────────

    /// `tmp_pr_review` / `tmp/abc1234` / `tmp-merge` — the operator-
    /// created scratch branches that show up after `cd canonical &&
    /// git checkout -b tmp_<...>`. Must classify as reviewer_checkout.
    #[test]
    fn reviewer_checkout_pattern_matches_tmp_prefix() {
        assert!(
            is_reviewer_checkout("tmp_pr_review"),
            "`tmp_pr_review` must match"
        );
        assert!(
            is_reviewer_checkout("tmp/abc1234"),
            "`tmp/abc1234` must match (slash-separated tmp branch)"
        );
        assert!(
            is_reviewer_checkout("tmp-merge"),
            "`tmp-merge` must match (hyphen variant)"
        );
        assert!(is_reviewer_checkout("tmp"), "bare `tmp` must match");
    }

    /// `pr<N>_head` — the `gh pr fetch` / manual `git fetch origin
    /// refs/pull/<N>/head:pr<N>_head` style. Common operator-typed
    /// pattern when inspecting a PR locally. Must classify as
    /// reviewer_checkout.
    #[test]
    fn reviewer_checkout_pattern_matches_pr_head_suffix() {
        assert!(
            is_reviewer_checkout("pr123_head"),
            "`pr123_head` must match"
        );
        assert!(
            is_reviewer_checkout("pr850_head"),
            "`pr850_head` must match (real example from operator's report)"
        );
        assert!(
            is_reviewer_checkout("pr1_head"),
            "single-digit pr1_head must match"
        );
    }

    /// `review/.*` — explicit `review/<n>` namespace. Some workflows
    /// adopt this prefix for inspection refs.
    #[test]
    fn reviewer_checkout_pattern_matches_review_prefix() {
        assert!(
            is_reviewer_checkout("review/123"),
            "`review/123` must match"
        );
        assert!(
            is_reviewer_checkout("review/feat-x"),
            "`review/feat-x` must match"
        );
    }

    /// **CRITICAL** negative: legitimate working branch names must NOT
    /// match. The pattern is narrow by design — only the three
    /// observed pollution shapes. A false-positive here would have
    /// the boot sweeper auto-deleting legitimate work.
    #[test]
    fn reviewer_checkout_pattern_does_not_match_main_or_fix_branches() {
        assert!(!is_reviewer_checkout("main"), "main must NOT match");
        assert!(!is_reviewer_checkout("master"), "master must NOT match");
        assert!(
            !is_reviewer_checkout("fix/123-real-work"),
            "fix/.* (legitimate fix branch) must NOT match"
        );
        assert!(
            !is_reviewer_checkout("feat/some-feature"),
            "feat/.* must NOT match"
        );
        assert!(
            !is_reviewer_checkout("temporary-work"),
            "`temporary-work` must NOT match — only `tmp.*` (3-letter \
             prefix) qualifies, not arbitrary 'temp' variants"
        );
        assert!(
            !is_reviewer_checkout("pr-merge-queue"),
            "`pr-merge-queue` must NOT match — pattern requires \
             `pr\\d+_head` shape specifically"
        );
        assert!(
            !is_reviewer_checkout(""),
            "empty string must NOT match (defensive)"
        );
    }

    /// Spawn a temp git repo scoped to `tag`. The repo has an initial
    /// commit on `main` + pinned per-repo gitconfig (`user.name`/
    /// `user.email`) so subsequent git subprocess calls don't fail
    /// with "unable to auto-detect email address" under CI runners
    /// that lack a global ~/.gitconfig. Mirrors #814 r1's CI
    /// portability fix.
    ///
    /// Returns the repo dir path.
    pub(super) fn setup_repo(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("agend-817-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&base).ok();
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).ok();
        git_run(&repo, &["init", "-b", "main"]);
        git_run(&repo, &["config", "user.name", "test"]);
        git_run(&repo, &["config", "user.email", "t@t"]);
        git_run(&repo, &["commit", "--allow-empty", "-m", "main: initial"]);
        repo
    }

    /// Run git with predictable env. `GIT_AUTHOR_DATE` /
    /// `GIT_COMMITTER_DATE` callers use `git_run_dated` instead.
    pub(super) fn git_run(dir: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git ran")
    }

    /// Run git with explicit author + committer date for back-dating
    /// commits. Used by stale_idle tests to plant commits N days in
    /// the past without `chrono::Utc::now() - duration` arithmetic
    /// (flaky near day boundaries).
    pub(super) fn git_run_dated(
        dir: &Path,
        args: &[&str],
        date_rfc3339: &str,
    ) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_AUTHOR_DATE", date_rfc3339)
            .env("GIT_COMMITTER_DATE", date_rfc3339)
            .output()
            .expect("git ran")
    }

    /// Helper: create a branch off main with one commit. Returns the
    /// branch tip SHA.
    pub(super) fn create_branch_with_commit(repo: &Path, branch: &str, commit_msg: &str) -> String {
        git_run(repo, &["checkout", "-b", branch]);
        let file = repo.join(format!("{branch}.txt"));
        std::fs::write(&file, format!("content for {branch}\n")).expect("write");
        git_run(repo, &["add", &format!("{branch}.txt")]);
        git_run(repo, &["commit", "-m", commit_msg]);
        let sha = String::from_utf8_lossy(&git_run(repo, &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();
        git_run(repo, &["checkout", "main"]);
        sha
    }

    fn bind_handler_repo(home: &Path, repo: &Path, agent: &str) {
        let binding_dir = home.join("runtime").join(agent);
        std::fs::create_dir_all(&binding_dir).expect("mkdir binding");
        std::fs::write(
            binding_dir.join("binding.json"),
            serde_json::json!({
                "source_repo": repo.display().to_string(),
                "branch": "feature",
                "worktree": repo.display().to_string(),
            })
            .to_string(),
        )
        .expect("write binding");
    }

    fn handler_dry_run(home: &Path, repo: &Path, agent: &str) -> serde_json::Value {
        bind_handler_repo(home, repo, agent);
        crate::mcp::handlers::ci::handle_cleanup_merged_branches(
            home,
            &serde_json::json!({"instance": agent}),
            agent,
        )
    }

    fn reviewer_candidate<'a>(
        response: &'a serde_json::Value,
        name: &str,
    ) -> &'a serde_json::Value {
        response["categories"]["reviewer_checkout"]
            .as_array()
            .expect("reviewer_checkout array")
            .iter()
            .find(|candidate| candidate["name"] == name)
            .unwrap_or_else(|| panic!("missing reviewer candidate {name}: {response}"))
    }

    fn add_local_bare_origin(repo: &Path) -> PathBuf {
        let origin = repo.parent().expect("repo parent").join("origin.git");
        git_run(
            repo,
            &["init", "--bare", origin.to_str().expect("origin path")],
        );
        git_run(
            repo,
            &[
                "remote",
                "add",
                "origin",
                origin.to_str().expect("origin path"),
            ],
        );
        origin
    }

    #[test]
    fn test_branch_sweep_scan_categorizes_clean_merged() {
        // #817 RED 1: branch "feat-a" merged into main via a merge
        // commit lands in `clean_merged` (git branch --merged main
        // includes it). Stub returns empty Categories → assertion
        // fails. C2 lands the real scan that picks it up.
        let repo = setup_repo("clean_merged");
        create_branch_with_commit(&repo, "feat-a", "feat: a");
        // Merge via a no-fast-forward merge so a merge commit exists.
        git_run(&repo, &["merge", "--no-ff", "-m", "merge feat-a", "feat-a"]);
        // Branch still exists locally after merge.

        let now = chrono::Utc::now();
        let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
        assert!(
            cats.clean_merged.iter().any(|c| c.name == "feat-a"),
            "clean_merged must include feat-a, got: {cats:?}"
        );
        // Not in other buckets.
        assert!(!cats.squash_merged.iter().any(|c| c.name == "feat-a"));
        assert!(!cats.stale_idle.iter().any(|c| c.name == "feat-a"));
        assert!(!cats.active_unknown.iter().any(|c| c.name == "feat-a"));

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn test_branch_sweep_scan_categorizes_squash_merged() {
        // #817 RED 2: branch "feat-b" whose commit was squash-applied
        // to main as a NEW commit with same patch-id but DIFFERENT
        // SHA (mirrors GitHub's "Squash and merge" semantics). The
        // detector must use `git cherry main feat-b` (patch-id based)
        // — `git branch --merged` would miss this case because the
        // feat-b SHA isn't reachable from main HEAD.
        //
        // To simulate the SHA-divergence: main advances by an
        // unrelated commit FIRST, then we cherry-pick feat-b with
        // `--no-commit` + commit with a different message. The
        // resulting main HEAD has feat-b's patch but a fresh SHA.
        let repo = setup_repo("squash_merged");
        create_branch_with_commit(&repo, "feat-b", "feat: b body");
        // Make main diverge first so cherry-pick doesn't fast-forward.
        std::fs::write(repo.join("unrelated.txt"), "main moves\n").expect("write");
        git_run(&repo, &["add", "unrelated.txt"]);
        git_run(&repo, &["commit", "-m", "main: unrelated work"]);
        // Squash-apply feat-b's diff to main as a separate commit.
        git_run(&repo, &["cherry-pick", "--no-commit", "feat-b"]);
        git_run(&repo, &["commit", "-m", "squash: feat-b body"]);

        let now = chrono::Utc::now();
        let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
        assert!(
            cats.squash_merged.iter().any(|c| c.name == "feat-b"),
            "squash_merged must include feat-b, got: {cats:?}"
        );
        // Not in clean_merged — feat-b's SHA is NOT in main's
        // ancestry post-squash (main has a different SHA with same
        // patch-id).
        assert!(!cats.clean_merged.iter().any(|c| c.name == "feat-b"));

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn test_branch_sweep_scan_categorizes_stale_idle() {
        // #817 RED 3: branch "old-wip" with committer-date 100 days
        // in the past, not merged, not squash-merged → stale_idle.
        // Uses GIT_AUTHOR_DATE/COMMITTER_DATE env to back-date the
        // commit (NOT chrono arithmetic — flaky near day boundary).
        let repo = setup_repo("stale_idle");
        // Back-date by 100 days from a fixed reference point.
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-15T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let old_date = (now - chrono::Duration::days(100)).to_rfc3339();
        git_run(&repo, &["checkout", "-b", "old-wip"]);
        std::fs::write(repo.join("wip.txt"), "wip content\n").expect("write");
        git_run(&repo, &["add", "wip.txt"]);
        git_run_dated(&repo, &["commit", "-m", "WIP: stale work"], &old_date);
        git_run(&repo, &["checkout", "main"]);

        let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
        assert!(
            cats.stale_idle.iter().any(|c| c.name == "old-wip"),
            "stale_idle must include old-wip (100d > 90d threshold), got: {cats:?}"
        );
        // NOT merged + NOT squash-merged.
        assert!(!cats.clean_merged.iter().any(|c| c.name == "old-wip"));
        assert!(!cats.squash_merged.iter().any(|c| c.name == "old-wip"));

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    // ── #817 apply-path tests ──

    /// #2011 regression: a branch checked out in a worktree whose physical
    /// directory is GONE (crashed release / manual rm / pre-prune-era leak)
    /// must still be deletable by the sweep — git counts it "checked out"
    /// until the registration is pruned, and 14 such branches piled up live
    /// on 2026-06-11. emit_delete_batch now prunes orphaned registrations in
    /// the same transaction (delete dir → registration goes → branch
    /// deletable). Pre-#2011 this test fails: `branch -D` refuses with
    /// "checked out at".
    #[test]
    fn test_orphaned_worktree_registration_does_not_block_delete_2011() {
        let repo = setup_repo("orphan_wt_reg");
        let home = repo.parent().unwrap().to_path_buf();
        create_branch_with_commit(&repo, "feat-orphan", "feat: orphan");
        let _merge = git_run(
            &repo,
            &["merge", "--no-ff", "-m", "merge feat-orphan", "feat-orphan"],
        );
        // Check the branch out in a worktree, then vaporize ONLY the
        // physical directory — the registration survives (the leak shape).
        let wt_dir = repo.parent().unwrap().join("orphan-wt-dir");
        std::fs::remove_dir_all(&wt_dir).ok(); // stale residue from a prior run
        let wt_str = wt_dir.display().to_string();
        let out = git_run(&repo, &["worktree", "add", &wt_str, "feat-orphan"]);
        assert!(
            out.status.success(),
            "worktree add must succeed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::remove_dir_all(&wt_dir).expect("rm worktree dir");
        // Precondition pin: the registration is still there (prunable).
        let list = git_run(&repo, &["worktree", "list", "--porcelain"]);
        assert!(
            String::from_utf8_lossy(&list.stdout).contains("orphan-wt-dir"),
            "leak shape precondition: registration must survive the rm"
        );

        let now = chrono::Utc::now();
        let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
        let mut confirm = std::collections::HashSet::new();
        confirm.insert("feat-orphan".to_string());
        let applied = emit_delete_batch(&home, &repo, &cats, &confirm, "#2011 test").expect("emit");
        assert_eq!(
            applied, 1,
            "orphaned registration must not block the branch delete"
        );
        let post = enumerate_branches(&repo).expect("enumerate");
        assert!(
            !post.iter().any(|b| b.name == "feat-orphan"),
            "feat-orphan must be deleted after the in-transaction prune"
        );
    }

    #[test]
    fn test_branch_sweep_apply_deletes_confirmed_subset() {
        // GREEN: emit_delete_batch runs `git branch -D <name>` for
        // each confirm_id and writes a `branch_sweep_apply` event-log
        // entry per success. Confirms double-opt-in actually deletes
        // the named branches AND records source SHA for restore.
        let repo = setup_repo("apply_subset");
        let home = repo.parent().unwrap().to_path_buf();
        // Create two clean-merged branches; only delete the first.
        create_branch_with_commit(&repo, "feat-keep", "feat: keep");
        git_run(
            &repo,
            &["merge", "--no-ff", "-m", "merge feat-keep", "feat-keep"],
        );
        create_branch_with_commit(&repo, "feat-delete", "feat: delete");
        git_run(
            &repo,
            &["merge", "--no-ff", "-m", "merge feat-delete", "feat-delete"],
        );

        let now = chrono::Utc::now();
        let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
        assert_eq!(
            cats.clean_merged.len(),
            2,
            "two branches expected: {cats:?}"
        );

        let mut confirm = std::collections::HashSet::new();
        confirm.insert("feat-delete".to_string());

        let applied =
            emit_delete_batch(&home, &repo, &cats, &confirm, "post-#817 test apply").expect("emit");
        assert_eq!(applied, 1, "exactly 1 deletion expected");

        // feat-delete is gone; feat-keep still exists.
        let post = enumerate_branches(&repo).expect("enumerate");
        let names: Vec<&str> = post.iter().map(|b| b.name.as_str()).collect();
        assert!(!names.contains(&"feat-delete"), "feat-delete must be gone");
        assert!(names.contains(&"feat-keep"), "feat-keep must remain");

        // Event-log entry per success.
        let log_path = home.join("event-log.jsonl");
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(
            log.contains("branch_sweep_apply"),
            "event-log must record branch_sweep_apply, got: {log}"
        );
        assert!(
            log.contains("feat-delete"),
            "event-log must name the deleted branch"
        );
        assert!(
            log.contains("post-#817 test apply"),
            "event-log must carry the audit_reason"
        );

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn branch_recovery_ref_records_source_sha_and_audit_identity() {
        let repo = setup_repo("branch_recovery_metadata");
        let home = repo.parent().unwrap().to_path_buf();
        git_run(&repo, &["checkout", "-b", "review/123"]);
        std::fs::write(repo.join("residue.txt"), "review residue\n").expect("write");
        git_run(&repo, &["add", "residue.txt"]);
        git_run(&repo, &["commit", "-m", "review residue"]);
        let tip = String::from_utf8_lossy(&git_run(&repo, &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();
        git_run(&repo, &["checkout", "main"]);

        let recovery_ref = prepare_branch_recovery(
            Some(&home),
            &repo,
            "review/123",
            &tip,
            "recovery metadata test",
        )
        .expect("recovery ref must be created");
        let resolved =
            String::from_utf8_lossy(&git_run(&repo, &["rev-parse", &recovery_ref]).stdout)
                .trim()
                .to_string();
        assert_eq!(resolved, tip, "recovery ref must preserve the source SHA");
        assert!(
            recovery_ref.starts_with("refs/agend/recovery/branch/review_123/"),
            "recovery identity must include the sanitized branch: {recovery_ref}"
        );
        let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(
            log.contains("branch_cleanup_prepared"),
            "audit event missing: {log}"
        );
        assert!(
            log.contains(&format!("source_sha={tip}")),
            "source SHA missing: {log}"
        );
        assert!(
            log.contains(&format!("recovery_ref={recovery_ref}")),
            "recovery identity missing: {log}"
        );

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn test_branch_sweep_apply_skips_unknown_confirm_id() {
        // GREEN: emit_delete_batch tolerates confirm_ids that aren't
        // in any category (e.g. operator typo). Skips silently (the
        // handler-level validator rejects these BEFORE calling this
        // function, so emit_delete_batch's contract is "do best-effort
        // for the candidates it recognizes"). Returns 0 deletions.
        let repo = setup_repo("apply_skip_unknown");
        let home = repo.parent().unwrap().to_path_buf();
        let cats = Categories::default(); // empty
        let mut confirm = std::collections::HashSet::new();
        confirm.insert("nonexistent-branch".to_string());
        let applied =
            emit_delete_batch(&home, &repo, &cats, &confirm, "unknown probe").expect("emit");
        assert_eq!(
            applied, 0,
            "unknown confirm_ids yield 0 deletions, not errors"
        );
        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn test_branch_sweep_handler_apply_requires_audit_reason_and_confirm_ids() {
        // GREEN: handler validator rejects apply=true with missing
        // confirm_ids OR missing audit_reason. Sets up a minimal
        // binding so the handler can resolve source_repo.
        let repo = setup_repo("handler_validators");
        let home = repo.parent().unwrap().to_path_buf();
        let agent = "test-agent";
        let binding_dir = home.join("runtime").join(agent);
        std::fs::create_dir_all(&binding_dir).expect("mkdir");
        std::fs::write(
            binding_dir.join("binding.json"),
            serde_json::json!({
                "source_repo": repo.display().to_string(),
                "branch": "feature",
                "worktree": repo.display().to_string(),
            })
            .to_string(),
        )
        .expect("write binding");

        // apply=true without confirm_ids → reject.
        let r = crate::mcp::handlers::ci::handle_cleanup_merged_branches(
            &home,
            &serde_json::json!({"instance": agent, "apply": true}),
            agent,
        );
        assert!(
            r["error"]
                .as_str()
                .map(|e| e.contains("confirm_ids"))
                .unwrap_or(false),
            "missing confirm_ids must reject: {r}"
        );
        assert_eq!(r["code"], "missing_confirm_ids");

        // apply=true with confirm_ids but no audit_reason → reject.
        let r = crate::mcp::handlers::ci::handle_cleanup_merged_branches(
            &home,
            &serde_json::json!({
                "instance": agent,
                "apply": true,
                "confirm_ids": ["some-branch"],
            }),
            agent,
        );
        assert!(
            r["error"]
                .as_str()
                .map(|e| e.contains("audit_reason"))
                .unwrap_or(false),
            "missing audit_reason must reject: {r}"
        );
        assert_eq!(r["code"], "missing_audit_reason");

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn test_branch_sweep_handler_active_unknown_remains_fail_closed() {
        // A branch in `active_unknown` (recent, unmerged, not
        // squash-applied) is NOT in `candidate_ids` and remains visible in
        // the dry-run response. Even an explicit confirm cannot delete it:
        // unknown provenance is a lifecycle KEEP decision.
        let repo = setup_repo("active_unknown_opt_in");
        let home = repo.parent().unwrap().to_path_buf();
        let agent = "test-agent";
        let binding_dir = home.join("runtime").join(agent);
        std::fs::create_dir_all(&binding_dir).expect("mkdir");
        std::fs::write(
            binding_dir.join("binding.json"),
            serde_json::json!({
                "source_repo": repo.display().to_string(),
                "branch": "feature",
                "worktree": repo.display().to_string(),
            })
            .to_string(),
        )
        .expect("write binding");

        // Create a recent unmerged branch → active_unknown.
        create_branch_with_commit(&repo, "wip-active", "feat: active wip");

        // Dry-run: candidate_ids should be empty for wip-active
        // (only deletable buckets); active_unknown is in categories
        // but not in candidate_ids.
        let r = crate::mcp::handlers::ci::handle_cleanup_merged_branches(
            &home,
            &serde_json::json!({"instance": agent}),
            agent,
        );
        let candidate_ids: Vec<&str> = r["candidate_ids"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(
            !candidate_ids.contains(&"wip-active"),
            "wip-active must NOT be in candidate_ids (active_unknown is non-deletable), got: {candidate_ids:?}"
        );
        let active_unknown: Vec<&str> = r["categories"]["active_unknown"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect();
        assert!(
            active_unknown.contains(&"wip-active"),
            "wip-active must appear in active_unknown bucket for visibility, got: {active_unknown:?}"
        );

        // Apply with wip-active in confirm_ids → handler reports no deletion
        // and preserves the branch, despite the name being in all_ids.
        let r = crate::mcp::handlers::ci::handle_cleanup_merged_branches(
            &home,
            &serde_json::json!({
                "instance": agent,
                "apply": true,
                "confirm_ids": ["wip-active"],
                "audit_reason": "verify active_unknown remains preserved",
            }),
            agent,
        );
        assert_eq!(r["applied"], 0, "active_unknown must remain preserved: {r}");
        assert!(
            enumerate_branches(&repo)
                .expect("branches")
                .iter()
                .any(|candidate| candidate.name == "wip-active"),
            "active_unknown branch must remain after explicit confirmation"
        );

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

<<<<<<< HEAD
    /// RED: a clean-merged branch with a non-GitHub local origin causes
    /// `open_pr_status` → `Unknown` → lifecycle Keep. The apply response
    /// currently returns only `applied: 0` with zero structured explanation.
    /// Assert that a `skipped` list surfaces the concrete blocker so
    /// operators can distinguish "safely preserved because of open-PR
    /// uncertainty" from "nothing matched".
    #[test]
    fn apply_skipped_surfaces_local_origin_unknown_pr_blocker() {
        let repo = setup_repo("skip-local-pr-unknown");
        let home = repo.parent().unwrap().to_path_buf();
        let agent = "skip-pr-agent";

        // Local bare origin → extract_github_repo returns None →
        // OpenPrStatus::Unknown for any terminal branch.
        add_local_bare_origin(&repo);

        // Create a branch and merge it → clean_merged (terminal provenance).
        create_branch_with_commit(&repo, "feat-merged-local", "feat: local work");
        git_run(
            &repo,
            &[
                "merge",
                "--no-ff",
                "-m",
                "merge feat-merged-local",
                "feat-merged-local",
            ],
        );

        bind_handler_repo(&home, &repo, agent);

        // Apply with the merged branch → lifecycle Keep (open_pr = Unknown).
        let r = crate::mcp::handlers::ci::handle_cleanup_merged_branches(
            &home,
            &serde_json::json!({
                "instance": agent,
                "apply": true,
                "confirm_ids": ["feat-merged-local"],
                "audit_reason": "RED: verify skipped reason surfaces",
            }),
            agent,
        );
        assert_eq!(r["applied"], 0, "branch must be preserved: {r}");

        // The response MUST contain a structured skipped list.
        let skipped = r["skipped"]
            .as_array()
            .unwrap_or_else(|| panic!("apply response must contain 'skipped' array, got: {r}"));
        assert_eq!(skipped.len(), 1, "exactly one skipped entry expected: {r}");
        assert_eq!(
            skipped[0]["branch"], "feat-merged-local",
            "skipped entry must name the branch: {r}"
        );
        assert_eq!(
            skipped[0]["blocker"], "open_pr_status_unknown",
            "skipped entry must pin the exact blocker: {r}"
        );

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    /// RED: a clean-merged branch with a live binding on another agent
    /// causes `active_holder` → `Some(true)` → lifecycle Keep. The apply
    /// response must surface the concrete binding blocker, not just
    /// `applied: 0`.
    #[test]
    fn apply_skipped_surfaces_active_binding_blocker() {
        let repo = setup_repo("skip-active-binding");
        let home = repo.parent().unwrap().to_path_buf();
        let caller = "skip-binding-caller";
        let holder = "holder-agent";

        // Create and merge a branch → clean_merged.
        create_branch_with_commit(&repo, "feat-held", "feat: held work");
        git_run(
            &repo,
            &["merge", "--no-ff", "-m", "merge feat-held", "feat-held"],
        );

        // Bind the caller agent so handle_cleanup finds source_repo.
        bind_handler_repo(&home, &repo, caller);

        // Create a SECOND agent's binding on the same branch + repo →
        // branch_has_active_binding returns Some(true).
        let holder_dir = home.join("runtime").join(holder);
        std::fs::create_dir_all(&holder_dir).expect("holder dir");
        std::fs::write(
            holder_dir.join("binding.json"),
            serde_json::json!({
                "source_repo": repo.display().to_string(),
                "branch": "feat-held",
                "worktree": repo.display().to_string(),
            })
            .to_string(),
        )
        .expect("holder binding");

        let r = crate::mcp::handlers::ci::handle_cleanup_merged_branches(
            &home,
            &serde_json::json!({
                "instance": caller,
                "apply": true,
                "confirm_ids": ["feat-held"],
                "audit_reason": "verify binding blocker surfaces",
            }),
            caller,
        );
        assert_eq!(r["applied"], 0, "branch must be preserved: {r}");

        let skipped = r["skipped"]
            .as_array()
            .unwrap_or_else(|| panic!("apply response must contain 'skipped' array, got: {r}"));
        assert_eq!(skipped.len(), 1, "exactly one skipped entry expected: {r}");
        assert_eq!(
            skipped[0]["branch"], "feat-held",
            "skipped entry must name the branch: {r}"
        );
        assert_eq!(
            skipped[0]["blocker"], "active_holder",
            "skipped entry must pin the exact blocker: {r}"
        );

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    /// Bug 3 RED: dry_run_observability must not abort when a reviewer_checkout
    /// branch from the categories no longer exists in a fresh branch enumeration.
    /// This can happen when a concurrent cleanup or manual deletion removes the
    /// branch between scan() and dry_run_observability(). Currently the code
    /// calls ok_or_else(...) which returns Err, aborting the entire dry-run.
    #[test]
    fn dry_run_observability_skips_absent_reviewer_branch() {
        let repo = setup_repo("absent_reviewer");
        add_local_bare_origin(&repo);

        // Build categories with a reviewer_checkout entry for a branch that
        // does NOT exist in the repo (simulating concurrent deletion).
        let categories = Categories {
            reviewer_checkout: vec![Candidate {
                name: "tmp_gone".to_string(),
                tip_sha: "0000000000000000000000000000000000000000".to_string(),
                reason: "reviewer checkout pattern".to_string(),
            }],
            ..Default::default()
        };

        let result = dry_run_observability(&repo, "main", &categories);
        assert!(
            result.is_ok(),
            "dry_run_observability must not abort when a reviewer branch is absent, got: {:?}",
            result.err()
        );

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }
}
