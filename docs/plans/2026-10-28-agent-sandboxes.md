# Agent sandboxes — Plan

Status: plan, 2026-10-28. Builds on the `sandbox-jail` crate ([repository](https://github.com/gabrielkoerich/sandbox-jail), 0.3.0 on crates.io, lume VMs in review), and replaces the earlier research note on Lume VMs (`docs/research/lume-macos-vm-sandboxing.md`) as the plan of record.

## What it is

Orch runs each task and review agent inside a sandbox the kernel enforces. Two modes, chosen by the operator:

| Mode | What runs the agent | What it isolates |
|------|---------------------|------------------|
| `os` | Seatbelt (`sandbox-exec`) on macOS, bubblewrap on Linux, around the agent process on the host | Files: the agent writes its worktree, its git dirs and `$HOME` minus a denylist, and cannot read keys |
| `vm` | A throwaway tart or lume clone of an operator-built image | Everything: own kernel, sees only the worktree, its git dirs and the attempt dir |

Orch builds a `sandbox_jail::Policy` for each run, and the crate enforces it or refuses it before the agent starts.

## Why

Isolation today is per agent CLI and uneven:

| Agent | Isolation today |
|-------|-----------------|
| claude, kimi, minimax | `--disallowedTools` patterns (`Bash(rm *)`, `Read(<main dir>/*)`). They match the command text, so `bash -c`, `python` or `/bin/rm` gets past them |
| codex | Its own Seatbelt, `--sandbox workspace-write`, plus `--add-dir` for the git dirs |
| grok | `--sandbox off` for tasks, because its CLI cannot add the git dir |
| opencode | None, it runs with `--dangerously-skip-permissions` and ignores the deny list |

One policy, enforced by the kernel, gives every agent the same limits.

## Where it plugs in

Every task and review agent starts in `build_tmux_runner_scripts()` (`src/engine/runner/agent.rs:271`), which writes `agent_cmd.sh` (line 290):

```bash
{agent_cmd} 2>{stderr_file}
```

`os` mode puts `Sandbox::argv()` in front of it:

```bash
{argv...} /bin/bash -c {agent_cmd} 2>{stderr_file}
```

`vm` mode runs it through the clone instead, with `Vm::command`: `tart exec` or `lume ssh` into the guest, from the worktree. lume passes no stdin, which the agent command does not need, since it reads its prompt with a `<` redirect that runs in the guest.

The redirect, `script`, `tee` and tmux stay outside the sandbox in both modes, so `output.json`, `stderr.txt` and `exit.txt` need no rule. `review.rs` uses `spawn_in_tmux` too and gets the sandbox with no extra work. The control session and the router (`direct.rs`, `control.rs`) stay outside it until phase 4.

## `os` mode

### Policy

| Field | Value |
|-------|-------|
| `writable` | The worktree, its `--git-dir` and `--git-common-dir` (`resolve_git_dir_arg()`, line 196), `$TMPDIR`, `/private/tmp`, `$HOME` |
| `read_only` | In the git common dir, `hooks`, `config`, `info`, `modules`, `packed-refs`, `refs/tags` and every branch ref but the task's own, including branches only `packed-refs` holds. The worktree git dir's `config.worktree`. The main checkout of every configured project, except its `.git`. Every entry of `~/.orch` and of `~/.orch/worktrees` but the task's own worktree and git dir. `~/Library/LaunchAgents`, `~/.local/bin`, `~/bin`. The shell and git startup files. Every dotfile at the top of the home but `.claude.json*`, `~/.config` but `~/.config/opencode`, every `PATH` dir inside the home, and the agents' settings, hooks and instructions under `~/.claude` and `~/.codex` |
| `hidden` | `sandbox_jail::known::credentials($HOME)` but cargo's config, and `~/.gnupg` |
| `unix_sockets` | The worktree and `$TMPDIR` when it is not `/tmp`. tmux's socket is in `/tmp`, Docker's and the ssh-agent's in the home. On Linux, `/tmp`, `/run` and `/var/tmp` are empty scratch dirs, since bwrap has no socket filter |
| `mach_services` | `sandbox_jail::known::BASIC_MACH_SERVICES` and `com.apple.SecurityServer` for the keychain. LaunchServices is out, so `open` cannot start an app outside |
| Everything else | The default: readable, network on |

- `$HOME` writable covers agent state (`~/.claude`, `~/.codex`, `~/.local/share/opencode`) and every language's caches without naming any.
- A hook or `core.hooksPath` written to the git common dir runs outside the sandbox the next time the operator runs git. Startup files, `~/.local/bin` and `LaunchAgents` run later as the operator.
- `read_only` wins over `writable`, so `~/.orch` cannot be one rule with the worktree as an exception. Orch lists the entries and leaves out the task's own.
- `~/.orch` stays readable. Job prompts read `orch.db` and `~/.orch/state/*/tasks/*/attempts/`, and agents read the template and the main checkout.
- Job agents run `orch task list` and `orch task runs`, which write `orch.db-wal` and `orch.db-shm`. Internal (job) tasks get `orch.db*` writable, GitHub-issue tasks do not.
- Claude's login is in the login keychain, which macOS reads in the client process, so `~/Library/Keychains` stays readable. Hiding it logged Claude out in a probe on 2026-10-28. Each item keeps its own access list.
- The home stays writable for caches, and a denylist misses what it does not name: a toolchain under `~/.rustup` or `~/.nvm` runs later as the operator. Closing that needs a read-only home with per-language cache paths, which #3674 ruled out.

The crate keeps a renamed parent from moving a ruled path out from under its rule, so an agent cannot `mv ~/.config` to get at `~/.config/git`. The cost is that the agent cannot rename or `chmod` `$HOME` itself.

### Agents

| Agent | Change |
|-------|--------|
| codex | Seatbelt cannot nest, so `SandboxLevel::Os` maps to `--dangerously-bypass-approvals-and-sandbox`. Its `--add-dir` handling and the shipcheck rule escape stop mattering |
| grok | None, task runs already use `--sandbox off` |
| claude, kimi, minimax | None. `--disallowedTools` stays as a second layer |
| opencode | None |

Probed on 2026-10-28 under this policy: `claude -p` edits and commits, `codex exec` reaches its API once `~/.codex` is writable. opencode hung on the test machine even without a sandbox, so it is unverified.

### Rejected: an allowlist of tool caches

Allowing only `~/.cargo`, `~/.npm`, `~/.bun` and so on is tighter, but it puts per-language logic in orch, which #3674 rejected, and it breaks the first time a project uses a tool not on the list.

## `vm` mode

The agent runs in a tart or lume clone of an image the operator builds. The clone shares the worktree and its git dirs read-write, and the attempt dir read-only for the prompt files, each at its host path. Nothing else of the host exists in the guest.

What the operator's image needs, since the guest sees no host toolchain and no host login:

- For tart, the Tart Guest Agent and passwordless sudo, as `ghcr.io/cirruslabs/ubuntu` and the `macos-*-base` images have. For lume, SSH and the `lume` user, as `ubuntu-noble-vanilla` and the `macos-*-vanilla` images have.
- Every agent CLI orch routes to, and the project toolchains.
- A login per agent that works without the host's keychain or home. Claude Code takes `CLAUDE_CODE_OAUTH_TOKEN` from `claude setup-token`. Codex reads `~/.codex/auth.json`. Orch passes these into the clone through the runner's environment, never into the image.

Lifecycle: orch boots the clone in `spawn_in_tmux`, keeps the `sandbox_jail::vm::Vm` handle for the task's run, and drops it when the run ends, which deletes the clone. A crash or a restart can leave clones behind, so startup reconciliation deletes every `sandbox-jail-*` clone that no live run owns.

Cost and limits: on an M4 Max a tart Linux clone answers in about 3 s and a macOS clone in about 20 s, a lume macOS clone in about 11 s and its Ubuntu image in about 2 minutes. Apple allows two macOS VMs at once per Mac, so macOS `vm` tasks run two at a time. The crate refuses `network = false` for a VM, so agents in a VM keep the network.

### Rejected: Docker

A container sees no macOS toolchain, needs the agents and their logins in its image just as a VM does, and shares the host kernel. Orch keeps Docker for what it already does: running untrusted PR code in `just review-pr`.

## Shipcheck inside the sandbox

An agent that runs `shipcheck run` or `shipcheck commit`, as `prompts/agent_system.md` tells it to, starts shipcheck's own Seatbelt jail inside orch's, which macOS refuses. In `vm` mode shipcheck is usually not in the image at all.

| Option | Cost |
|--------|------|
| A. With a sandbox on, the prompt drops `shipcheck run`. The pre-push hook runs the checks when orch pushes, outside the sandbox, and the daemon runs them on the PR head | The agent loses its loop of check, fix, commit. Failures come back a review cycle later |
| B. `shipcheck run` inside a sandbox asks the shipcheck daemon over a local socket to run the checks on the worktree, in shipcheck's own jail. The policy allows that one socket | A new daemon socket in shipcheck, which must accept only worktrees of installed repositories. Does not reach a VM |
| C. Orch runs `shipcheck run` outside the sandbox after the agent exits, and re-dispatches on failure | Same loop loss as A, and the shipcheck call moves into orch's runner |

Recommendation: A first, then B for `os` mode.

## Changes in orch

1. `sandbox-jail = "0.4"` in `Cargo.toml`. It has no runtime dependencies.
2. `SandboxLevel::Os`, set by `workflow.sandbox: os`, read per project so a project's `.orch.yml` can set `false`. Phase 3 adds `tart:<image>` and `lume:<image>` to the same key, as `sandbox-jail exec --vm` takes them. Opt-in. The operator sets it in `config.yml`. `workflow.permissions.sandbox` stays what it is, codex's own sandbox.
3. `spawn_in_tmux` builds the `Policy`. The git dir lookup runs for codex only today and moves out of that `if`.
4. `build_tmux_runner_scripts` writes the sandbox in front of the agent command: `Sandbox::argv()` for `os`, `Vm::command` for `vm`. A `vm` clone boots with `Vm::tart` or `Vm::lume`.
5. Codex maps `SandboxLevel::Os` and `Vm` to `--dangerously-bypass-approvals-and-sandbox`.
6. When the sandbox cannot start (`Backend::detect()` or `Vm::tart` or `Vm::lume` fails, or the crate refuses the policy), the task blocks with the error. It never falls back to running unsandboxed.
7. A sandbox denial blocks the task with the denied path in `block_reason`. It is a gap in the policy, so it must not cool down the agent or the model. `detect_permission_denied` already matches `operation not permitted`. The path comes from the agent's stderr or the Seatbelt log (`log show --predicate 'sender == "Sandbox"'`).
8. Startup reconciliation deletes orphaned `sandbox-jail-*` tart and lume clones.
9. Docs: `docs/content/agents.md` and `configuration.md` get both modes, the policy and how to read a denial.

## Phases

1. `os` mode for task and review agents: steps 1 to 6, with shipcheck option A.
2. Denial reporting (step 7). Run it on the operator's projects and widen the policy where real tasks hit it.
3. `vm` mode: steps 2, 4, 5 and 8 for `vm`, with one tart Linux image that carries claude and codex. lume works through the same config.
4. The control session and the router (`direct.rs`), with `orch.db` writable.
5. Shipcheck option B, once shipcheck has the daemon socket.

## Costs

- A `$HOME` denylist is weaker than an allowlist. A path that should be denied and is not on the list stays writable.
- The network stays open in both modes, so an agent can still send out anything it can read. In `os` mode `hidden` covers keys only.
- Codex loses its own sandbox. In `os` mode orch's policy is then the only layer.
- A denial blocks a task that ran before. Expect a few blocked tasks in phase 2 until the policy settles.
- `vm` mode needs an image per guest OS with every agent and toolchain, kept up to date by the operator, and an agent login that works without the host keychain.
- Apple marks `sandbox-exec` deprecated. Codex and Claude Code still use it. If it goes, `os` mode on macOS goes with it and `vm` mode is the fallback.

## Open items

- Verify that a Claude Code login from `CLAUDE_CODE_OAUTH_TOKEN` and a copied `~/.codex/auth.json` work in a clone.
- The tmux pane gets the agent's output through `script`, which needs a pseudo-terminal around `tart exec`. Check whether `tart exec -t` is needed for agents that switch to block buffering without one.
- Opencode is unverified in `os` mode.
