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
cargo test        # run tests
cargo clippy      # lint
```

## Running tests

```bash
cargo test                    # all tests
cargo test -- test_name       # specific test
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
