//! `review.mode` per project: the project's `.orch.yml`, then the global config, then `comment`

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewMode {
    /// The `## Automated Review` comment and the `orch-review.yml` gate
    Comment,
    /// A real review and the `orch/review` check run from the GitHub App
    App,
}

impl ReviewMode {
    fn parse(value: Option<&str>) -> Option<Self> {
        match value.map(str::trim) {
            Some("app") => Some(Self::App),
            Some("comment") => Some(Self::Comment),
            Some(other) if !other.is_empty() => {
                tracing::warn!(
                    value = other,
                    "unknown review.mode, expected comment or app"
                );
                None
            }
            _ => None,
        }
    }
}

/// The first valid value wins, project before global
pub fn resolve(project: Option<&str>, global: Option<&str>) -> ReviewMode {
    ReviewMode::parse(project)
        .or_else(|| ReviewMode::parse(global))
        .unwrap_or(ReviewMode::Comment)
}

/// The mode of the project whose `gh.repo` is `repo`, the global value when no project matches
pub fn for_repo(repo: &str) -> ReviewMode {
    let project_dir = crate::config::get_projects_with_paths()
        .ok()
        .and_then(|projects| projects.into_iter().find(|(r, _)| r == repo))
        .map(|(_, dir)| dir);
    for_project_dir(project_dir.as_deref())
}

pub fn for_project_dir(project_dir: Option<&Path>) -> ReviewMode {
    let project =
        project_dir.and_then(|dir| crate::config::get_project_only(dir, "review.mode").ok());
    let global = crate::config::get_global("review.mode").ok();
    resolve(project.as_deref(), global.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_beats_global_and_comment_is_the_default() {
        assert_eq!(resolve(Some("app"), Some("comment")), ReviewMode::App);
        assert_eq!(resolve(Some("comment"), Some("app")), ReviewMode::Comment);
        assert_eq!(resolve(None, Some("app")), ReviewMode::App);
        assert_eq!(resolve(None, None), ReviewMode::Comment);
    }

    #[test]
    fn an_unknown_value_falls_through_to_the_next_source() {
        assert_eq!(resolve(Some("bogus"), Some("app")), ReviewMode::App);
        assert_eq!(resolve(Some(" app "), None), ReviewMode::App);
        assert_eq!(resolve(Some(""), None), ReviewMode::Comment);
    }

    #[test]
    fn the_project_file_is_read_without_the_global_fallback() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".orch.yml"), "review:\n  mode: app\n").unwrap();
        let value = crate::config::get_project_only(dir.path(), "review.mode").unwrap();
        assert_eq!(value, "app");

        let empty = tempfile::tempdir().unwrap();
        assert!(crate::config::get_project_only(empty.path(), "review.mode").is_err());
    }
}
