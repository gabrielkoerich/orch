use crate::backends::Status;
use crate::cli::init_task_manager;
use crate::db::TaskStatus;
use crate::sidecar;
use crate::tmux::TmuxManager;
use chrono::{DateTime, Duration, Utc};

/// Simple dashboard command combining task status, active sessions, and recent activity.
pub async fn dashboard() -> anyhow::Result<()> {
    let task_manager = init_task_manager().await?;

    let statuses = [
        Status::New,
        Status::Routed,
        Status::InProgress,
        Status::InReview,
        Status::Done,
        Status::NeedsReview,
        Status::Blocked,
    ];

    // Fetch all tasks from both backends
    let all_external = task_manager.list_all_external_tasks().await?;
    let all_internal = task_manager.db.list_all_internal_tasks().await?;

    let mut counts: Vec<(Status, usize, usize)> = Vec::new(); // (status, external, internal)
    let mut total = 0usize;
    let mut done_tasks = Vec::new();
    for s in &statuses {
        let label = s.as_label();
        let ext_filtered: Vec<_> = all_external
            .iter()
            .filter(|t| t.labels.contains(&label.to_string()))
            .cloned()
            .collect();
        let ts = match s {
            Status::New => TaskStatus::New,
            Status::Routed => TaskStatus::Routed,
            Status::InProgress => TaskStatus::InProgress,
            Status::Done => TaskStatus::Done,
            Status::Blocked => TaskStatus::Blocked,
            Status::InReview => TaskStatus::InReview,
            Status::NeedsReview => TaskStatus::NeedsReview,
        };
        let int_count = all_internal.iter().filter(|t| t.status == ts).count();
        total += ext_filtered.len() + int_count;
        counts.push((*s, ext_filtered.len(), int_count));
        if *s == Status::Done {
            done_tasks = ext_filtered;
        }
    }

    println!("Tasks ({} total)", total);

    for (s, ext, int) in &counts {
        let count = ext + int;
        if count > 0 {
            println!(
                "  {:<12} {:>3}{}",
                s.as_label().replace("status:", ""),
                count,
                if *int > 0 {
                    format!(" ({} internal)", int)
                } else {
                    String::new()
                }
            );
        }
    }

    println!("\nActive Sessions");
    let tmux = TmuxManager::new();
    let sessions = tmux.list_sessions().await.unwrap_or_default();
    for s in sessions.iter() {
        let agent = sidecar::get(&s.task_id, "agent").unwrap_or_default();
        println!("  {:<25} {:<8} #{}", s.name, agent, s.task_id);
    }

    println!("\nRecent (last 24h)");
    let cutoff: DateTime<Utc> = Utc::now() - Duration::hours(24);
    for r in done_tasks.iter().take(10) {
        if let Ok(dt) = DateTime::parse_from_rfc3339(&r.updated_at) {
            let dt_utc = dt.with_timezone(&Utc);
            if dt_utc >= cutoff {
                let agent = sidecar::get(&r.id.0, "agent").unwrap_or_default();
                let elapsed = Utc::now() - dt_utc;
                let mins = elapsed.num_minutes();
                println!(
                    "  ✅ #{:<4} {:<30} {:<8} done {:>4}m ago",
                    r.id.0, r.title, agent, mins
                );
            }
        }
    }

    Ok(())
}
