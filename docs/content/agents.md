+++
title = "Agents"
description = "Agentic mode, safety rules, and context enrichment"
weight = 5
+++

Once routed, agents run in full agentic mode with tool access:

- **Claude**: `-p` (non-interactive), `--permission-mode bypassPermissions` (autonomous) or `acceptEdits` (supervised), `--output-format json`, system prompt via `--append-system-prompt`
- **Codex**: `exec` subcommand with `--sandbox workspace-write -c 'approval_policy="never"'` (autonomous) or `--dangerously-bypass-approvals-and-sandbox` (full access); `-c 'sandbox_workspace_write.network_access=true'`; system+agent prompt combined
- **OpenCode**: `opencode run` with `--dangerously-skip-permissions` and combined prompt
- **Grok**: `grok --output-format streaming-messages-json`, `--permission-mode bypassPermissions` (autonomous) or `acceptEdits` (supervised), system prompt via `--system-prompt-override`, task prompt via `--prompt-file`. Task runs use `--sandbox off` because the workspace profile cannot write a worktree git dir that lives outside the cwd. Deny rules still block `rm` and `git push`.
- **Kimi / MiniMax**: spawned via their CLIs with the prompt as stdin; combined system+task prompt

Agents execute inside an isolated git worktree created for the task (not your main repo), so they can read project files, edit code, and run commands.

## tmux Runner

Agent sessions run inside tmux sessions. Orch spawns the agent CLI as the tmux session shell, so tmux provides the PTY. There is no external PTY process or `portable_pty` crate. Prompt files are written to per-attempt directories for auditability.

## Agent Output

The agent writes a JSON file to `~/.orch/state/{repo}/tasks/{id}/attempts/{n}/output.json`. Task metadata (branch, worktree, agent, model, attempts, memory, etc.) is stored in the unified SQLite database at `~/.orch/orch.db`. The expected agent output format:

```json
{
  "status": "done|in_progress|blocked|needs_review",
  "summary": "what was done",
  "reason": "why blocked/needs_review",
  "accomplished": ["list of completed items"],
  "remaining": ["list of remaining items"],
  "blockers": ["list of blockers"],
  "files_changed": ["list of modified files"],
  "needs_help": false,
  "delegations": [{"title": "...", "body": "...", "labels": [], "suggested_agent": "codex"}]
}
```

## PATH Configuration

When orch runs as a service (e.g. via `brew services`), agents start with a minimal PATH that may not include tools like `bun`, `anchor`, `cargo`, or `solana`. There are two ways to fix this:

**Option 1: Create `~/.path` (recommended)**

Create a `~/.path` file that exports your development tool paths:

```bash
# ~/.path
export PATH="/opt/homebrew/bin:$PATH"
export PATH="$HOME/.bun/bin:$PATH"
export PATH="$HOME/.cargo/bin:$PATH"
export PATH="$HOME/.local/bin:$PATH"
```

Orch sources this file before launching agents, so any tool on your PATH will be available to agents when the runner is configured to inherit the shell environment.

**Option 2: Default fallback**

If `~/.path` doesn't exist, orch automatically prepends these well-known directories (only when they exist on disk):

- `/opt/homebrew/bin`
- `/opt/homebrew/sbin`
- `/usr/local/bin`
- `$HOME/.cargo/bin`
- `$HOME/.local/bin`
- `$HOME/.bun/bin`

## Safety Rules

Agents are constrained by rules in the system prompt and by runner-enforced tool allowlists/denylists:

- **No `rm`**: `--disallowedTools` blocks `rm` — agents must use `trash` (macOS) or `trash-put` (Linux)
- **No commits to main**: agents must always work in feature branches
- **GitHub issue linking**: if a task has a linked issue, the agent receives the issue reference for branch naming and PR linking
- **Cost-conscious sub-agents**: agents are instructed to use cheap models for routine sub-agent work

## OS Sandbox

Task and review agents run inside Seatbelt on macOS or bubblewrap on Linux, through the [sandbox-jail](https://github.com/gabrielkoerich/sandbox-jail) crate. It is on by default. `workflow.sandbox: false` turns it off, in the global config for every project or in one project's `.orch.yml`. Any other value leaves it on, so a typo cannot run an agent without it. The chats have their own stricter policy, which this setting does not change.

| The agent | Paths |
|-----------|-------|
| May write | Its worktree. In the git dir, the objects, the task's branch, remote refs and the task's own worktree dir, `HEAD` included, since a rebase detaches it. Its own temp dir (`$TMPDIR`), which orch deletes when the run ends, and `/private/tmp`. The home's top-level dirs but `.config`, `.claude`, `.ssh`, `.gnupg`, `.passbox`, `bin`, `Library` and `.local`, and inside those `Library/Caches`, `Library/Logs`, `.local/share` and `.local/state`. Claude Code's session dir for the worktree, `~/.claude/projects/<worktree path>`, where it keeps command output too long for its context |
| May not write | Everything else, among it every file at the top of the home and any new one there, `~/.claude.json`, `~/.config`, the agents' settings, every `PATH` dir inside the home such as `~/.cargo/bin`, cargo's `config.toml` and gradle's init scripts, the git dir's hooks, config and other branches, the hooks and config of every other repository found up to four levels below a writable home dir when the policy is built, the worktree's `.git` file and `.orch.yml`, every main checkout but its `.git`, `~/.orch` but the task's own worktree (and `orch.db` for job tasks), tags, and shipcheck's state in `~/.local/state/shipcheck`, from which it posts a PR's checks |
| May not read | Credentials: `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.config/gh`, `~/.docker/config.json`, `.netrc`, `.npmrc` and the like, and the address and password of opencode's background service. The login keychain stays readable and `security` stays runnable, since Claude reads its login through them. An agent can read any item whose access list trusts `security`, as it can today without the sandbox |
| May not connect to | A unix socket outside the worktree and `$TMPDIR`: tmux's, Docker's, the ssh-agent's. The one exception is passbox's broker, `~/.passbox/broker.sock`, which asks the operator before it hands out a secret. On macOS, any service but the basic ones, the keychain's, certificate trust and the read-only LaunchServices database, so `open` cannot start an app outside the sandbox. A job task also reaches the calendar |
| May not run (macOS) | `launchctl` and setuid binaries such as `sudo` |

The agent's environment drops `TMUX`, `TMUX_PANE` and `SSH_AUTH_SOCK`. `gh` gets an empty `GH_CONFIG_DIR` in the run's temp dir and runs on `GH_TOKEN`, since its own config dir holds its logins and is hidden. `git fetch` takes no tags, because one tag it cannot write fails the whole fetch. The network stays open.

Seatbelt cannot nest, so every tool that starts a sandbox of its own needs it off:

| Tool | Inside the OS sandbox |
|------|-----------------------|
| codex | Runs with `--dangerously-bypass-approvals-and-sandbox`. With its own sandbox on, every command fails and codex still exits 0 |
| grok | Runs with `--sandbox off` |
| opencode | Runs with `--standalone` and `OPENCODE_PTY_SOCKET_DIR` in the run's temp dir. Without `--standalone`, `opencode run` is a client of a background service that runs every tool outside the sandbox |
| shipcheck | Hands `run`, `status`, `logs` and `wait` to its daemon, which runs them outside and posts the result on the push. orch sets `SHIPCHECK_HANDOFF=1` and asks `shipcheck handoff` whether the daemon watches the repository. With an older shipcheck, or a repository the daemon does not watch, the agent's prompt has it run the project's checks itself, the push hook runs shipcheck outside, and orch runs it before the review |
| `swift build`, `xcodebuild` | Need `--disable-sandbox`, or `-derivedDataPath` in the worktree with `-IDEPackageSupportDisableManifestSandbox=YES -IDEPackageSupportDisablePluginExecutionSandbox=YES`. The agent's prompt says so |
| A headless browser | `chrome-headless-shell --no-sandbox --single-process` runs. Brave and Chrome need LaunchServices, which would let an agent start an app outside, and do not start |

What the policy costs:

- `ps`, `pgrep` and `pkill` fail. `ps` would show the environment of every process of the user, secrets included. `kill` and `killall` work on the agent's own processes.
- `git config`, `git tag`, global installs (`cargo install`, `npm i -g`) and tools that write `~/.config` fail.
- `mktemp` with no template fails on macOS, where it ignores `TMPDIR` and uses the per-user temp dir. That dir is shared with every other process, so it stays closed. The agent's prompt says to pass a template under `$TMPDIR`.
- An agent can change or delete what other processes keep in `/private/tmp`. Only the tmux and launchd socket dirs there are read-only. Agents write `/tmp` paths by name too often to close it.
- A browser that runs outside the sandbox with a debugging port on localhost is in reach of every task, and through it every file the operator can read or write. The sandbox has no rule for one port yet.
- The passbox broker is in reach of every task. An approval window the operator opened, or a live grant, serves an agent with no new prompt.
- A new file at the top of a main checkout, or in a writable home dir, is writable. Only entries that exist when the policy is built are read-only.
- The policy is fixed when the run starts, from a scan of the home at most a minute old. The hooks and config of a repository cloned after that scan stay writable for that run.
- On Linux, a read-only path that does not exist when the run starts stays writable, since bwrap binds only existing paths. An agent can create `~/.cargo/config.toml`, gradle's `init.d` or the worktree's `.orch.yml`, and the next build the operator runs reads it.
- On Linux, bwrap has no socket filter. `/tmp`, `/run` and `/var/tmp` start empty, so tmux's, the ssh-agent's and rootless Docker's sockets are out of reach, but a socket in the home, such as Docker Desktop's, is not.
- On Linux, a `sandbox.write` path that does not exist is left out of the policy, since bwrap cannot bind it. Create it first.

### Package manager settings

`~/.npmrc`, `~/.config/uv` and `~/.config/pip` are hidden, since they can hold a registry token. So that an install inside the sandbox still follows the operator's rules, such as a minimum package age, orch writes a copy of each file without its credentials into the run's temp dir and points the tool at it with `NPM_CONFIG_USERCONFIG`, `UV_CONFIG_FILE` and `PIP_CONFIG_FILE`. `.npmrc` loses its credential lines. `uv.toml` and `pip.conf` are copied only when they hold no credential, since a dropped line can break them.

### More paths

The config adds paths to the policy:

```yaml
sandbox:
  write:
    - ~/.config/solana/install
  read:
    - ~/.pypirc
  sockets:
    - ~/.docker/run/docker.sock
```

| Key | Effect |
|-----|--------|
| `sandbox.write` | The agent may write the path. In a project's `.orch.yml` or the global config |
| `sandbox.read` | Shows a hidden credential path. Name the hidden path or a parent of it. Global config only |
| `sandbox.sockets` | The agent may connect to the unix socket, or to the sockets in the dir. Global config only |

A path is absolute or starts with `~/`. The `write` lists of the project and of the global config add up. orch reads the project's list from the main checkout, which the agent cannot write, so a change to it takes effect only once it is merged. A merged change must not hand later tasks a credential or Docker's socket, so `read` and `sockets` are the operator's alone.

A `write` entry is refused, with a warning in the log, when it is the home, a parent of it or an entry at its top, or when it is inside `~/.ssh`, `~/.gnupg`, `~/.passbox`, `~/.claude`, `~/.orch`, `~/.config/git`, `~/bin`, `~/.local/bin`, `~/Library/LaunchAgents` or `~/Library/Keychains`, in any case. The check guards against a mistake. A project's `.orch.yml` is trusted as a whole, since its `bash` jobs already run as the operator. No entry lifts a read-only rule: a path under `~/.cargo/bin` stays read-only.

When the sandbox cannot start, the task blocks with the reason and no agent runs. It counts against no agent or model. A path the agent needs and cannot write shows up as `Operation not permitted` in its output.

An install that upgrades to this version runs its next task in the sandbox with no config change. To run agents without it, set `workflow.sandbox: false` and run `orch task unblock all` for the tasks that blocked.

On Linux the sandbox needs `bwrap` and unprivileged user namespaces, and the block reason gives the fix, such as the AppArmor profile Ubuntu needs for bwrap. When orch runs in a Docker container, start the container with `--security-opt seccomp=unconfined --security-opt systempaths=unconfined`. Docker's seccomp profile stops bwrap from creating a user namespace, and its masked `/proc` paths stop bwrap from mounting `/proc`. On macOS, orch inside another Seatbelt sandbox cannot start one, since Seatbelt does not nest.

## Worktrees

Orch creates worktrees before launching agents. Agents do NOT create worktrees themselves.

**Path:** `~/.orch/worktrees/<project>/<branch>/` — worktrees are always placed under `~/.orch/worktrees` with a project/branch layout. Worktrees live in orch home directory (`ORCH_HOME`, default `~/.orch`) and are safe to remove by the cleanup process once branches are merged.

**Steps:**
1. `gh issue develop <issue> --base main --name <branch>` — registers branch with GitHub
2. `git branch <branch> main` — creates branch from main
3. `git worktree add <path> <branch>` — creates worktree
4. Agent runs inside the worktree directory

After an agent finishes, orch handles all git operations: pushing the branch, creating the PR, and linking it to the issue. Agents only commit — they do not push or create PRs. The runner injects `GH_TOKEN` into the environment for read-only operations (e.g., checking CI status), but agents should not call GitHub write APIs directly. Attribution footers (for example: `Created by claude[bot] via Orch`) are added to issue and PR comments so it's clear which agent produced the content.

## Context Enrichment

Every agent receives a rich context built from multiple sources:

| Context | Source | When |
|---------|--------|------|
| System prompt | `prompts/agent_system.md` | Always |
| Task details | SQLite store (`~/.orch/orch.db`) | Always |
| Agent profile | Router-generated role, tools, and constraints | Always |
| Error history | SQLite store | On retries |
| Last error | Task store `last_error` field | On retries |
| GitHub issue comments | GitHub API | If issue linked |
| Prior run context | `contexts/task-{id}.md` | On retries |
| Tool call summaries | `.orch/tools-{id}.json` | On retries |
| Repo tree | `git ls-files` | Always |
| Project instructions | `CLAUDE.md` + `AGENTS.md` + `README.md`, a file read once when one links to the other | If files exist |
| Parent/sibling context | Parent task summary + accomplished | If child task |
| Git diff | Uncommitted changes | On retries |

## Error Handling

When a task fails:
1. Error is recorded in `last_error` and `history`
2. The failed agent or model gets a cooldown, and the task is re-routed (see [Cooldowns](#cooldowns))
3. A structured comment is posted on the linked GitHub issue with an attribution footer (for example: `Created by claude[bot] via Orch`) so it's clear which agent produced the comment
4. After `workflow.max_attempts` (default 5) the task goes to `blocked`, and orch removes any forced `agent:*` label so `/retry` routes it freely

```bash
orch task retry <id>       # reset a task to new (a done task is refused)
orch task unblock <id>     # reset a blocked task to new
orch task unblock all      # reset all blocked tasks to new
```

## Cooldowns

A failed run puts the agent or the model in a cooldown, and the router skips it until the cooldown ends. Cooldowns are stored in SQLite and survive a restart.

| Failure | Cooldown |
|---------|----------|
| One-off model failure: empty or cut off output, an unparseable review, an opaque provider error | That model, 5 minutes, then 15 minutes |
| Model not found, not supported or region blocked | That model, 4 hours, growing to 7 days |
| Other model-unavailable error | That model, 5 minutes, growing to 4 hours |
| Billing cycle exhausted on a known model | That model, 4 hours, growing to 7 days |
| Generic agent failure | That agent, 5 minutes, growing to 4 hours |
| Out of credits | That agent, 1 hour, growing to 8 hours |
| Org-level disabled | That agent, 2 hours, growing to 8 hours |
| Billing cycle exhausted, model unknown | That agent, 24 hours, growing to 7 days |
| Rate limit with a "try again at" time | Until that time |
| Silence | That model for `engine.silence_cooldown` (1 hour, 4 hours after 5 silences in 24 hours), and the agent for 120 seconds |
| Review timeout | None, the task goes back to `needs_review` |

A success resets the agent's failure counts, so the next failure starts from the base again.

An agent is degraded, and skipped, when all its models are cooled or it has `dispatch.degraded_rate_limit_threshold` (3) rate-limit rows in the last `dispatch.health_check_window_hours` (6). Only rows of type `rate` count. Failed runs and timeouts keep their own cooldowns and do not count.

```bash
orch cooldown list          # active cooldowns and degraded agents
orch cooldown clear <key>   # clear one cooldown (claude or claude:sonnet) and that agent's rate-limit window
orch cooldown clear --all   # clear every cooldown and every agent's rate-limit window
```

The running service picks a clear up on its next tick. That includes the GitHub 5xx circuit breaker: a cleared `cooldown:github:5xx` closes it and empties its error window.

## Codex Sandbox Notes

Codex runs under the `exec` subcommand. The sandbox flag depends on `workflow.permissions.mode` and `workflow.permissions.sandbox`:

| Mode | Sandbox | Flags |
|------|---------|-------|
| `autonomous` | (any) | `--sandbox workspace-write -c 'approval_policy="never"'` |
| `supervised` | `workspace-write` | `-c 'approval_policy="on-request"' --sandbox workspace-write` |
| `autonomous` | `full-access` | `--dangerously-bypass-approvals-and-sandbox` |

The runner also sets:

- `-c 'sandbox_workspace_write.network_access=true'` (must precede `exec` — placing it after silently leaves the sandbox network-blocked)
- `-c 'shell_environment_policy.inherit=all'` (so `bun`, `cargo`, etc. work)

`--full-auto` is **not** used — it was deprecated in Codex 0.128.0. Do not reintroduce it.
