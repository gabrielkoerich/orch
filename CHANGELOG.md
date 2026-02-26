# Changelog

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
