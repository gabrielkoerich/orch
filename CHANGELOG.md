# Changelog

## 2026-02-26 (cont.) — Model Map Fix, Polling Fallback & Cleanup

### Summary

Fixed the last open issue (#100), unstuck and merged the polling fallback PR (#131),
and closed all remaining open issues. Zero open issues, zero open PRs.

### Bug Fixes

- **Updated default model map** (#100, PR #130): Replaced placeholder/outdated model names:
  - Claude: `haiku` → `claude-haiku-4-5-20251001`, `sonnet` → `claude-sonnet-4-6`, `opus` → `claude-opus-4-6`
  - Codex: `gpt-5.x` placeholders → `o4-mini`, `gpt-4.1`, `o3`
  - OpenCode: `github-copilot/gpt-5.x` → `openai/gpt-4.1-mini`, `anthropic/claude-sonnet-4-6`, `anthropic/claude-opus-4-6`
  - Updated `pricing_for_model()` to match new model names

### Features

- **Polling fallback for webhooks** (#127, PR #131): When webhooks are disabled or fail,
  the engine automatically uses faster polling (30s instead of 120s):
  - Webhook health check pings local `/health` endpoint every 60s (with 5s timeout)
  - Automatic fallback mode switching with clear log messages
  - When webhooks disabled, starts in polling mode immediately
  - Configurable intervals via `engine.fallback_sync_interval` and `engine.webhook_health_check_interval`

### Review Fixes (PR #131)

- Removed `webhook_healthy` from `EngineConfig` (was runtime state on a config struct)
- Set `webhook_healthy = true` after server starts (prevents false fallback log)
- Added 5s timeout to health check HTTP request (prevents blocking engine loop)
- Removed redundant default assignments in `from_config()` else branches
- Updated AGENTS.md to accurately describe health check scope (local server only)

### Housekeeping

- Closed issue #126 (scheduled job marked done but still open)
- All 244 tests passing, zero clippy warnings, clean `cargo fmt`
- Zero open issues, zero open PRs

---

## 2026-02-26 — Per-Agent Runners, Error Mapping & PR Maintenance

### Summary

Implemented the AgentRunner trait system for per-agent output parsing and error classification,
enabling autonomous failover when agents fail. Also rebased, fixed, and pushed all 4 open PRs.

### Features

- **Per-agent runner trait** (#116): Each agent (Claude, Codex, OpenCode) now has its own parser:
  - `AgentRunner` trait with `build_command()`, `parse_response()`, `classify_error()`
  - `AgentError` enum (11 variants) for granular error classification
  - Claude runner: JSON envelope parser, extracts usage/permission_denials
  - Codex runner: NDJSON stream parser, detects turn.failed/error events
  - OpenCode runner: NDJSON parser, step_finish tokens, free model discovery
- **Unified PermissionRules**: Each agent translates to its native CLI flags
  - Claude: `--permission-mode bypassPermissions`, `--disallowedTools`
  - Codex: `--ask-for-approval never`, `--sandbox workspace-write`
  - OpenCode: worktree-level isolation (no native permission flags)
- **Model-level cooldowns**: 1-hour ban on agent+model combos that fail
- **Free model last resort**: When all agents exhausted, try OpenCode free models
- **Explicit git workflow in agent prompts**: Agents now instructed to commit, push, create PR
- **UTF-8-safe string truncation** in all error messages

### Bug Fixes (from PR review)

- Fixed async DB call without `.await` in `handle_failover()` — future was never polled
- Fixed metrics outcome: rerouted tasks no longer counted as "success"
- Fixed `sidecar::set` errors silently swallowed — now logged
- Removed dead `RetryableError` methods with inverted `is_exhausted` logic
- Skipped cooldown for `MissingTooling` errors (permanent, not transient)
- Fixed HashSet rebuilt per iteration in exhaustion check

### PR Fixes (from review comments on PR #120)

- Fixed rate limiting counter that never reset — now uses week-scoped keys
- Removed unused `_failed_tasks` DB query
- Removed dead `get_agent_complexity_performance_7d()` and `AgentComplexityStat`
- Added logging for silently dropped DB row parse failures
- Removed redundant `should_issue` parameter from detection functions
- Added issue deduplication via `has_open_issue_with_title()`

### PRs Rebased & Fixed (4 in session)

| PR | Title | Fixes Applied |
|----|-------|--------------|
| #125 | PR review integration | `cargo fmt`, clippy warnings |
| #124 | Unified notification system | `cargo fmt`, merge conflict resolution, missing `transport` arg |
| #122 | Agent memory across retries | `cargo fmt`, merge conflict in runner error handling |
| #120 | Self-improvement loop | `cargo fmt`, rate limit counter, dead code, dedup, review fixes |

### Tests

- **206+ tests** passing across all branches
- Added fixture-based tests for all 3 agent parsers (Claude, Codex, OpenCode)
- Test fixtures in `tests/fixtures/` with real agent output samples
- Zero `cargo clippy` warnings, clean `cargo fmt`

---

## 2026-02-25 / 2026-02-26 — Major Milestone: Full Rust Rewrite Complete

### Summary

Completed the full migration from bash scripts to native Rust. The `orch` binary now handles
the entire task lifecycle natively — from routing and agent invocation to git operations and
PR creation — with no bash scripts required. Additionally, 8 PRs from orchestrator agents
were reviewed, fixed, and merged in a single session.

### Features

- **Binary rename**: `orch-core` → `orch` — single binary, all subcommands native (#66)
- **Runner rewrite**: `run_task.sh` fully replaced by `src/engine/runner/` with 5 sub-modules:
  - `context.rs` — prompt context building
  - `worktree.rs` — git worktree management
  - `agent.rs` — agent command building (Claude, Codex, OpenCode, Kimi, MiniMax)
  - `response.rs` — error classification (timeout, rate limit, auth, missing tools)
  - `git_ops.rs` — auto-commit, push, PR creation
- **Config hot-reload** (#78): Engine picks up `.orchestrator.yml` / `config.yml` changes without restart
- **Multi-project support** (#82): `orch serve` handles multiple repos simultaneously
- **Channel implementations** (#81): Telegram, Discord, GitHub channels wired into engine
- **Security module** (#79): Secret scanning + redaction before posting comments to GitHub
- **State directory fix** (#80): Flattened `~/.orchestrator/.orchestrator/` → `~/.orchestrator/state/`
- **Directory rename** (#83): `~/.orchestrator/` → `~/.orch/` with backward-compatible fallbacks
- **Jobs consolidation**: Separate `jobs.yml` merged into `.orchestrator.yml`
- **Shell completions**: `orch completions bash/zsh/fish`

### CLI (All Justfile Recipes Ported)

```
orch serve | service start/stop/restart/status
orch task list/get/add/route/run/retry/unblock/attach/live/kill/publish
orch job list/add/remove/enable/disable/tick
orch init | agents | stream | log | version | config | sidecar | cron | template | parse
```

### Tests

- **118 tests** passing (up from 79 at start of session)
- Added unit tests for runner modules (branch naming, error classification, fallback agents)
- Clean `cargo clippy` and `cargo fmt`

### PRs Merged (8 in session)

| PR | Title | Author Agent |
|----|-------|-------------|
| #78 | Wire config hot-reload into engine | claude |
| #79 | Integrate security module into response posting | minimax |
| #80 | Flatten nested state directory | claude |
| #81 | Wire channel implementations into engine | opencode |
| #82 | Add multi-project support to engine serve loop | minimax |
| #83 | Rename ~/.orchestrator/ → ~/.orch/ | opencode |
| #84 | Polish PLAN.md and specs.md | claude (manual) |
| #85 | Update PLAN.md checklist | claude (manual) |

### Issues Created for Next Sprint

| Issue | Title | Assigned Agent |
|-------|-------|---------------|
| #86 | Weighted round-robin agent routing | claude |
| #87 | Token budget and cost tracking | codex |
| #88 | Axum webhook HTTP server for GitHub events | opencode |
| #89 | Cross-compile CI pipeline (macOS arm64 + x86_64) | kimi |
| #90 | Metrics and observability | minimax |

### Deleted

- `justfile` — all 34+ recipes ported to native CLI subcommands
