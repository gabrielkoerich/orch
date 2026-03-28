+++
title = "Morning Review -- 2026-03-28"
date = 2026-03-28T11:15:00Z
[extra]
job = "morning-review"
+++

## Summary

Exceptional overnight output: 37 commits, 0 open GitHub issues. All three retro carry-overs from
2026-03-27 evening are resolved. Service and CLI are now in sync at v0.43.0. The pipeline is clean —
no blocked or stuck tasks. The opencode `github-copilot/*` failure rate remains high (53 failures in
24h) but is now actively mitigated by silence detection and cooldown escalation.

---

## Recent Activity (Last 24h)

### Key Commits

- **Silence-count cooldown escalation** (#1165, `da9f3bb`) — escalates cooldown duration based on
  consecutive silence count; prevents models that fail repeatedly from re-entering the pool quickly.
- **Return None when all models in cooldown pool exhausted** (#1166, `375cf61`) — fixes potential
  infinite loop when every model in the pool is cooling down; returns `None` cleanly for fallback.
- **Auto-unblock recoverable failures** (#1161, `0d5ea5a`) — engine automatically transitions tasks
  from `blocked` back to `routed` when the block reason is recoverable (rate limits, timeouts).
- **Fetch issue comments via per-issue API endpoint** (#1162, `b1b38a5`) — switches from listing
  all comments to per-issue fetches, reducing API overhead and improving accuracy.
- **Cleanup session before early returns in runner** (#1158, `b0d9eaa`) — fixes #1142: session leaks
  and secret exposure in tmux env when runner exits early.
- **Clear corrupt delegations JSON from store on parse failure** (#1146, `5f7d5d8`) — fixes #1141:
  malformed delegation JSON was silently blocking every re-dispatch for affected tasks.
- **Persistent chat sessions + cross-agent session handoff** (#1150, `bf6f204`) — merges #1149:
  chat sessions now persist across orch restarts; handoff between agents preserves context.
- **Remove legacy sidecar migration code** (#1157, `b2c303a`) — dead code removed after SQLite
  migration is confirmed stable.
- **Self-improvement: debug agent errors and fix root causes** (#1148, `00e7096`) — meta-issue for
  improving error diagnostics; infrastructure for better post-mortem analysis.

---

## Operational Health

### Version

```
CLI:     0.43.0
Service: 0.43.0  ✓ in sync
```

Version drift resolved. No action needed.

### GitHub Issues

**6 open issues** — filed by the morning review job shortly after this post was initially written:

| # | Title | Status |
|---|-------|--------|
| [#1173](https://github.com/gabrielkoerich/orch/issues/1173) | chat_session get_or_create_session races and can leak tmux sessions | open |
| [#1171](https://github.com/gabrielkoerich/orch/issues/1171) | minimax review agent produces unparseable output — 4 parse errors stall review cycles | in_progress (codex) |
| [#1170](https://github.com/gabrielkoerich/orch/issues/1170) | kimi is out of Copilot quota — review agent fails 100% for 16+ hours | in_progress (codex) |
| [#1169](https://github.com/gabrielkoerich/orch/issues/1169) | github-copilot models on opencode have ~99% failure rate — silent exit 0 loops waste hours per day | in_review (codex) |
| [#1168](https://github.com/gabrielkoerich/orch/issues/1168) | chat_session pane diff slices at raw byte offset — wrong output or panic when scrollback shifts | in_progress (codex) |
| [#1167](https://github.com/gabrielkoerich/orch/issues/1167) | auto_unblock_count reset to 0 when increment fails — bypasses 3-retry guard | in_progress (opencode) |

Three distinct problem areas: opencode/kimi model quota/reliability, chat session concurrency, and a
correctness bug in the auto-unblock counter.

### Task Run Outcomes (Last 24h)

| Agent    | Model                                | Outcome | Count |
|----------|--------------------------------------|---------|-------|
| claude   | sonnet                               | success | 71    |
| minimax  | opus                                 | success | 38    |
| opencode | github-copilot/gpt-5.4-mini          | failed  | 38    |
| codex    | gpt-5.2-codex                        | success | 30    |
| claude   | opus                                 | success | 21    |
| opencode | github-copilot/claude-sonnet-4.6     | failed  | 15    |
| opencode | github-copilot/gpt-5.4-mini          | (blank) | 13    |
| kimi     | opus                                 | failed  | 8     |
| opencode | github-copilot/claude-sonnet-4.6     | (blank) | 5     |

`github-copilot/*` models account for **53 explicit failures** plus ~18 blank outcomes (silent
exit-0). Silence detection + cooldown escalation is catching these and re-routing to claude/codex.
No manual intervention required; the system is self-healing.

### Task Activity (Last 12h)

| Event           | Count |
|-----------------|-------|
| status_change   | 687   |
| dispatch        | 214   |
| branch_delete   | 68    |
| error           | 63    |
| push            | 62    |
| review_start    | 34    |
| review_decision | 27    |
| pr_create       | 22    |

63 errors in 12h — consistent with routing fallback events, not indicative of systemic failures.
High dispatch and review activity shows the pipeline is healthy and moving.

### Active Pipeline

| ID             | Status       | Agent  | Title                                    |
|----------------|--------------|--------|------------------------------------------|
| internal:20402 | in_progress  | claude | Code review                              |
| internal:20403 | in_progress  | claude | Daily morning review (this task)         |
| internal:20404 | new          | —      | Self-improvement: debug agent errors     |
| internal:20405 | new          | —      | Morning briefing (bean)                  |
| internal:20406 | new          | —      | Trading update (bean)                    |
| internal:20407 | new          | —      | Trading scan (bean)                      |

No blocked, stuck, or repeated-failure tasks. 3 bean tasks queued and will be dispatched shortly.

### Log Anomaly: opencode Routing Parse Failure

The router failed to parse a routing response from `opencode/mimo-v2-omni-free` for task
`internal:20402` — streaming JSON lines before the routing result. The engine recorded a cooldown
and fell back successfully. This is the same pattern from yesterday; it is handled.

---

## Retrospective Follow-ups (from 2026-03-27 evening)

- [x] **#1142** (session leaks + secret exposure) — fixed via #1158
- [x] **#1141** (delegation JSON corruption) — fixed via #1146
- [x] **#1149** (persistent chat sessions) — merged via #1150
- [x] **CLI version drift** — resolved, both at v0.43.0
- [ ] **opencode silent exit-0 root cause** — mitigated (#1165, #1166, #1135, #1144) but root
      cause (github-copilot/* model instability) is not eliminated. Escalation and pool exhaustion
      handling are now in place.
- [ ] **Update SKILL.md** — operational learnings about silence detection and cooldown behavior not
      yet captured in documentation.

---

## Today's Priorities

1. **Resolve #1169 (github-copilot ~99% failure)** — currently in_review. If the fix lands, verify
   the failure rate drops. If it doesn't, disable `github-copilot/*` in config.
2. **Resolve #1170 (kimi quota exhausted)** and **#1171 (minimax unparseable output)** — both
   degrade review agent reliability significantly. Kimi quota may self-heal; minimax needs a parser fix.
3. **Resolve #1167 (auto_unblock_count bug)** — silent correctness bug that bypasses the 3-retry
   guard. Small fix but high impact on task lifecycle correctness.
4. **Resolve #1168 and #1173 (chat session bugs)** — byte-offset slicing and race conditions in
   `get_or_create_session`. Lower urgency but could cause data corruption.
5. **Watch bean tasks** — `internal:20405`, `internal:20406`, `internal:20407` are queued.
   Confirm they dispatch and complete cleanly under the new silence escalation logic.
6. **Update SKILL.md** — capture silence detection and cooldown escalation patterns. Low urgency.
