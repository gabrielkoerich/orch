//! Git work in task worktrees: commits, pushes, PRs, worktree setup and diffs

pub mod cleanup;
pub mod commit;
pub mod cow;
pub mod diff;
pub mod git_ops;
pub mod progress;
pub mod push_retry;
pub mod screenshots;
pub mod shipcheck;
pub mod worktree;
