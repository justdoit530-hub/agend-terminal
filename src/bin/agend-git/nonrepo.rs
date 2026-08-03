//! #3142: non-repository cwd passthrough helpers for agend-git.
//!
//! `paths_are_foreign` fails closed (returns `false`) when the cwd has no
//! commondir. That is correct for the foreign-repo mutation gate, but a
//! read-only command like `status` or `rev-parse` would then be redirected
//! into the bound worktree and report a false repository context. These
//! helpers detect and handle that case separately.

use std::env;

use super::{find_git_dir, Action};

/// Returns `true` when the process cwd is NOT inside any git repository.
pub(crate) fn cwd_is_nonrepo() -> bool {
    env::current_dir()
        .map(|cwd| find_git_dir(&cwd).is_none())
        .unwrap_or(false)
}

/// Convert only bound read-only commands to Passthrough when cwd is not inside
/// any repository. Mutating commands retain their existing bound-worktree policy;
/// leading `-C` routing is handled separately.
pub(crate) fn apply_nonrepo_read_passthrough(
    action: Action,
    subcmd: &str,
    cwd_nonrepo: bool,
) -> Action {
    if cwd_nonrepo
        && matches!(action, Action::ChdirPass(_))
        && matches!(
            subcmd,
            "status"
                | "log"
                | "diff"
                | "show"
                | "blame"
                | "ls-files"
                | "ls-tree"
                | "rev-parse"
                | "fetch"
                | "remote"
                | "branch"
                | "tag"
                | "describe"
                | "shortlog"
                | "reflog"
        )
    {
        Action::Passthrough
    } else {
        action
    }
}
