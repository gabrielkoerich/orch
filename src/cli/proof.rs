use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::Context;
use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

use crate::cli;
use crate::cmd::SyncCommandErrorContext;
use crate::config;

const FOOTER_MARKER: &str = "Created by ";
const FOOTER_BOT: &str = "[bot] via [Orch]";

#[derive(Deserialize)]
struct GhPr {
    number: u64,
    body: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: DateTime<Utc>,
    #[serde(rename = "mergedAt")]
    merged_at: Option<DateTime<Utc>>,
    #[serde(rename = "closingIssuesReferences", default)]
    closing_issues: Vec<GhIssueRef>,
}

#[derive(Deserialize)]
struct GhIssueRef {
    number: u64,
}

#[derive(Deserialize)]
struct GhIssue {
    number: u64,
    #[serde(rename = "createdAt")]
    created_at: DateTime<Utc>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Proof {
    pub repo: String,
    pub generated_at: DateTime<Utc>,
    pub days: Vec<ProofDay>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct ProofDay {
    pub date: String,
    /// Every PR merged that day, the figure `gh pr list --state merged` shows
    pub merged: usize,
    pub merged_by_orch: usize,
    pub prs: Vec<ProofPr>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct ProofPr {
    pub number: u64,
    pub agent: String,
    pub model: Option<String>,
    pub merged_at: DateTime<Utc>,
    pub issue: Option<u64>,
    /// Starts at the issue creation, or at the PR creation when the PR closes no issue
    pub hours_to_merge: f64,
}

/// Read the agent and model from the footer orch writes on every PR body
fn parse_footer(body: &str) -> Option<(String, Option<String>)> {
    let line = body.lines().rev().find(|l| l.contains(FOOTER_BOT))?;
    let after_marker = &line[line.find(FOOTER_MARKER)? + FOOTER_MARKER.len()..];
    let agent = after_marker[..after_marker.find(FOOTER_BOT)?].to_string();
    if agent.is_empty() {
        return None;
    }
    let tail = &line[line.find(FOOTER_BOT)? + FOOTER_BOT.len()..];
    let model = tail
        .split('`')
        .nth(1)
        .filter(|m| !m.is_empty())
        .map(str::to_string);
    Some((agent, model))
}

/// Group the merged PRs by UTC day of the merge
fn build_proof(
    repo: &str,
    now: DateTime<Utc>,
    since: DateTime<Utc>,
    prs: Vec<GhPr>,
    issues: &[GhIssue],
) -> Proof {
    let issue_created: HashMap<u64, DateTime<Utc>> =
        issues.iter().map(|i| (i.number, i.created_at)).collect();
    let mut days: BTreeMap<String, ProofDay> = BTreeMap::new();

    for pr in prs {
        let Some(merged_at) = pr.merged_at else {
            continue;
        };
        if merged_at < since || merged_at > now {
            continue;
        }
        let date = merged_at.format("%Y-%m-%d").to_string();
        let day = days.entry(date.clone()).or_insert_with(|| ProofDay {
            date,
            merged: 0,
            merged_by_orch: 0,
            prs: Vec::new(),
        });
        day.merged += 1;

        let Some((agent, model)) = pr.body.as_deref().and_then(parse_footer) else {
            continue;
        };
        let issue = pr
            .closing_issues
            .iter()
            .map(|i| i.number)
            .find(|n| issue_created.contains_key(n));
        let start = issue
            .and_then(|n| issue_created.get(&n).copied())
            .unwrap_or(pr.created_at);
        #[expect(
            clippy::cast_precision_loss,
            reason = "a duration in seconds far below 2^52"
        )]
        let hours_to_merge =
            ((merged_at - start).num_seconds().max(0) as f64 / 3600.0 * 100.0).round() / 100.0;
        day.merged_by_orch += 1;
        day.prs.push(ProofPr {
            number: pr.number,
            agent,
            model,
            merged_at,
            issue,
            hours_to_merge,
        });
    }

    let mut days: Vec<ProofDay> = days.into_values().collect();
    for day in &mut days {
        day.prs.sort_by_key(|p| p.number);
    }
    Proof {
        repo: repo.to_string(),
        generated_at: now,
        days,
    }
}

fn gh_json<T: for<'de> Deserialize<'de>>(args: &[&str]) -> anyhow::Result<T> {
    let output = Command::new("gh").args(args).output_with_context()?;
    if !output.status.success() {
        anyhow::bail!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout).context("gh printed JSON that does not parse")
}

/// Write the merged PRs of a repository per day, the agent of each and the time to merge
pub fn export(repo: Option<String>, since: &str, out: Option<&Path>) -> anyhow::Result<()> {
    let repo = match repo {
        Some(r) => r,
        None => config::get_current_repo()?,
    };
    let hours = cli::parse_since(since)
        .ok_or_else(|| anyhow::anyhow!("unrecognised --since value {since:?}"))?;
    let now = Utc::now();
    let since_at = now - Duration::hours(i64::from(hours));

    let prs: Vec<GhPr> = gh_json(&[
        "pr",
        "list",
        "--repo",
        &repo,
        "--state",
        "merged",
        "--limit",
        "5000",
        "--json",
        "number,body,createdAt,mergedAt,closingIssuesReferences",
    ])?;
    let issues: Vec<GhIssue> = gh_json(&[
        "issue",
        "list",
        "--repo",
        &repo,
        "--state",
        "all",
        "--limit",
        "10000",
        "--json",
        "number,createdAt",
    ])?;

    let proof = build_proof(&repo, now, since_at, prs, &issues);
    let json = serde_json::to_string_pretty(&proof)?;
    match out {
        Some(path) => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, format!("{json}\n"))?;
        }
        None => println!("{json}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn pr(number: u64, body: &str, created: &str, merged: &str, issue: Option<u64>) -> GhPr {
        GhPr {
            number,
            body: Some(body.to_string()),
            created_at: at(created),
            merged_at: Some(at(merged)),
            closing_issues: issue
                .into_iter()
                .map(|number| GhIssueRef { number })
                .collect(),
        }
    }

    const FOOTER: &str = "text\n\n---\n*Task #1 · Created by claude[bot] via [Orch](https://github.com/x/y) using `sonnet`*";

    #[test]
    fn footer_gives_agent_and_model() {
        assert_eq!(
            parse_footer(FOOTER),
            Some(("claude".to_string(), Some("sonnet".to_string())))
        );
        let no_model = "*Task #1 · Created by codex[bot] via [Orch](https://x)*";
        assert_eq!(parse_footer(no_model), Some(("codex".to_string(), None)));
        assert_eq!(parse_footer("a human wrote this"), None);
    }

    #[test]
    fn groups_by_day_and_counts_every_merged_pr() {
        let prs = vec![
            pr(
                1,
                FOOTER,
                "2026-11-05T10:00:00Z",
                "2026-11-05T12:00:00Z",
                Some(7),
            ),
            pr(
                2,
                "human",
                "2026-11-05T10:00:00Z",
                "2026-11-05T13:00:00Z",
                None,
            ),
            pr(
                3,
                FOOTER,
                "2026-11-06T01:00:00Z",
                "2026-11-06T02:30:00Z",
                None,
            ),
            pr(
                4,
                FOOTER,
                "2026-10-01T01:00:00Z",
                "2026-10-01T02:00:00Z",
                None,
            ),
        ];
        let issues = [GhIssue {
            number: 7,
            created_at: at("2026-11-05T06:00:00Z"),
        }];
        let proof = build_proof(
            "o/r",
            at("2026-11-06T12:00:00Z"),
            at("2026-11-01T00:00:00Z"),
            prs,
            &issues,
        );
        assert_eq!(proof.days.len(), 2);
        let d5 = &proof.days[0];
        assert_eq!(
            (d5.date.as_str(), d5.merged, d5.merged_by_orch),
            ("2026-11-05", 2, 1)
        );
        assert_eq!(d5.prs[0].issue, Some(7));
        assert!((d5.prs[0].hours_to_merge - 6.0).abs() < f64::EPSILON);
        let d6 = &proof.days[1];
        assert_eq!((d6.merged, d6.merged_by_orch), (1, 1));
        assert!((d6.prs[0].hours_to_merge - 1.5).abs() < f64::EPSILON);
    }
}
