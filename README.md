# orch

[![CI](https://github.com/gabrielkoerich/orch/actions/workflows/release.yml/badge.svg)](https://github.com/gabrielkoerich/orch/actions/workflows/release.yml)

**Write the plan. orch builds it.**

Coding agents can write the code, and a person still runs the loop around them: split the plan, start each session, paste the CI failure back, answer the review, rebase, merge, start the next one. The agents work while you sit there, and the quota you pay for resets whether you used it or not.

You write a markdown plan. orch turns it into GitHub issues and builds them in order, on your machine, with the agent CLIs on your PATH.

```
plan.md
   │
   ▼
orch plan          GitHub issues with blocked-by links
   │
   ▼
agent              own worktree, routed by quota
   │
   ▼
review agent       changes go back to the author
   │
   ▼
CI + merge         your branch protection
   │
   ▼
next unblocked task
```

## Install

```bash
brew tap gabrielkoerich/homebrew-tap
brew install orch
```

On Linux, install the prebuilt binary with [cargo-binstall](https://github.com/cargo-bins/cargo-binstall), or build from source with cargo:

```bash
cargo binstall orch-cli      # prebuilt x86_64 binary
cargo install orch-cli       # from source, needs libssl-dev and pkg-config
orch service install         # systemd user unit for `orch serve`
```

`orch service start|stop|restart|status` drives it through `systemctl --user`.

## Quick start

Log in to `gh` and one agent CLI, then:

```bash
orch init                 # probe agents, write config, start the service
orch plan PLAN.md         # turn a markdown plan into issues
```

`orch init` asks for one repository and writes `~/.orch/config.yml` with only the values that differ from the defaults. `--repo OWNER/REPO` skips the question.

The full walkthrough is in [Getting started](https://orch.gabrielkoerich.com/getting-started/). Features, the CLI, configuration, and the rest of the reference live in the [docs](https://orch.gabrielkoerich.com).

## License

MIT
