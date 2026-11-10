+++
title = "Development"
description = "Contributing, testing, and release pipeline"
weight = 41
aliases = ["/development/"]

[extra]
group = "Operate"
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

CI enforces all four, run them locally before pushing:

```bash
cargo fmt -- --check                       # formatting
cargo clippy --all-targets -- -D warnings  # lints (warnings are errors, incl. test code)
cargo nextest run                          # tests
DYLINT_RUSTFLAGS="-D warnings" cargo dylint --all -- --all-targets  # use_in_fn lint
```

Install dylint with `cargo +stable install cargo-dylint dylint-link --version 6.3.2 --locked`. The pinned 1.94.1 is too old to build cargo-dylint.

## Import rules

Every import is a `use` at the top of its module. Lint rules enforce it, because agents copy the style they see and an instruction alone did not stop inline paths from spreading.

| Rule | Lint |
|------|------|
| No crate-rooted path of more than two segments in code (`crate::db::Store`, `std::fs::read`) | `clippy::absolute_paths` with `absolute-paths-max-segments = 2` |
| `mod` declarations, then `use`, then all other items | `clippy::arbitrary_source_item_ordering`, module groups in `clippy.toml`, no alphabetical order inside a group |
| No `use` after a statement | `clippy::items_after_statements` (pedantic) |
| No `use` inside a function, method or closure body | `use_in_fn`, a [dylint](https://github.com/trailofbits/dylint) library loaded from [dylint-lints](https://github.com/gabrielkoerich/dylint-lints) at a tag, set in `dylint.toml` |

No Clippy or rustc lint catches a `use` at the start of a body, so `use_in_fn` fills that gap. It is an early lint pass over the expanded AST, so it also flags a `use` that a local `macro_rules!` expands into a body. It skips macros from other crates. A test that greps the source was rejected: it is not a lint, and it misses macro output.

`arbitrary_source_item_ordering` skips `#[cfg(test)]` modules, so a test module stays at the bottom of its file. Any other inline module goes in its own file and is declared at the top.

Costs:

- The lint library builds with the nightly that `dylint_linting` 6.3 needs, pinned in `rust-toolchain.toml` of the dylint-lints repository and in `release.yml`. A nightly bump is one release there and a new tag here, and rustc's internal API can break the lint on the way.
- Each CI run of `check` installs that nightly with `rustc-dev`, builds the dylint driver and the lint library, and runs one more `cargo check` of orch on the nightly. A rustc warning that only the nightly knows also fails that step.

Or all at once:

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run
```

## Release Pipeline

A push to `main` runs `.github/workflows/release.yml`:

1. `check`, `test` and `secrets` run formatting, clippy, the docs build, the test suite and gitleaks
2. `check-release` picks the next version from the commit subjects, or skips the release
3. `build-macos` builds, signs and notarizes the arm64 binary, and `build-linux` builds the `x86_64-unknown-linux-gnu` binary
4. `release` then runs these steps in order: create and push the tag, build the changelog, create the GitHub release, publish the docs for that tag, update the `gabrielkoerich/homebrew-tap` formula, publish `orch-cli` to crates.io, and push the version bump commit to `main` last

The bump commit is `chore: bump version to vX.Y.Z [skip ci]`, so its push starts no new run.

### Conventional Commits

Use prefixes in commit messages:
- `feat:` bumps the minor version
- `BREAKING CHANGE:` or `breaking:` bumps the major version
- any other subject bumps the patch version
- `chore:` and `docs:` alone make no release

If all commits since the last tag are `chore:`/`docs:` only, the release is skipped. Otherwise the highest bump in the range wins.

## Project Structure

```
src/
  main.rs           — CLI entrypoint (clap)
  store/            — Unified SQLite task store (tasks, metrics, KV, rate limits)
  config/           — Config loading, hot-reload, multi-project
  engine/           — Core orchestration (tick, sync, review, cleanup, router, runner)
  github/           — GitHub API (HTTP client, token resolution, types, Projects V2)
  backends/         — External task backends (GitHub Issues)
  channels/         — Communication channels (Telegram, Discord, Slack, GitHub, tmux)
  cluster/          — Multi-machine cluster (Raft)
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
