//! Worktree auto-cleanup v2 — runtime registry based.
//!
//! On by default; gated **opt-out** via `AGEND_WORKTREE_AUTO_CLEANUP=0`
//! (any other value, or unset, leaves it enabled — see `auto_cleanup_enabled`).
//! Sweeps worktrees whose branches are merged into main OR whose remote
//! tracking ref has been deleted (squash-merged PRs), using the daemon's
//! live AgentConfig registry to find repos and detect in-use worktrees.
//! Also prunes orphaned local branches with no worktree.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod occupancy;
pub(crate) use occupancy::{binding_scan_all_strict, bound_worktree_paths_or_ambiguous, is_in_use};

/// Returns true unless `AGEND_WORKTREE_AUTO_CLEANUP` is explicitly set to "0".
/// Cleanup is on by default — set `AGEND_WORKTREE_AUTO_CLEANUP=0` to disable.
pub fn auto_cleanup_enabled() -> bool {
    std::env::var("AGEND_WORKTREE_AUTO_CLEANUP")
        .ok()
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// Entry for a git worktree.
#[derive(Debug, Clone)]
pub struct WorktreeEntry {
    pub path: String,
    pub branch: String,
}

/// List all git worktrees (excluding the main worktree).
fn list_worktrees(repo_root: &Path) -> Vec<WorktreeEntry> {
    // git-raw-allowed: TRIM-SENSITIVE parser. `--porcelain` terminates each
    // worktree record with a blank line; the loop below flushes a pending entry
    // on that blank line. `git_cmd` trims trailing whitespace → the final record's
    // terminator is dropped → the last (often only) worktree is never pushed →
    // the sweep silently finds nothing. Must read raw, untrimmed stdout.
    // (Already AGEND_GIT_BYPASS.)
    let output = match Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();
    let mut current_path = None;
    let mut current_branch = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            current_path = Some(p.to_string());
        } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
            current_branch = Some(b.to_string());
        } else if line.is_empty() {
            if let (Some(path), Some(branch)) = (current_path.take(), current_branch.take()) {
                if branch != "main" && branch != "master" {
                    entries.push(WorktreeEntry { path, branch });
                }
            }
            current_path = None;
            current_branch = None;
        }
    }
    entries
}

fn branch_tip_info(repo_root: &Path, branch: &str) -> Option<(String, u64)> {
    let hash = crate::git_helpers::git_cmd(repo_root, &["rev-parse", branch]).ok()?;
    let ts_str =
        crate::git_helpers::git_cmd(repo_root, &["log", "-1", "--format=%ct", branch]).ok()?;
    let ts: u64 = ts_str.parse().ok()?;
    Some((hash, ts))
}

/// Check if a branch is merged into the default branch (local check, no API needed).
fn is_branch_merged(repo_root: &Path, branch: &str) -> bool {
    let default = crate::git_helpers::default_branch(repo_root);
    // W1.2: git_ok = always-bypass + bounded, true iff exit-0 (the
    // `output().map(success).unwrap_or(false)` idiom, byte-for-byte).
    if !crate::git_helpers::git_ok(
        repo_root,
        &["merge-base", "--is-ancestor", branch, &default],
    ) {
        return false;
    }
    // #t-…81457-1: is-ancestor is trivially TRUE when `branch`'s tip IS
    // `default`'s tip — indistinguishable, from git state alone, between a
    // brand-new zero-commit branch (nothing ever merged — dev3's PRUNE_LIVE
    // incident) and a genuinely fast-forward-merged branch (whose tip
    // legitimately became identical to default's). There is no git-content
    // signal that tells these apart. Reuse the SAME `SQUASH_GC_MIN_TIP_AGE`
    // floor the squash path already relies on for the identical reason: only
    // trust "merged" once the shared tip has sat for a while, giving a
    // just-created branch's binding-registry entry (fix #1, the primary
    // defense) time to be observed even if that check somehow lagged.
    let Some((_, tip_ts)) = branch_tip_info(repo_root, branch) else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Duration::from_secs(now.saturating_sub(tip_ts)) >= SQUASH_GC_MIN_TIP_AGE
}

/// Check if a branch's remote tracking ref has been deleted (i.e. the PR was
/// squash-merged or the remote branch was deleted). This catches the common
/// case where `is_branch_merged` returns false because GitHub squash-merge
/// rewrites the commit hash.
fn is_remote_gone(repo_root: &Path, branch: &str) -> bool {
    // Read upstream tracking remote name
    // W1.2: git_cmd → trimmed stdout on success; the `success && !stdout.is_empty()`
    // filter becomes Ok-then-non-empty.
    let remote =
        crate::git_helpers::git_cmd(repo_root, &["config", &format!("branch.{branch}.remote")])
            .ok()
            .filter(|s| !s.is_empty());
    let Some(remote) = remote else {
        // No remote configured — not a remote-tracking branch, don't treat as "gone"
        return false;
    };
    // #t-…81457-1: refs/remotes/{remote}/{branch}'s absence only means "the
    // remote branch was deleted" when the branch was ever tracking THAT same
    // remote branch to begin with. bind_self's branch creation (git branch
    // <name> origin/main) auto-sets branch.<name>.merge = refs/heads/main
    // (tracks origin/main, not a same-named remote branch) — a branch that's
    // simply never been pushed under its own name would otherwise be
    // conflated with "was pushed, remote then deleted" and misclassified as
    // gone. Self-reproduced live: this agent's own fresh worktree and
    // gapfix-dev2's were both reaped this way within ~70s of bind, before
    // either had pushed. Require the upstream merge ref to actually be
    // refs/heads/<branch> first; on any ambiguity (missing/mismatched)
    // prefer NOT concluding gone — a false negative just waits for the next
    // sweep once the branch is genuinely pushed-then-orphaned, a false
    // positive is an irrecoverable delete.
    let merge_ref =
        crate::git_helpers::git_cmd(repo_root, &["config", &format!("branch.{branch}.merge")]).ok();
    if merge_ref.as_deref() != Some(format!("refs/heads/{branch}").as_str()) {
        return false;
    }
    // Check if the remote ref still exists
    let remote_ref = format!("refs/remotes/{remote}/{branch}");
    // git-raw-allowed: error→EXISTS (`unwrap_or(true)`) is a deliberate safe
    // default — a transient git error must NOT be read as "remote gone" (which
    // would auto-delete a live branch). `git_ok`'s error→false would INVERT this,
    // so do not "tidy" this into git_ok. (Already AGEND_GIT_BYPASS.)
    let exists = Command::new("git")
        .args(["rev-parse", "--verify", &remote_ref])
        .current_dir(repo_root)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(true);
    !exists
}

/// Check if a worktree has uncommitted changes.
fn is_worktree_dirty(worktree_path: &Path) -> bool {
    // git-raw-allowed: error→DIRTY (`unwrap_or(true)`) is a deliberate safe
    // default — a git error must protect uncommitted work, not let it be swept.
    // `git_ok`'s error→false would invert this (also: needs `!stdout.is_empty()`,
    // not exit-status). (Already AGEND_GIT_BYPASS.)
    Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(worktree_path)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(true)
}

/// Remove a worktree, and delete its branch ONLY when `delete_branch` is set.
///
/// CR-2026-06-14 (data-loss): the worktree-DIR removal and the `git branch -D`
/// are DECOUPLED. Reclaiming the worktree directory is harmless, but deleting
/// the branch ref is irreversible — a remote-gone branch carrying
/// committed-but-unpushed local work would lose it. The caller passes
/// `delete_branch = true` only when the work is preserved in the default branch
/// (merged or squash-merged); otherwise the branch ref (and its unpushed
/// commits) survives even though the stale worktree dir is reclaimed.
///
/// On Windows, retries up to 3 times with exponential backoff (200ms, 400ms)
/// to absorb transient EACCES from file locks held by preceding git processes.
fn remove_worktree(
    repo_root: &Path,
    worktree_path: &str,
    branch: &str,
    delete_branch: bool,
) -> bool {
    let max_attempts: u32 = if cfg!(windows) { 3 } else { 1 };
    let mut wt_ok = false;
    for attempt in 0..max_attempts {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(100 * (1 << attempt)));
        }
        wt_ok = crate::git_helpers::git_ok(
            repo_root,
            &["worktree", "remove", "--force", worktree_path],
        );
        if wt_ok {
            break;
        }
    }
    if wt_ok && delete_branch {
        // W1.2: best-effort branch delete (result was already ignored).
        let _ = crate::git_helpers::git_ok(repo_root, &["branch", "-D", branch]);
    }
    wt_ok
}

/// Runtime-based sweep: uses live `binding.json` state to find repos and
/// AgentConfig working_dirs to detect in-use worktrees.
///
/// `configs`: map of agent name → (working_dir, worktree_source) from daemon's live registry.
/// `fleet_dirs`: fallback working_directories from fleet.yaml for stopped agents.
///
/// Returns list of (branch, path, repo) that were removed.
pub fn sweep_from_registry(
    home: &Path,
    configs: &HashMap<String, (Option<PathBuf>, Option<PathBuf>)>,
    fleet_dirs: &[PathBuf],
) -> Vec<(String, String)> {
    if !auto_cleanup_enabled() {
        return Vec::new();
    }

    // Collect unique source repos from active configs and live bindings
    let mut repos: HashSet<PathBuf> = HashSet::new();
    for src in crate::binding::bound_source_repos(home) {
        repos.insert(src);
    }
    let mut active_dirs: Vec<PathBuf> = Vec::new();

    for (working_dir, worktree_source) in configs.values() {
        if let Some(src) = worktree_source {
            repos.insert(src.clone());
        }
        if let Some(wd) = working_dir {
            active_dirs.push(wd.clone());
        }
    }
    // Add fleet.yaml dirs as fallback for stopped agents
    active_dirs.extend(fleet_dirs.iter().cloned());

    // #t-…81457-1: `configs` (in-memory AgentConfig.working_dir) is a SEPARATE
    // registry from binding.json, updated on its own schedule — a worktree the
    // daemon itself just auto-bound (binding.json written) can be invisible to
    // `configs` until it catches up. binding.json is read fresh every call and
    // is authoritative for "is anyone bound here right now", so feed it into
    // the same occupancy check directly instead of relying solely on `configs`.
    //
    // reviewer4 REJECTED r0 of this fix: an unreadable/corrupt binding.json is
    // an AMBIGUITY, not an absence — it could be hiding a live worktree. Fail
    // the whole round closed (no removals) rather than treat it as "not
    // bound"; deletion is auto-run, "寧可漏收不可誤收". A merely-missing file
    // (agent never bound) is the normal steady state and does not trigger this.
    match bound_worktree_paths_or_ambiguous(home) {
        Ok(paths) => active_dirs.extend(paths),
        Err(()) => {
            tracing::warn!(
                "worktree-reclaim: an unreadable/corrupt binding.json was found — \
                 skipping ALL removals this sweep tick (fail-closed); will retry \
                 next tick"
            );
            return Vec::new();
        }
    }

    let mut removed = Vec::new();

    for repo in &repos {
        // Prune stale remote refs before remote-gone detection
        let remote = crate::git_helpers::primary_remote(repo);
        // git-raw-allowed: NETWORK op — `git_cmd` hardcodes LOCAL_GIT_TIMEOUT (60s),
        // too tight for a fetch; use the raw form (already AGEND_GIT_BYPASS) rather
        // than shoehorn a network op through the local helper. (A `git_cmd_network`
        // variant is YAGNI for this single fire-and-forget fetch.)
        let _ = Command::new("git")
            .args(["fetch", "--prune", &remote])
            .current_dir(repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output();

        // CR-2026-06-14: needed to decide whether a stale worktree's branch is
        // safe to `branch -D` (its work is in the default branch) vs must be kept
        // (committed-but-unpushed local work that a remote-gone signal alone
        // would otherwise destroy). Mirrors the phase-2 `prune_orphaned_branches`
        // safety gate.
        let default = crate::git_helpers::default_branch(repo);

        // Phase 1: clean worktrees (existing logic + remote-gone)
        let entries = list_worktrees(repo);
        for entry in &entries {
            let wt_path = Path::new(&entry.path);

            if is_in_use(wt_path, &active_dirs) {
                tracing::debug!(branch = %entry.branch, path = %entry.path, "skipping worktree (in use by agent)");
                continue;
            }

            if is_worktree_dirty(wt_path) {
                tracing::debug!(branch = %entry.branch, path = %entry.path, "skipping dirty worktree");
                continue;
            }

            let merged = is_branch_merged(repo, &entry.branch);
            let gone = is_remote_gone(repo, &entry.branch);
            if !merged && !gone {
                continue;
            }

            // CR-2026-06-14 (data-loss): reclaim the stale worktree DIR on
            // (merged || gone), but `branch -D` ONLY when the work is preserved
            // in the default branch — merged (ancestor) or squash-merged. A
            // remote-gone worktree whose branch is NEITHER carries
            // committed-but-unpushed local work; deleting the ref would lose it
            // irrecoverably (phase-1 only skips *dirty* worktrees, not
            // committed-but-unpushed ones). The worktree dir is still reclaimed.
            let branch_safe_to_delete =
                merged || is_squash_gc_eligible(repo, &entry.branch, &default);

            tracing::info!(
                branch = %entry.branch,
                path = %entry.path,
                reason = if merged { "merged" } else { "remote-gone" },
                delete_branch = branch_safe_to_delete,
                "removing stale worktree"
            );
            if remove_worktree(repo, &entry.path, &entry.branch, branch_safe_to_delete) {
                removed.push((entry.branch.clone(), entry.path.clone()));
            }
        }

        // Phase 2: prune orphaned branches (no worktree, remote gone or merged).
        // PR-D6: sweep is always live now (gated by AUTO_CLEANUP only), so the
        // helpers' still-supported `dry_run` param is passed `false` here.
        prune_stale_worktrees(repo, false);
        let pruned = prune_orphaned_branches_with_home(Some(home), repo, false);
        for (branch, _reason) in pruned {
            removed.push((branch, "(no worktree)".to_string()));
        }
    }
    // Durable retry: settle any cleanup intents whose branches are now
    // confirmed merged. This is the retry consumer for intents that survived
    // a failed poller settlement or whose CI watch was removed before the
    // settlement succeeded.
    crate::cleanup_intents::sweep_settle_merged(home);
    crate::cleanup_intents::reconcile_terminal_review_intents(home, false);
    removed
}

/// #1750-B3: minimum branch-tip age before the SQUASH-merged path will auto-GC
/// a branch. The `--merged`/remote-gone signals are definitive and need no age
/// belt, but the cherry/tree-diff squash detection is heuristic — a young branch
/// that happens to be tree-equal to main (or a PR merged moments ago that a
/// human may still follow up on locally) is left for a later tick. A
/// genuinely-orphaned squash-merged branch's tip predates the merge, so it
/// clears this floor on the next sweep.
const SQUASH_GC_MIN_TIP_AGE: Duration = Duration::from_secs(24 * 60 * 60);

const REVIEW_SCAFFOLD_TTL: Duration = Duration::from_secs(72 * 60 * 60);


fn is_stale_review_scaffold(repo_root: &Path, branch: &str) -> bool {
    if !crate::branch_sweep::is_reviewer_checkout(branch) {
        return false;
    }
    let Some((_, tip_ts)) = branch_tip_info(repo_root, branch) else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Duration::from_secs(now.saturating_sub(tip_ts)) >= REVIEW_SCAFFOLD_TTL
}

fn branch_reap_delete(provably_in_default: bool, remote_gone: bool, scaffold_ttl: bool) -> bool {
    use crate::worktree::disposition::{branch_disposition, BranchDisposition, BranchSignal};
    let signal = if provably_in_default {
        BranchSignal::ProvablyInDefault
    } else if remote_gone {
        BranchSignal::RemoteGoneOnly
    } else {
        BranchSignal::NotMerged
    };
    matches!(branch_disposition(signal), BranchDisposition::DeleteBranch) || scaffold_ttl
}

/// #1750-B3: age of `branch`'s tip commit (committer date), or `None` if it
/// can't be resolved. `%ct` is a unix timestamp (seconds), so no date parsing.
fn branch_tip_age(repo_root: &Path, branch: &str) -> Option<Duration> {
    // W1.2: git_cmd → trimmed stdout; spawn-error + non-zero both collapse to `None`.
    let ts_str =
        crate::git_helpers::git_cmd(repo_root, &["log", "-1", "--format=%ct", branch]).ok()?;
    let ts: u64 = ts_str.parse().ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(Duration::from_secs(now.saturating_sub(ts)))
}

/// #1750-B3: is `branch` a squash-merge orphan eligible for auto-GC? True when
/// it is squash-merged into the default branch AND its tip is older than
/// [`SQUASH_GC_MIN_TIP_AGE`]. Reuses `branch_sweep`'s detection (git cherry +
/// #1280 tree-diff fallback) so the auto path matches the operator sweep.
fn is_squash_gc_eligible(repo_root: &Path, branch: &str, default: &str) -> bool {
    crate::branch_sweep::is_squash_merged(repo_root, default, branch)
        && branch_tip_age(repo_root, branch).is_some_and(|age| age >= SQUASH_GC_MIN_TIP_AGE)
}

pub(crate) fn branch_has_active_binding(home: &Path, repo: &Path, branch: &str) -> Option<bool> {
    branch_has_other_active_binding(home, repo, branch, None)
}

pub(crate) fn branch_has_other_active_binding(
    home: &Path,
    repo: &Path,
    branch: &str,
    excluded_worktree: Option<&str>,
) -> Option<bool> {
    let canonical_repo = std::fs::canonicalize(repo).ok()?;
    let bindings = binding_scan_all_strict(home).ok()?;
    for (_, binding) in bindings {
        let bound_branch = binding["branch"]
            .as_str()
            .filter(|branch| !branch.is_empty())?;
        let source = binding["source_repo"]
            .as_str()
            .filter(|source| !source.is_empty())?;
        if excluded_worktree.is_some() && binding["worktree"].as_str().is_none_or(str::is_empty) {
            return None;
        }
        if excluded_worktree
            .zip(binding["worktree"].as_str())
            .is_some_and(|(excluded, bound)| excluded == bound)
        {
            continue;
        }
        let Ok(source) = std::fs::canonicalize(source) else {
            return None;
        };
        if bound_branch == branch && source == canonical_repo {
            return Some(true);
        }
    }
    Some(false)
}

/// Run `git worktree prune` then delete local branches whose remote tracking
/// ref is gone, that are merged into main, or that are squash-merge orphans
/// (#1750-B3). Skips branches checked out in any worktree.
///
/// #2605: `dry_run` computes the exact same eligibility (merged/squash gate,
/// worktree-occupancy skip) but skips the actual `git branch -D` — eligible
/// branches are still returned (with their real reason: `"merged"` or
/// `"squash-merged"`) so the caller can log/audit the candidate list.
#[allow(dead_code)]
fn prune_orphaned_branches(repo_root: &Path, dry_run: bool) -> Vec<(String, &'static str)> {
    prune_orphaned_branches_with_home(None, repo_root, dry_run)
}

fn prune_orphaned_branches_with_home(
    home: Option<&Path>,
    repo_root: &Path,
    dry_run: bool,
) -> Vec<(String, &'static str)> {
    let default = crate::git_helpers::default_branch(repo_root);
    // Collect branches currently checked out in worktrees — cannot delete these
    let wt_branches: HashSet<String> = list_worktrees(repo_root)
        .into_iter()
        .map(|e| e.branch)
        .collect();

    // W1.2: git_cmd → trimmed stdout on success; spawn-error + non-zero collapse to `Err → []`.
    let branches: Vec<String> =
        match crate::git_helpers::git_cmd(repo_root, &["branch", "--format=%(refname:short)"]) {
            Ok(stdout) => stdout
                .lines()
                .filter(|b| *b != default.as_str())
                .map(String::from)
                .collect(),
            _ => return Vec::new(),
        };
    // Snapshot the open-PR inventory once for this repository/sweep.  Each
    // branch below consumes the bounded snapshot instead of issuing its own
    // synchronous SCM lookup; an Unknown snapshot keeps all terminal
    // candidates fail-closed.
    let open_pr_snapshot = crate::branch_sweep::open_pr_snapshot(repo_root, &default);

    let mut pruned = Vec::new();
    for branch in &branches {
        if wt_branches.contains(branch) {
            continue;
        }
        let merged = is_branch_merged(repo_root, branch);
        // #1750-B3: also reap squash-merge orphans (the 95/99 case the
        // squash-blind `--merged` missed) — gated on tip-age for the heuristic
        // squash detection only.
        //
        // CR-2026-06-14: `is_remote_gone` is NO LONGER an independent delete
        // trigger. Remote-gone alone is not proof the branch's work is preserved
        // — a branch pushed once, then deleted on the remote while local commits
        // kept accruing, is remote-gone yet carries committed-but-unpushed work
        // that `git branch -D` destroys irrecoverably. Reap a branch ONLY when
        // its work IS in the default branch (every commit reachable): merged
        // (ancestor) or squash-merged. A remote-gone branch that is NEITHER has
        // unpushed local commits → KEEP. A squash-merged branch whose remote was
        // auto-deleted stays reapable — it is now caught by the squash check
        // (which no longer excludes the gone case) instead of the unguarded
        // remote-gone trigger.
        let squash = !merged && is_squash_gc_eligible(repo_root, branch, &default);
        // PR-A P1 (branch-residue RCA §3): disposable reviewer-checkout
        // scaffolding (review/* etc.) that is unoccupied (checked above) and
        // aged past REVIEW_SCAFFOLD_TTL. These never carry a PR and never merge,
        // so the merged/squash paths never reap them — a TTL is their only live
        // terminal path (H1 in the RCA).
        let scaffold = !merged && !squash && is_stale_review_scaffold(repo_root, branch);
        let provenance = if merged {
            crate::worktree::disposition::BranchProvenance::Merged
        } else if squash {
            crate::worktree::disposition::BranchProvenance::SquashMerged
        } else if scaffold {
            crate::worktree::disposition::BranchProvenance::ReviewerResidue
        } else {
            crate::worktree::disposition::BranchProvenance::Unknown
        };
        let task_active = match home {
            Some(h) => crate::branch_sweep::branch_has_active_task(h, branch),
            None => Some(false),
        };
        let binding_active = match home {
            Some(h) => branch_has_active_binding(h, repo_root, branch),
            None => Some(false),
        };
        let active_holder = match (wt_branches.contains(branch), binding_active) {
            (true, _) | (_, Some(true)) => Some(true),
            (false, Some(false)) => Some(false),
            _ => None,
        };
        let open_pr = if merged || squash || scaffold {
            match open_pr_snapshot.status_for(branch) {
                crate::branch_sweep::OpenPrStatus::Open => Some(true),
                crate::branch_sweep::OpenPrStatus::NotOpen => Some(false),
                crate::branch_sweep::OpenPrStatus::Unknown => None,
            }
        } else {
            // Unknown provenance is already a KEEP decision; avoid an
            // unnecessary network probe for branches that cannot be deleted.
            Some(false)
        };
        let lifecycle = crate::worktree::disposition::branch_lifecycle_disposition(
            &crate::worktree::disposition::BranchLifecycleInput {
                provenance,
                terminal: merged || squash || scaffold,
                active_holder,
                task_active,
                open_pr,
                // Reviewer residue is snapshotted into a recovery ref before
                // deletion below; merged/squash work is already in `default`.
                unique_unpreserved_work: Some(false),
            },
        );
        // PR-D·D3: the reap decision (L4) delegates to `branch_disposition` via
        // `branch_reap_delete`. `merged || squash` = ProvablyInDefault → delete;
        // `scaffold` is the explicit external-arm override (a NotMerged branch the
        // classifier keeps, aged past its TTL). Phase-2 does not compute remote-gone
        // (CR-2026-06-14 dropped it as a trigger) → remote_gone=false. Byte-identical
        // to the prior `!merged && !squash && !scaffold` continue-gate.
        if !branch_reap_delete(merged || squash, false, scaffold)
            || !matches!(
                lifecycle,
                crate::worktree::disposition::BranchLifecycleDisposition::Delete
            )
        {
            continue;
        }
        // The scaffolding path never merged, so its commits survive only in the
        // object store once the branch ref is gone — capture the tip SHA BEFORE
        // deletion so the log keeps them recoverable.
        let scaffold_tip = if scaffold {
            branch_tip_info(repo_root, branch).map(|(sha, _)| sha)
        } else {
            None
        };
        if scaffold && !dry_run {
            if let Some(tip) = scaffold_tip.as_deref() {
                if crate::branch_sweep::prepare_branch_recovery(
                    home,
                    repo_root,
                    branch,
                    tip,
                    "review-scaffold-ttl",
                )
                .is_err()
                {
                    continue;
                }
            } else {
                continue;
            }
        }
        let ok = dry_run || crate::git_helpers::git_ok(repo_root, &["branch", "-D", branch]);
        let reason = if merged {
            "merged"
        } else if squash {
            "squash-merged"
        } else {
            "review-residue"
        };
        if ok {
            if dry_run {
                tracing::info!(branch, reason, tip_sha = ?scaffold_tip, "would prune orphaned branch (dry-run)");
            } else {
                tracing::info!(branch, reason, tip_sha = ?scaffold_tip, "pruned orphaned branch");
            }
            pruned.push((branch.clone(), reason));
        }
    }
    pruned
}

/// Run `git worktree prune` to clean stale worktree bookkeeping entries.
fn prune_stale_worktrees(repo_root: &Path, dry_run: bool) {
    if dry_run {
        return;
    }
    // W1.2: best-effort prune (result was already ignored).
    let _ = crate::git_helpers::git_ok(repo_root, &["worktree", "prune"]);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn setup_test_repo(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agend-wt-v2-{}-{}-{}",
            tag,
            std::process::id(),
            C.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).ok();
        git_in(&dir, &["init", "-b", "main"]);
        std::fs::write(dir.join("README.md"), "init").ok();
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-m", "init"]);
        dir
    }

    fn git_in(dir: &Path, args: &[&str]) {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git");
    }

    #[test]
    fn test_flag_disabled_default() {
        let _lock = ENV_LOCK.lock();
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        assert!(auto_cleanup_enabled());
    }

    #[test]
    fn test_flag_disabled_explicit() {
        let _lock = ENV_LOCK.lock();
        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "0");
        assert!(!auto_cleanup_enabled());
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    }

    #[test]
    fn test_flag_enabled() {
        let _lock = ENV_LOCK.lock();
        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        assert!(auto_cleanup_enabled());
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    }

    #[test]
    fn test_sweep_noop_when_flag_disabled() {
        let _lock = ENV_LOCK.lock();
        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "0");
        let configs = HashMap::new();
        let home = tmp_home("sweep-noop");
        let removed = sweep_from_registry(&home, &configs, &[]);
        assert!(removed.is_empty());
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_v2_merged_worktree_removed() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("v2-merged");
        // #t-…81457-1: old dated commit so it clears is_branch_merged's age gate
        make_old_dated_branch(&repo, "feat/done", "2024-01-01T00:00:00 +0000");
        let wt = repo.join("wt-done");
        git_in(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "feat/done"],
        );
        git_in(&repo, &["merge", "feat/done"]);

        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        // No active agent using this worktree
        let mut configs = HashMap::new();
        configs.insert(
            "other-agent".to_string(),
            (Some(repo.join("other")), Some(repo.clone())),
        );
        let home = tmp_home("v2-merged");
        let removed = sweep_from_registry(&home, &configs, &[]);
        assert!(
            removed.iter().any(|(b, _)| b == "feat/done"),
            "merged worktree must be removed: {removed:?}"
        );
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_v2_dirty_worktree_preserved() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("v2-dirty");
        make_old_dated_branch(&repo, "feat/dirty", "2024-01-01T00:00:00 +0000");
        let wt = repo.join("wt-dirty");
        git_in(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "feat/dirty"],
        );
        git_in(&repo, &["merge", "feat/dirty"]);
        std::fs::write(wt.join("uncommitted.txt"), "dirty").ok();

        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        let mut configs = HashMap::new();
        configs.insert(
            "agent".to_string(),
            (Some(repo.join("other")), Some(repo.clone())),
        );
        let home = tmp_home("v2-dirty");
        let removed = sweep_from_registry(&home, &configs, &[]);
        assert!(removed.is_empty(), "dirty worktree must NOT be removed");
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_v2_unmerged_worktree_preserved() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("v2-unmerged");
        git_in(&repo, &["branch", "feat/wip"]);
        let wt = repo.join("wt-wip");
        git_in(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "feat/wip"],
        );
        std::fs::write(wt.join("new.txt"), "x").ok();
        git_in(&wt, &["add", "."]);
        git_in(&wt, &["commit", "-m", "wip"]);

        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        let mut configs = HashMap::new();
        configs.insert(
            "agent".to_string(),
            (Some(repo.join("other")), Some(repo.clone())),
        );
        let home = tmp_home("v2-unmerged");
        let removed = sweep_from_registry(&home, &configs, &[]);
        assert!(removed.is_empty(), "unmerged worktree must NOT be removed");
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[cfg(unix)] // Windows path format — t-20260424173948421544-1
    fn test_v2_active_runtime_worktree_not_removed_under_bootstrap_redirect() {
        // Production shape: agent's working_dir is <repo>/.worktrees/<agent>,
        // worktree_source is <repo>. Sweep must NOT remove the active worktree.
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("v2-active");
        make_old_dated_branch(&repo, "feat/active", "2024-01-01T00:00:00 +0000");
        let wt = repo.join("wt-active");
        git_in(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "feat/active"],
        );
        git_in(&repo, &["merge", "feat/active"]);
        // Merged + clean, but agent is actively using this worktree

        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        let mut configs = HashMap::new();
        // Agent's working_dir points to the worktree (bootstrap redirect)
        configs.insert(
            "active-agent".to_string(),
            (Some(wt.clone()), Some(repo.clone())),
        );
        let home = tmp_home("v2-active");
        let removed = sweep_from_registry(&home, &configs, &[]);
        assert!(
            removed.is_empty(),
            "active agent worktree must NOT be removed: {removed:?}"
        );
        assert!(wt.exists(), "worktree dir must still exist");
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_v2_remote_gone_worktree_removed() {
        // Simulate squash-merge: branch is NOT merged (different hash) but
        // remote tracking ref is gone after `git fetch --prune`.
        let _lock = ENV_LOCK.lock();

        // Create "remote" bare repo
        let remote_dir = std::env::temp_dir().join(format!(
            "agend-wt-v2-remote-gone-{}-{}",
            std::process::id(),
            std::sync::atomic::AtomicU32::new(0).fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&remote_dir).ok();
        git_in(&remote_dir, &["init", "--bare", "-b", "main"]);

        // Clone it
        let repo = std::env::temp_dir().join(format!(
            "agend-wt-v2-remote-gone-clone-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&repo);
        Command::new("git")
            .args([
                "clone",
                remote_dir.to_str().unwrap(),
                repo.to_str().unwrap(),
            ])
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("clone");
        std::fs::write(repo.join("README.md"), "init").ok();
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-m", "init"]);
        git_in(&repo, &["push", "-u", "origin", "main"]);

        // Create a feature branch, push it, then delete remote ref
        git_in(&repo, &["checkout", "-b", "feat/squashed"]);
        std::fs::write(repo.join("feat.txt"), "feature").ok();
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-m", "feature work"]);
        git_in(&repo, &["push", "-u", "origin", "feat/squashed"]);
        git_in(&repo, &["checkout", "main"]);

        // Create worktree on that branch
        let wt = repo.join("wt-squashed");
        git_in(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "feat/squashed"],
        );

        // Simulate: remote branch deleted (squash-merged on GitHub)
        git_in(&remote_dir, &["branch", "-D", "feat/squashed"]);
        git_in(&repo, &["fetch", "--prune"]);

        // Branch is NOT merged (different commit hash) but remote is gone
        assert!(
            !is_branch_merged(&repo, "feat/squashed"),
            "branch should NOT be detected as merged"
        );
        assert!(
            is_remote_gone(&repo, "feat/squashed"),
            "branch remote should be detected as gone"
        );

        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        let mut configs = HashMap::new();
        configs.insert(
            "other".to_string(),
            (Some(repo.join("other")), Some(repo.clone())),
        );
        let home = tmp_home("v2-remote-gone");
        let removed = sweep_from_registry(&home, &configs, &[]);
        assert!(
            removed.iter().any(|(b, _)| b == "feat/squashed"),
            "remote-gone worktree must be removed: {removed:?}"
        );
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&remote_dir).ok();
    }

    // ── #1750-B3: local squash-merge orphan auto-GC ──

    /// Commit like `git_in`'s commit but with a fixed author+committer DATE, so
    /// `branch_tip_age` is deterministic regardless of wall-clock.
    fn git_commit_dated(dir: &Path, msg: &str, date: &str) {
        Command::new("git")
            .args(["commit", "-m", msg])
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .output()
            .expect("dated commit");
    }

    /// Build a LOCAL squash-merge orphan: `branch` carries `feat.txt`, then main
    /// diverges (`other.txt`) and cherry-picks `branch`'s patch — so `branch` is
    /// NOT a `--merged` ancestor (different SHA) but IS squash-merged (git cherry
    /// shows `-`). `branch`'s tip is committed at `tip_date`.
    fn make_squash_orphan(repo: &Path, branch: &str, tip_date: &str) {
        git_in(repo, &["checkout", "-b", branch]);
        std::fs::write(repo.join("feat.txt"), "feature").ok();
        git_in(repo, &["add", "."]);
        git_commit_dated(repo, "feature work", tip_date);
        git_in(repo, &["checkout", "main"]);
        // Diverge main on a DIFFERENT file so the cherry-pick applies cleanly.
        std::fs::write(repo.join("other.txt"), "main-side").ok();
        git_in(repo, &["add", "."]);
        git_in(repo, &["commit", "-m", "main diverge"]);
        git_in(repo, &["cherry-pick", branch]);
    }

    #[test]
    fn prune_squash_merged_old_branch_1750_b3() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("b3-squash-old");
        // Old tip (well past SQUASH_GC_MIN_TIP_AGE) + squash-merged into main.
        make_squash_orphan(&repo, "feat/squash-old", "2024-01-01T00:00:00 +0000");
        // Precondition: the squash-blind signals MISS it (the #1750 bug).
        assert!(
            !is_branch_merged(&repo, "feat/squash-old"),
            "not a --merged ancestor"
        );
        assert!(
            !is_remote_gone(&repo, "feat/squash-old"),
            "no remote configured"
        );

        let pruned = prune_orphaned_branches(&repo, false);
        assert!(
            pruned.iter().any(|b| b.0 == "feat/squash-old"),
            "#1750-B3: a squash-merged orphan past the age floor must be auto-GC'd, got: {pruned:?}"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn prune_skips_squash_merged_too_new_1750_b3() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("b3-squash-new");
        // Squash-merged but tip committed NOW (git_in default date) → under the
        // age floor → must NOT be deleted yet (a later sweep reaps it).
        git_in(&repo, &["checkout", "-b", "feat/squash-new"]);
        std::fs::write(repo.join("feat.txt"), "feature").ok();
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-m", "feature work"]); // now-dated tip
        git_in(&repo, &["checkout", "main"]);
        std::fs::write(repo.join("other.txt"), "main-side").ok();
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-m", "main diverge"]);
        git_in(&repo, &["cherry-pick", "feat/squash-new"]);

        assert!(
            crate::branch_sweep::is_squash_merged(&repo, "main", "feat/squash-new"),
            "precondition: detected as squash-merged"
        );
        let pruned = prune_orphaned_branches(&repo, false);
        assert!(
            !pruned.iter().any(|b| b.0 == "feat/squash-new"),
            "#1750-B3: a squash-merged branch under the tip-age floor must NOT be GC'd yet"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn prune_skips_unmerged_branch_1750_b3() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("b3-unmerged");
        // A genuinely unmerged branch (old tip) — squash detection must NOT fire.
        git_in(&repo, &["checkout", "-b", "feat/wip"]);
        std::fs::write(repo.join("feat.txt"), "wip").ok();
        git_in(&repo, &["add", "."]);
        git_commit_dated(&repo, "wip", "2024-01-01T00:00:00 +0000");
        git_in(&repo, &["checkout", "main"]);

        assert!(
            !crate::branch_sweep::is_squash_merged(&repo, "main", "feat/wip"),
            "precondition: NOT squash-merged"
        );
        let pruned = prune_orphaned_branches(&repo, false);
        assert!(
            !pruned.iter().any(|b| b.0 == "feat/wip"),
            "#1750-B3: a real unmerged branch must NOT be GC'd"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn prune_skips_checked_out_squash_orphan_1750_b3() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("b3-squash-checkedout");
        make_squash_orphan(&repo, "feat/squash-wt", "2024-01-01T00:00:00 +0000");
        // Check the squash-merged branch out in a worktree → must be skipped.
        let wt = repo.join("wt-squash");
        git_in(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "feat/squash-wt"],
        );

        let pruned = prune_orphaned_branches(&repo, false);
        assert!(
            !pruned.iter().any(|b| b.0 == "feat/squash-wt"),
            "#1750-B3: a squash-merged branch checked out in a worktree must NOT be GC'd"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    /// #t-…81457-1: build a branch with a single OLD dated commit (checked out
    /// then back to main), so `is_branch_merged`'s age gate treats it as
    /// genuinely merged rather than a suspiciously-fresh zero-commit branch.
    /// Mirrors `make_squash_orphan`'s dating approach but for a plain
    /// fast-forward-mergeable branch (no divergence from main).
    fn make_old_dated_branch(repo: &Path, branch: &str, tip_date: &str) {
        git_in(repo, &["checkout", "-b", branch]);
        std::fs::write(repo.join("feat.txt"), "feature").ok();
        git_in(repo, &["add", "."]);
        git_commit_dated(repo, "feature work", tip_date);
        git_in(repo, &["checkout", "main"]);
    }

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-wt-v2-cleanup-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn write_source_repo_binding(home: &Path, agent: &str, source_repo: &Path) {
        let dir = crate::paths::runtime_dir(home).join(agent);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("binding.json"),
            serde_json::to_string(&serde_json::json!({
                "source_repo": source_repo.display().to_string(),
            }))
            .unwrap(),
        )
        .unwrap();
    }

    /// Seed `home/runtime/<agent>/binding.json` with BOTH `source_repo` and
    /// `worktree` — the real production shape (`binding::bind`'s writer sets
    /// both). `write_source_repo_binding` (above) only sets `source_repo`,
    /// which is enough for repo-discovery tests but not for exercising
    /// worktree-occupancy via the binding registry.
    fn write_full_binding(
        home: &Path,
        agent: &str,
        branch: &str,
        source_repo: &Path,
        worktree: &Path,
    ) {
        let dir = crate::paths::runtime_dir(home).join(agent);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("binding.json"),
            serde_json::to_string(&serde_json::json!({
                "branch": branch,
                "source_repo": source_repo.display().to_string(),
                "worktree": worktree.display().to_string(),
            }))
            .unwrap(),
        )
        .unwrap();
    }

    /// #t-…81457-1 primary fix: a worktree with a LIVE `binding.json` entry
    /// must never be swept, even when the daemon's in-memory `configs`
    /// (AgentConfig.working_dir) registry hasn't caught up yet — the exact
    /// dev3 PRUNE_LIVE incident (auto-bind at 11:04, sweep at 11:08, `configs`
    /// still empty for that agent). Pre-fix, `is_in_use` only ever consulted
    /// `configs`/`fleet_dirs`, never `binding.json`, so this worktree — merged
    /// AND clean, exactly like dev3's — was eligible and got removed.
    #[test]
    fn sweep_skips_worktree_known_only_via_binding_json_not_yet_in_configs_registry() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("binding-only-occupancy");
        // Old dated commit so `is_branch_merged`'s age gate (fix #2) does NOT
        // protect this worktree — isolates fix #1 (binding-registry occupancy)
        // as the ONLY thing standing between this live-bound worktree and removal.
        make_old_dated_branch(&repo, "feat/fresh-bind", "2024-01-01T00:00:00 +0000");
        let wt = repo.join("wt-fresh-bind");
        git_in(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "feat/fresh-bind"],
        );
        git_in(&repo, &["merge", "feat/fresh-bind"]);

        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        std::env::set_var("AGEND_WORKTREE_PRUNE_LIVE", "1");
        let home = tmp_home("binding-only-occupancy");
        // `configs` deliberately EMPTY — the in-memory registry hasn't caught
        // up to the fresh bind yet. binding.json is the only live signal.
        let configs: HashMap<String, (Option<PathBuf>, Option<PathBuf>)> = HashMap::new();
        write_full_binding(&home, "dev3", "feat/fresh-bind", &repo, &wt);

        let removed = sweep_from_registry(&home, &configs, &[]);
        assert!(
            removed.is_empty(),
            "a worktree with a LIVE binding.json entry must never be swept, even \
             when `configs` hasn't caught up yet (the exact dev3 PRUNE_LIVE \
             incident): {removed:?}"
        );
        assert!(
            wt.exists(),
            "the live-bound worktree directory must survive"
        );

        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE");
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// #t-…81457-1 REJECTED rework (reviewer4 r0): an unreadable/corrupt
    /// `binding.json` for ANY agent is an AMBIGUITY, not an absence — it could
    /// be hiding the very live binding that would have protected the worktree
    /// under test. Pre-rework, `bound_worktree_paths` silently skipped it
    /// (same as a missing file), so an old (age-gate-cleared), clean, merged
    /// worktree with no binding of its OWN was still removed even though a
    /// SIBLING agent's binding.json existed but failed to parse. Reproduces
    /// reviewer4's exact repro shape: this must now skip the ENTIRE sweep
    /// round (fail closed), not just the ambiguous row.
    #[test]
    fn sweep_fails_closed_when_any_binding_json_is_corrupt() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("corrupt-binding-ambiguity");
        make_old_dated_branch(&repo, "feat/live-bound", "2024-01-01T00:00:00 +0000");
        let wt = repo.join("wt-live-bound");
        git_in(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "feat/live-bound"],
        );
        git_in(&repo, &["merge", "feat/live-bound"]);

        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        std::env::set_var("AGEND_WORKTREE_PRUNE_LIVE", "1");
        let home = tmp_home("corrupt-binding-ambiguity");
        let configs: HashMap<String, (Option<PathBuf>, Option<PathBuf>)> = HashMap::new();
        // One valid binding for repo discovery ...
        write_source_repo_binding(&home, "other-agent", &repo);
        // ... and one CORRUPT binding.json for a DIFFERENT agent — unrelated to
        // `feat/live-bound` on its face, but the daemon cannot know that from a
        // file it failed to parse.
        let corrupt_dir = crate::paths::runtime_dir(&home).join("dev3");
        std::fs::create_dir_all(&corrupt_dir).unwrap();
        std::fs::write(corrupt_dir.join("binding.json"), b"not valid json").unwrap();

        let removed = sweep_from_registry(&home, &configs, &[]);
        assert!(
            removed.is_empty(),
            "an unreadable/corrupt binding.json anywhere must fail the WHOLE \
             sweep round closed, even for an unrelated, otherwise-eligible \
             worktree: {removed:?}"
        );
        assert!(wt.exists(), "the worktree must survive the ambiguous round");

        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE");
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// #t-…81457-1 REJECTED rework, negative control: a MISSING binding.json
    /// (the normal steady state — most agents are never bound) must NOT
    /// trigger the fail-closed ambiguity path, or every legitimate cleanup
    /// case regresses (the 26 real candidates PRUNE_LIVE's first tick
    /// correctly reaped would silently stop being collected).
    #[test]
    fn sweep_still_removes_when_no_binding_json_exists_at_all() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("no-binding-normal");
        make_old_dated_branch(&repo, "feat/done", "2024-01-01T00:00:00 +0000");
        let wt = repo.join("wt-done");
        git_in(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "feat/done"],
        );
        git_in(&repo, &["merge", "feat/done"]);

        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        std::env::set_var("AGEND_WORKTREE_PRUNE_LIVE", "1");
        let home = tmp_home("no-binding-normal");
        let configs: HashMap<String, (Option<PathBuf>, Option<PathBuf>)> = HashMap::new();
        // Repo discovery needs ONE valid binding; no agent has a binding
        // pointing at `wt` itself, and no binding.json anywhere is corrupt.
        write_source_repo_binding(&home, "other-agent", &repo);

        let removed = sweep_from_registry(&home, &configs, &[]);
        assert!(
            removed.iter().any(|(b, _)| b == "feat/done"),
            "a genuinely unbound, old, clean, merged worktree must still be \
             removed — a merely-absent binding.json is not an ambiguity: {removed:?}"
        );

        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE");
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// #t-…81457-1 depth fix #1: `is_branch_merged`'s is-ancestor check is
    /// trivially true for a branch whose tip is IDENTICAL to the default
    /// branch (zero commits ever made) — nothing has actually been merged,
    /// there's nothing to merge. This is a unit-level pin on the exact
    /// function so the fix can't regress even if the occupancy fix (above)
    /// changes shape later — "單靠 ① 未來 binding 生命週期一變又漏" (lead).
    #[test]
    fn is_branch_merged_rejects_zero_commit_branch_tip_equals_default() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("zero-commit-merged-unit");
        git_in(&repo, &["branch", "feat/never-touched"]);

        assert!(
            !is_branch_merged(&repo, "feat/never-touched"),
            "a branch whose tip is IDENTICAL to main (zero commits, nothing ever \
             diverged) must not be classified as merged — there is nothing to merge"
        );

        std::fs::remove_dir_all(&repo).ok();
    }

    /// #t-…81457-1 depth fix #1, integration level: the same zero-commit
    /// scenario through the full sweep, with NO occupancy signal at all (no
    /// binding, no configs) — isolates this fix from the binding-registry fix
    /// above. This is dev3's actual incident mechanics minus the binding gap.
    #[test]
    fn sweep_does_not_treat_zero_commit_worktree_as_merged() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("zero-commit-sweep");
        git_in(&repo, &["branch", "feat/fresh-no-commits"]);
        let wt = repo.join("wt-fresh-no-commits");
        git_in(
            &repo,
            &[
                "worktree",
                "add",
                wt.to_str().unwrap(),
                "feat/fresh-no-commits",
            ],
        );

        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        std::env::set_var("AGEND_WORKTREE_PRUNE_LIVE", "1");
        let home = tmp_home("zero-commit-sweep");
        let configs: HashMap<String, (Option<PathBuf>, Option<PathBuf>)> = HashMap::new();
        write_source_repo_binding(&home, "other-agent", &repo); // repo discovery only

        let removed = sweep_from_registry(&home, &configs, &[]);
        assert!(
            removed.is_empty(),
            "a zero-commit branch (tip==main, nothing diverged) must not be \
             classified merged just because is-ancestor is trivially true: {removed:?}"
        );
        assert!(wt.exists());

        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE");
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// #t-…81457-1 depth fix #2, LIVE self-reproduced incident: production
    /// branch creation (`bind_self`'s `ensure_branch_fetch` → `git branch
    /// <name> origin/main`) auto-sets upstream tracking to `origin/main`
    /// (`branch.<name>.merge = refs/heads/main`), NOT to a same-named remote
    /// branch — because the branch has never been pushed under its own name.
    /// `is_remote_gone`'s `refs/remotes/{remote}/{branch}` existence check
    /// assumes the upstream mirrors the LOCAL branch's own name; it never does
    /// for a from-origin/main tracked branch, so a legitimately-never-pushed
    /// branch is misclassified as "remote gone". Self-reproduced live: this
    /// agent's own fresh worktree AND gapfix-dev2's were both
    /// `worktree_auto_removed reason=remote-gone` within ~70s of bind, before
    /// either had pushed (event-log confirmed, same tick).
    #[test]
    fn is_remote_gone_does_not_misfire_for_never_pushed_branch_tracking_main() {
        let _lock = ENV_LOCK.lock();
        let remote_dir = std::env::temp_dir().join(format!(
            "agend-wt-v2-neverpushed-remote-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&remote_dir).ok();
        git_in(&remote_dir, &["init", "--bare", "-b", "main"]);

        let repo = std::env::temp_dir().join(format!(
            "agend-wt-v2-neverpushed-clone-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&repo);
        Command::new("git")
            .args([
                "clone",
                remote_dir.to_str().unwrap(),
                repo.to_str().unwrap(),
            ])
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("clone");
        std::fs::write(repo.join("README.md"), "init").ok();
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-m", "init"]);
        git_in(&repo, &["push", "-u", "origin", "main"]);

        // Production shape: `git branch <name> origin/main`, NEVER pushed
        // under its own name.
        git_in(&repo, &["branch", "fix/never-pushed", "origin/main"]);

        assert!(
            !is_remote_gone(&repo, "fix/never-pushed"),
            "a branch that tracks origin/main (never pushed under its own name) \
             must NOT be classified remote-gone — refs/remotes/origin/<name> was \
             never supposed to exist for it in the first place"
        );

        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&remote_dir).ok();
    }
}

#[cfg(test)]
mod lifecycle_r1_tests;

#[cfg(test)]
mod review_repro_worktree_git;
