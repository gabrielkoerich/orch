use crate::backends::Status;
use crate::config;
use crate::sidecar;
use crate::tmux::TmuxManager;
use anyhow::Context;
use chrono::{DateTime, Duration, Utc};

/// Simple dashboard command combining task status, active sessions, and recent activity.
pub async fn dashboard() -> anyhow::Result<()> {
    // Tasks summary (uses external backend)
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;

    let repo = config::get_current_repo()
        .with_context(|| "'repo' not set — ensure .orch.yml has gh.repo")?;
    let backend: Box<dyn ExternalBackend> = Box::new(GitHubBackend::new(repo.clone()));

    let statuses = [
        Status::New,
        Status::Routed,
        Status::InProgress,
        Status::InReview,
        Status::Done,
        Status::NeedsReview,
        Status::Blocked,
    ];

    let mut counts: Vec<(Status, usize)> = Vec::new();
    let mut total = 0usize;
    for s in &statuses {
        let list = backend.list_by_status(*s).await?;
        total += list.len();
        counts.push((*s, list.len()));
    }

    println!("Tasks ({} total)", total);

    for (s, count) in &counts {
        println!(
            "  {:<12} {:>3} ",
            s.as_label().replace("status:", ""),
            count
        );
    }

    println!("\nActive Sessions");
    let tmux = TmuxManager::new();
    let sessions = tmux.list_sessions().await.unwrap_or_default();
    for s in sessions.iter() {
        // Try to read agent and title from sidecar using task id
        let agent = sidecar::get(&s.task_id, "agent").unwrap_or_default();
        let title = sidecar::get(&s.task_id, "title").unwrap_or_default();
        // Age
        let age = Utc::now() - s.created_at;
        let mins = age.num_minutes();
        println!(
            "  {:<25} {:<8} #{:<6} {}m ago",
            s.name, agent, s.task_id, mins
        );
    }

    println!("\nRecent (last 24h)");
    // Recent external tasks updated in last 24h — use metrics backend if available
    let recent = backend.list_by_status(Status::Done).await?;
    let cutoff: DateTime<Utc> = Utc::now() - Duration::hours(24);
    for r in recent.iter().take(10) {
        // parse updated_at RFC3339
        if let Ok(dt) = DateTime::parse_from_rfc3339(&r.updated_at) {
            let dt_utc = dt.with_timezone(&Utc);
            if dt_utc >= cutoff {
                let agent = sidecar::get(&r.id.0, "agent").unwrap_or_default();
                println!(
                    "  ✅ #{:<4} {:<30} {:<8} done {:>5} ago",
                    r.id.0, r.title, agent, ""
                );
            }
        }
    }

    Ok(())
}
