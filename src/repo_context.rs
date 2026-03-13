//! Repo context propagated into spawned async tasks.
//!
//! The `REPO_CONTEXT` task-local allows store task resolution and other
//! per-repo operations to access the repo slug without requiring every
//! call-site to pass it explicitly.

tokio::task_local! {
    /// Repo slug (e.g. "owner/repo") propagated into spawned async tasks
    /// so that path resolution can scope files per-repo without
    /// requiring every call-site to pass the repo explicitly.
    pub static REPO_CONTEXT: String;
}
