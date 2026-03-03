+++
title = "Development"
description = "Contributing, testing, and release pipeline"
weight = 11
+++

## Setup

```bash
git clone https://github.com/gabrielkoerich/orch.git
cd orch
cargo test          # run tests (Rust unit/integration tests)
just                # list available commands (project Make-like helper)
```

Requires: `yq`, `jq`, `just`, `python3`, `rg`, `fd`, `bats`.

## Tests

Run unit and integration tests with `cargo test`. Integration tests mock external agents and the `gh` CLI where appropriate. See `tests/` for fixtures and mocked responses.

## ShellCheck / security audit

CI runs `./bin/security-audit --strict`, which includes ShellCheck (`shellcheck -S error`) across shell-like scripts.

Bash treats backticks (`` `like this` ``) as command substitution inside double-quoted strings. If you want to print markdown that contains backticks (for example in CLI acknowledgements or issue comments), prefer single quotes, `printf`, or `$'...'` so the backticks are treated as literal characters (and ShellCheck won’t fail with parse errors like `SC1072` / `SC1073`).

## Release Pipeline

1. Push to `main` → CI runs tests and linters
2. CI auto-tags from conventional commits
3. CI generates changelog and creates GitHub release
4. CI updates `gabrielkoerich/homebrew-tap` Formula and publishes the Homebrew package
5. Users `brew upgrade orch` to install the new version

### Conventional Commits

Use prefixes in commit messages:
- `feat:` — new feature (bumps minor version)
- `fix:` — bug fix (bumps patch version)
- `chore:` — maintenance (no version bump)
- `docs:` — documentation (no version bump)

## Project Structure

```
scripts/
  lib.sh            — shared helpers (logging, locking, yq wrappers, GitHub API)
  backend.sh        — backend interface loader + jobs CRUD (YAML-backed)
  backend_github.sh — GitHub Issues backend implementation
  serve.sh          — main loop (poll, jobs, reviews)
  poll.sh           — finds and runs pending tasks
  run_task.sh       — runs a single task (route → agent → parse → push → PR)
  route_task.sh     — routes tasks via LLM
  add_task.sh       — creates tasks (GitHub issues)
  output.sh         — shared formatting (tables, sections)
  normalize_json.py — JSON extraction, tool history, token usage
  cron_match.py     — cron expression matcher
prompts/
  system.md         — system prompt
  agent.md          — execution prompt
  plan.md           — planning/decomposition prompt
  route.md          — routing prompt
  review.md         — review agent prompt
tests/
  orchestrator.bats — 200 bats tests
  gh_mock.sh        — comprehensive gh CLI mock
docs/
  content/          — documentation pages (Zola site)
  templates/        — Zola HTML templates
  config.toml       — Zola config
```
