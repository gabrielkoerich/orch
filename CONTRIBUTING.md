# Contributing

Thanks for contributing to Orch!

## Development setup

Prereqs (macOS/Homebrew):

```bash
brew install just ripgrep fd
# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Clone + sanity check:

```bash
git clone https://github.com/gabrielkoerich/orch.git
cd orch
cargo build       # build orch binary
cargo nextest run # run tests (preferred, matches CI)
cargo clippy --all-targets -- -D warnings  # lint (warnings are errors)
```

Install nextest: `cargo binstall cargo-nextest` (requires [cargo-binstall](https://github.com/cargo-bins/cargo-binstall)).

## Running tests

```bash
cargo nextest run              # all tests (preferred, matches CI)
cargo nextest run -E 'test(name)' # specific test by name
cargo test                     # fallback if nextest is not installed
cargo test -- test_name        # specific test (fallback)
```

## Required checks before committing

CI enforces all three — run them locally before pushing:

```bash
cargo fmt -- --check                       # formatting
cargo clippy --all-targets -- -D warnings  # lints (warnings are errors, incl. test code)
cargo nextest run                          # tests
```

Or all at once:

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run
```

## Commit message conventions

Use Conventional Commits:
- `feat:` new feature (minor bump)
- `fix:` bug fix (patch bump)
- `docs:` documentation only
- `chore:` maintenance/refactor

## PR workflow

- Branch from `main` and keep changes focused.
- Open a PR early; keep it small and easy to review.
- Prefer **squash merge** on GitHub.

```bash
git fetch origin
git checkout -b my-branch origin/main
git commit -m "docs: add contributing guide"
git push -u origin my-branch
```
