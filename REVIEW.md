# Orchestrator Review

## What It Is

A bash-based task orchestrator that uses LLMs to classify tasks, route them to the right coding agent (Claude, Codex, OpenCode), and manage the full lifecycle: routing, execution, delegation, retry, review, GitHub sync, and cron scheduling. All state lives in YAML files. No database, no server framework, no dependencies beyond `yq`, `jq`, `just`, and `python3`.

## What It's Good At

The architecture is sound for its use case. It's a single-user, single-machine orchestrator for automating coding work across multiple AI agents. The strengths:

- **Routing is clever.** Using a cheap/fast LLM as a classifier to pick the right agent + build a specialized profile is a good design. The router only classifies — it never touches code. Separation of concerns.
- **Context enrichment is thorough.** Agents get repo tree, project docs, skill docs, prior run logs, parent/sibling context, git diff. They start with real knowledge instead of wasting tokens exploring.
- **Delegation works well.** Parent blocks, children run in parallel, parent resumes with full context. This enables multi-step workflows naturally.
- **The retry/backoff system is solid.** Exponential backoff, stuck detection, history logging, reason tracking. Agents can't silently fail.
- **GitHub integration is complete.** Two-way sync, project board status, owner tagging on stuck tasks, rate limit backoff with persistent state.
- **Jobs system is pragmatic.** Per-job dedup via `active_task_id` is simple and correct — handles delegation, retry, review naturally without complex state machines.

## What It Can Do Well

- **Automated code maintenance**: linting, dependency updates, test fixes, doc generation — tasks with clear inputs and verifiable outputs
- **Multi-agent pipelines**: one agent plans, delegates coding to another, a third reviews
- **Scheduled maintenance**: daily checks, weekly reports, periodic refactors via cron jobs
- **GitHub-driven workflows**: issues become tasks, agents work them, results sync back as comments
- **Small-to-medium feature work**: adding a function, fixing a bug, writing tests — scoped, well-defined tasks
- **Parallel workloads**: 4+ tasks running simultaneously on different parts of a codebase

## Where It Would Fail

### 1. Complex, multi-file features requiring human judgment

An agent can read files and edit code, but it doesn't understand product requirements, user intent, or design tradeoffs the way a human does. A task like "redesign the auth system" would likely produce something that works technically but misses the point. The orchestrator can't evaluate whether the output is *correct*, only whether the agent *says* it's done.

### 2. Tasks requiring coordination across repos

The orchestrator operates in one `$PROJECT_DIR` at a time. Cross-repo work (e.g., "update the API and the client library") requires manual coordination or separate orchestrator instances.

### 3. Long-running tasks that need interactive feedback

Agents run non-interactively. If a task needs back-and-forth ("try this, no not that, adjust the styling") — that's not what this is for. It's fire-and-forget.

### 4. High-concurrency production workloads

There are real race conditions:

| Issue | Risk | Likelihood |
|---|---|---|
| **ID collision during delegation** — two parallel delegation operations could both read the same MAX_ID | Data corruption | Low (lock held, but yq failure mid-write could cause it) |
| **Job dedup window** — two `jobs_tick` instances in the same minute could create duplicate tasks | Duplicate tasks | Very low (minute-level guard, but not atomic) |
| **Lock timeout under load** — 4 parallel tasks all finishing at once, all competing for the global YAML lock | Task failure | Low (20s timeout, yq is fast) |

These aren't show-stoppers for single-user use, but they'd bite in a multi-user or high-throughput setup.

### 5. Recovery from corrupted state

If `tasks.yml` gets corrupted (disk full during write, yq crash mid-update), everything stops. No automatic backup, no rollback, no integrity checking. You'd need to restore manually from git.

### 6. Unbounded context growth

A task retried 50 times accumulates 50 context entries in `contexts/task-{id}.md`. All of that gets injected into the next prompt. Combined with a large README, multiple skill docs, and parent context — you could hit token limits or just confuse the model with too much noise.

## Security Assessment

**Shell injection: safe.** Task titles/bodies go through `env()` in yq (string literal, not evaluated) and regex substitution in Python (no shell eval). No `eval` with user input anywhere.

**Prompt injection: possible.** A malicious task title from GitHub could influence the router's LLM decision. Not a shell-level risk, but an LLM-level one. Mitigated by the fact that you control the agents.

**Secrets: acceptable.** Config doesn't store tokens (uses `gh auth`). But `.orchestrator/output-*.json` could contain sensitive data from agent runs — make sure `.orchestrator/` stays gitignored.

## Production Hardening Recommendations

If you want to run this reliably long-term:

1. **Backup tasks.yml** — automatic daily copy to `.orchestrator/tasks.yml.backup` before each serve cycle
2. **Truncate large context** — cap `PROJECT_INSTRUCTIONS` at 10KB, rotate `contexts/task-{id}.md` after N retries
3. **Timeout stuck tasks** — auto-fail tasks in `in_progress` for > 2 hours (agent is truly hung, not just slow)
4. **Reopen closed issues** — when a `done` task reverts to `in_progress`, reopen the GitHub issue
5. **PID check on global lock** — like per-task locks already do, detect dead processes holding the global lock
6. **Validate YAML on read** — if `tasks.yml` is unparseable, restore from backup before proceeding

## Verdict

**Good for: a solo developer or small team automating coding work on a single project.** The bash-and-YAML approach is surprisingly effective — no infrastructure to manage, no database to maintain, easy to understand and debug. The routing + context enrichment + delegation pipeline is well-designed.

**Not ready for: multi-user production with high throughput.** The YAML file lock is the bottleneck, error recovery is manual, and there's no observability beyond log files and GitHub comments.

**Can it do complex tasks?** It can *orchestrate* complex tasks by breaking them into simpler pieces via delegation. A single agent will struggle with ambiguous, large-scope work. But the pipeline of route -> execute -> delegate -> review -> retry gives it a way to handle multi-step work that a single agent call can't. The quality depends entirely on the underlying agents — the orchestrator just makes sure they get the right context and their results are tracked.

It's a well-built tool for its scope. The hardening items above would make it production-reliable for single-user use.
