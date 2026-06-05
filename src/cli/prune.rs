use crate::engine::cleanup::cleanup_orphaned_worktree;
use std::path::Path;
use std::sync::Arc;

/// Remove orphaned worktrees — directories under `~/.orch/worktrees/` that are
/// not referenced by any task in the store.
///
/// Unlike `orch service doctor --fix`, this command does not require `--fix` to be
/// specified and does not prompt for confirmation. Orphaned worktrees are
/// unreferenced by definition and safe to remove automatically.
pub async fn run(dry_run: bool) -> anyhow::Result<()> {
    let store = Arc::new(crate::cli::init_store().await?);
    let all_tasks = store.list_all_global().await?;

    let worktrees_dir = crate::home::worktrees_dir()?;

    // Build a set of owned worktree paths for O(1) lookup.
    let owned: std::collections::HashSet<String> = all_tasks
        .iter()
        .filter(|t| !t.worktree.is_empty())
        .map(|t| t.worktree.clone())
        .collect();

    let mut removed = 0usize;
    let mut failed = 0usize;

    let project_dirs = match std::fs::read_dir(&worktrees_dir) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("cannot read worktrees directory: {e}");
            return Ok(());
        }
    };

    for project_entry in project_dirs {
        let project_entry = match project_entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("error reading worktrees directory entry: {e}");
                continue;
            }
        };
        if !project_entry
            .file_type()
            .map(|t| t.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        let branch_dirs = match std::fs::read_dir(project_entry.path()) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for branch_entry in branch_dirs {
            let branch_entry = match branch_entry {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("error reading project directory entry: {e}");
                    continue;
                }
            };
            if !branch_entry
                .file_type()
                .map(|t| t.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let wt_path = branch_entry.path().to_string_lossy().to_string();
            if owned.contains(&wt_path) {
                continue;
            }

            if dry_run {
                println!("would remove: {}", wt_path);
                removed += 1;
                continue;
            }

            let ok = cleanup_orphaned_worktree("prune", Path::new(&wt_path)).await;
            if ok {
                println!("removed: {}", wt_path);
                removed += 1;
            } else {
                eprintln!("failed to remove: {}", wt_path);
                failed += 1;
            }
        }
    }

    if removed == 0 && failed == 0 {
        println!("No orphaned worktrees found.");
        return Ok(());
    }

    if dry_run {
        println!(
            "\n{} orphaned worktree(s) would be removed (dry run)",
            removed
        );
    } else {
        println!(
            "\nRemoved {} orphaned worktree(s){}",
            removed,
            if failed > 0 {
                format!(", {} failed", failed)
            } else {
                String::new()
            }
        );
    }

    Ok(())
}
