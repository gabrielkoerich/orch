# Orchestrator — Agent & Developer Notes

You are an autonomous orchestrator. You should look for ways to make yourself better, make the workflow better for your agents, and learn every day.

## Upgrading

```bash
brew update && brew upgrade orchestrator
```

## Restarting the service

```bash
orchestrator stop && orchestrator start
```

Or equivalently:
```bash
brew services restart orchestrator
```

## Unblocking tasks

```bash
orchestrator tasks unblock all
```

## Logs

- Service log: `~/.orchestrator/.orchestrator/orchestrator.log`
- Brew stdout: `/opt/homebrew/var/log/orchestrator.log` (startup messages only)
- Brew stderr: `/opt/homebrew/var/log/orchestrator.error.log`

## Complexity-based model routing

The router assigns `complexity: simple|medium|complex` instead of specific model names. The actual model is resolved per agent from `config.yml`:

```yaml
model_map:
  simple:
    claude: haiku
    codex: gpt-5.1-codex-mini
  medium:
    claude: sonnet
    codex: gpt-5.2
  complex:
    claude: opus
    codex: gpt-5.3-codex
  review:
    claude: sonnet
    codex: gpt-5.2
```

See `model_for_complexity()` in `scripts/lib.sh`.

## Directory layout

```
~/.orchestrator/
  tasks.yml              # task database (all projects, filtered by dir)
  config.yml             # global config
  jobs.yml               # scheduled jobs
  projects/              # bare clones added via `orch project add`
    owner/repo.git       #   each has .orchestrator.yml inside
  worktrees/             # agent worktrees (all projects)
    repo/branch/         #   created by run_task.sh, one per task
  .orchestrator/         # runtime state (logs, prompts, pid, locks)
```

- **User-managed projects** (e.g. `~/Projects/foo`): user clones, runs `orch init`. Project dir stays where the user put it.
- **Orch-managed projects** (`orch project add owner/repo`): bare clone at `~/.orchestrator/projects/<owner>/<repo>.git`.
- **Worktrees**: always at `~/.orchestrator/worktrees/<project>/<branch>/` regardless of project type.
- `ORCH_WORKTREES` env var overrides the worktrees base directory.

## Adding external projects

```bash
orch project add owner/repo               # bare clone + write config + sync issues
orch task add "title" -p owner/repo        # add task to managed project
orch project create                        # link or create GitHub Project board
```

`project add` clones via SSH (`git@github.com:owner/repo.git`), writes `.orchestrator.yml` inside the bare repo, and imports open GitHub issues as tasks.

## Specs & Roadmap

See [specs.md](specs.md) for architecture overview, what's working, what's not, and improvement ideas.

## Release pipeline

1. Push to `main`
2. CI runs tests, auto-tags (semver from conventional commits)
3. GitHub release created, Homebrew tap formula updated automatically
4. `brew upgrade orchestrator` picks up the new version
5. `orchestrator stop && orchestrator start` to load new code

**Do NOT manually edit the tap formula** — the CI pipeline handles it. The `Formula/orchestrator.rb` in this repo is a local reference copy, not the real tap.

### Post-push workflow

After pushing to main, always complete the full cycle:

```bash
git push                                    # 1. push
gh run watch --exit-status                  # 2. watch CI (tests → release → deploy)
brew update && brew upgrade orchestrator    # 3. pull new formula + install
brew services restart orchestrator          # 4. restart service with new code
orchestrator version                        # 5. verify
```

Do not skip steps — the service runs from the Homebrew cellar, not the repo.

## Task status semantics

- **`blocked`** — waiting on a dependency (parent blocked on children, missing worktree/dir)
- **`needs_review`** — requires human attention (max attempts, review rejection, agent failures, retry loops, timeouts)
- `mark_needs_review()` sets `needs_review`, NOT `blocked`
- Only parent tasks waiting on children should be `blocked`
- `poll.sh` auto-unblocks parent tasks when all children are done

## Preferred tools

- Use `rg` instead of `grep` — faster, installed as a brew dependency
- Use `fd` instead of `find` — faster, installed as a brew dependency
- Use `trash` instead of `rm` — recoverable, enforced in system prompt

## Agent sandbox

Agents run in worktrees, NOT the main project directory. The orchestrator enforces this:

1. **Prompt-level**: system prompt tells agents the main project dir is read-only
2. **Tool-level**: dynamic `--disallowedTools` blocks Read/Write/Edit/Bash targeting the main project dir
3. Config: `workflow.sandbox: false` to disable (not recommended)

## Codex sandbox config

Codex runs with `--full-auto` + network access enabled by default. Configurable:

```yaml
# In config.yml or .orchestrator.yml
agents:
  codex:
    sandbox: full-auto  # full-auto | workspace-write | danger-full-access | none
```

Or per-run: `CODEX_SANDBOX=danger-full-access orchestrator task run 5`

Modes:
- `full-auto` (default) — filesystem sandboxed, network enabled
- `workspace-write` — same sandbox, explicit mode
- `danger-full-access` — no sandbox (for tasks needing bun, solana-test-validator, etc.)
- `none` — bypasses all Codex sandboxing (orchestrator is the sandbox)

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
