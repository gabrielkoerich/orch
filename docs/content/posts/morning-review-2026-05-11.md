+++
title = "Morning Review — 2026-05-11"
date = 2026-05-11
description = "Daily operational check-in: multi-agent degradation event observed yesterday; runner fixes merged; monitor kimi rate limits and closed-issue reconciliation timeouts."
+++

# Morning Review — 2026-05-11

## Recent Commits (last 24h)

| Hash | Message |
|------|---------|
| `14a3819d` | bug(ci): Zola docs build fails on malformed front matter — blocks release pipeline (#3098) |
| `5ab2ca69` | docs(posts): update evening retrospective 2026-05-10 with fixes, outcomes, and priorities (#3107) |
| `217d3b12` | Add PR-time Zola docs build check / front-matter linter (#3106) |
| `5f46759b` | fix(runner): glm exit-1 with cost telemetry in stdout classified as error (#3100) |
| `42e9883f` | fix(engine): bound and dedupe closed-issue reconciliation (#3099) |

## Operational Summary

Service activity: orch engine created several internal tasks at 10:00Z (morning-review, morning-briefing, twitter-trending-watch). Two recent fixes landed that reduce false failures caused by NDJSON envelope handling and auth-error extraction.

- Top operational observations:
  - Multi-agent degradation event occurred yesterday where multiple agents entered `agent_error` cooldowns (claude, opencode, kimi, glm). Pattern appears transient but unusual — monitor for recurrence.
  - Recent log noise: `timed out listing fallback tasks for closed-issue reconciliation` appears frequently in logs; investigate whether the cleanup listing query needs a higher timeout or index tuning.
  - Runner fixes (#3087/#3088) merged: NDJSON `terminal_reason:completed` handling and auth error extraction improved — this reduced false failure rates for codex and others.

## Task & Issue Snapshot

Orch task list highlights (current worktree):

| ID | Type | Status | Agent | Note |
|----|------|--------|-------|------|
| internal:149414 | internal | in_progress | opencode | This morning review (dispatched to opencode)
| internal:149337 | internal | blocked | minimax | Prior morning review attempt blocked on SSH/git fetch (owner action required)

Open GitHub issues (summary): see `gh issue list --state open` for full list; top operational issue remains #3087 which is addressed upstream.

## Health Checks

1. Stuck / failing tasks:
   - internal:149337 remains blocked due to `git fetch` SSH/agent signing failure (sign_and_send_pubkey: signing failed) — owner needs to address SSH agent or use HTTPS fallback for CI-driven operations.
   - Closed-issue reconciliation times out repeatedly (many `timed out listing fallback tasks for closed-issue reconciliation` WARN entries). This is likely a store query that needs bounding or indexing — track if it correlates with heavy store load.

2. Logs:
   - Recent orch.log shows repeated cleanup listing timeouts (every 5s window). These are WARNs but frequent — investigate query plan or increase timeout.
   - /opt/homebrew/var/log/orch.error.log is empty (0 bytes) as of May 10 19:17 — nothing actionable there.

3. task_runs summary (last 24h):

```
minimax|opus|success|40
codex|gpt-5.3-codex|success|16
kimi|opus|success|12
opencode|github-copilot/gpt-5-mini|success|11
opencode|github-copilot/claude-sonnet-4.6|success|6
glm|opus|success|4
claude|sonnet|failed|3
kimi|opus|failed|3
codex|gpt-5.3-codex|failed|2
kimi|opus|rate_limit|2
```

Notable: kimi failure/retry patterns and a small number of auth-related router LLM failures (401) observed during routing of some internal tasks.

## Retro Follow-ups

- #3087 (kimi exit-1 false failures) — fixed upstream and closed; verify reduction in false failures over next 24h.
- Closed-issue reconciliation timeouts — observed repeatedly in logs. Follow-up: consider adding an index or increasing the timeout for the listing queries if timeouts persist under load.

## Priorities for Today

1. Monitor task_runs and cooldown expirations for the next 24h to ensure the multi-agent degradation does not recur. If it repeats, collect timestamps and kv cooldown keys and open a diagnosis issue.
2. Owner action: fix SSH agent signing for the blocked worktree (internal:149337) or switch to HTTPS remote for automated runners.
3. Investigate the frequent cleanup listing timeouts: confirm query plan, consider increasing timeout, or add index to the relevant tables.
4. Confirm NDJSON/auth fixes reduced false failure counts (sample `task_runs` and `task_activity` events).

---

Prepared by Orch automation (internal task internal:149414, attempt 1).
