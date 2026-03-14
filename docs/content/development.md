+++
title = "Development"
description = "Contributing, testing, and release pipeline"
weight = 11
+++

## Setup

```bash
git clone https://github.com/gabrielkoerich/orch.git
cd orch
cargo build                                    # build orch binary
cargo nextest run                              # run tests (preferred, matches CI)
cargo clippy --all-targets -- -D warnings      # lint
```

Requires: Rust toolchain, `rg`, `fd`. Install nextest: `cargo binstall cargo-nextest`.

## Tests

Run unit and integration tests with `cargo nextest run` (preferred, matches CI) or `cargo test` as a fallback. Integration tests mock external agents and the `gh` CLI where appropriate.

## Required checks before committing

CI enforces all three -- run them locally before pushing:

```bash
cargo fmt -- --check                       # formatting
cargo clippy --all-targets -- -D warnings  # lints (warnings are errors, incl. test code)
cargo nextest run                          # tests
```

Or all at once:

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run
```

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
src/
  main.rs           — CLI entrypoint (clap)
  store.rs          — Unified SQLite task store (tasks, metrics, KV, rate limits)
  config/           — Config loading, hot-reload, multi-project
  engine/           — Core orchestration (tick, sync, review, cleanup, router, runner)
  github/           — GitHub API (HTTP client, token resolution, types, Projects V2)
  backends/         — External task backends (GitHub Issues)
  channels/         — Communication channels (Telegram, Discord, Slack, GitHub, tmux)
  cli/              — CLI command implementations
prompts/
  agent_system.md   — agent system prompt
  agent_message.md  — agent task message
  route.md          — routing prompt
  review_system.md  — review agent system prompt
  review_task.md    — review agent task prompt
docs/
  content/          — documentation pages (Zola site)
  templates/        — Zola HTML templates
  config.toml       — Zola config
```
