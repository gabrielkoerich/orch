# Orchestrator

[![CI](https://github.com/gabrielkoerich/orchestrator/actions/workflows/release.yml/badge.svg?branch=main)](https://github.com/gabrielkoerich/orchestrator/actions/workflows/release.yml?query=branch%3Amain)

A lightweight autonomous agent orchestrator that routes tasks to AI coding agents, manages their execution in isolated git worktrees, and tracks the full lifecycle from GitHub Issue to merged PR. GitHub Issues are the native task backend — labels drive status, comments carry agent output, and sub-issues handle delegation. Agents run via CLI tools (`claude`, `codex`, and `opencode`) in tmux sessions with full tool access.

## Install

```bash
brew tap gabrielkoerich/tap
brew install orchestrator
```

All dependencies (`yq`, `jq`, `just`, `python3`, `rg`, `fd`) are installed automatically.

### Agent CLIs

Install at least one:
```bash
brew install --cask claude-code   # Claude
brew install --cask codex         # Codex
brew install opencode             # OpenCode
```

Required: `gh` (GitHub CLI) — the native task backend uses GitHub Issues.
Optional: `bats` for running tests.

## Quick Start
```bash
cd ~/projects/my-app
orchestrator init               # configure project + GitHub repo
orchestrator task add "title"   # creates a GitHub issue
orchestrator task next          # route + run next task
orchestrator start              # start background server
```

## Architecture

Tasks are GitHub Issues. The orchestrator polls for issues with `status:new` labels, routes them to the best agent, creates an isolated git worktree, runs the agent in a tmux session, and posts results back as issue comments. PRs are created automatically and linked via `Closes #N`.

```
GitHub Issue (status:new) → Route (LLM picks agent) → Worktree (isolated branch)
    → tmux session (agent runs) → Commit + Push → PR → Review → Merge
```

## Files

All runtime state lives in `~/.orchestrator/` (`ORCH_HOME`):
- `orchestrator.db` — SQLite database (task metadata cache, jobs)
- `jobs.yml` — scheduled job definitions
- `config.yml` — runtime configuration
- `skills.yml` — approved skill repositories and catalog
- `skills/` — cloned skill repositories (via `skills-sync`)
- `projects/` — bare-cloned repositories (`owner/repo.git`)
- `worktrees/` — project-local git worktrees per task
- `.orchestrator/` — runtime state (pid, logs, locks, sidecars, prompts)

Source files:
- `prompts/system.md` — system prompt (output format, workflow, constraints)
- `prompts/agent.md` — execution prompt (task details + enriched context)
- `prompts/plan.md` — planning/decomposition prompt
- `prompts/route.md` — routing + profile generation prompt
- `prompts/review.md` — optional review agent prompt
- `scripts/backend_github.sh` — GitHub Issues backend (labels, comments, status)
- `scripts/run_task.sh` — task execution (routing, agent invocation, response parsing)
- `scripts/serve.sh` — main service loop
- `scripts/poll.sh` — task dispatcher
- `tests/orchestrator.bats` — 265+ tests
- `.orchestrator.example.yml` — template for per-project config override

## Task Model

Each task **is** a GitHub Issue. Metadata is stored in labels and a local sidecar JSON file:

**GitHub Issue labels** (source of truth):
- `status:new`, `status:routed`, `status:in_progress`, `status:done`, `status:blocked`, `status:in_review`, `status:needs_review`
- `agent:claude`, `agent:codex`, `agent:opencode`
- `complexity:simple`, `complexity:medium`, `complexity:complex`
- `role:backend`, `role:frontend`, `role:docs`, etc.
- `plan`, `scheduled`, `no-agent`, `no-review`, `has-error`

**Sidecar file** (`~/.orchestrator/.orchestrator/{task_id}.json`):
- `agent_model`, `complexity`, `branch`, `worktree`
- `attempts`, `duration`, `input_tokens`, `output_tokens`
- `summary`, `reason`, `accomplished[]`, `remaining[]`, `files_changed[]`
- `prompt_hash`, `last_comment_hash`

**Issue comments** serve as history (timestamped status transitions, agent reports).

## How It Works
1. **Create a GitHub Issue** (or via `orchestrator task add`). Gets `status:new` label automatically.
2. **Route the task** — LLM router picks the best agent + model and builds a specialized profile.
3. **Create worktree** — isolated git branch + worktree at `~/.orchestrator/worktrees/{project}/{branch}`.
4. **Run the agent** in a tmux session (`orch-{issue_number}`). Agent has full tool access inside the worktree.
5. **Collect results** — agent writes JSON to output file. Orchestrator parses, commits changes, pushes branch, creates PR.
6. **Review** — if enabled, a different agent reviews the PR and posts a GitHub review.
7. **Delegation** — if the agent returns `delegations`, child issues are created as sub-issues and the parent is blocked until children finish.
8. **Error handling** — failures are posted as issue comments with full context. `status:blocked` label added. `/retry` command to re-run.

## Routing: How the Orchestrator Picks an Agent

The orchestrator uses an LLM-as-classifier to route each task to the best agent. This is a non-agentic call (`--print`) — fast and cheap, no tool access needed.

### How It Works
1. `route_task.sh` sends the task title, body, labels, and the skills catalog to a lightweight LLM (default: `claude --model haiku --print`).
2. The router LLM returns JSON with:
   - **executor**: which agent to use (`codex`, `claude`, or `opencode`)
   - **model**: optional model suggestion (e.g. `sonnet`, `opus`, `gpt-4.1`)
   - **reason**: short explanation of the routing decision
   - **profile**: a specialized agent profile (role, skills, tools, constraints)
   - **selected_skills**: skill ids from the catalog
3. Sanity checks run — e.g. warns if a backend task gets routed to claude, or a docs task to codex.
4. If the router fails, it falls back to `config.yml`'s `router.fallback_executor` (default: `codex`).

### Router Config
```yaml
router:
  agent: "claude"       # which LLM does the routing
  model: "haiku"        # fast/cheap model for classification
  timeout_seconds: 120
  fallback_executor: "codex"  # safety net if routing fails
```

### Available Executors
| Executor | Best for |
|---|---|
| `codex` | Coding, repo changes, automation, tooling |
| `claude` | Analysis, synthesis, planning, writing |
| `opencode` | Lightweight coding and quick iterations |

The routing prompt is in `prompts/route.md`. The router only classifies — it never touches code or files.

## Agentic Mode

Once routed, agents run in full agentic mode with tool access:
- **Claude**: `-p` flag (non-interactive agentic mode), `--permission-mode acceptEdits`, `--output-format json`, system prompt via `--append-system-prompt`
- **Codex**: `-q` flag (quiet non-interactive mode), `--json`, system+agent prompt combined
- **OpenCode**: `opencode run --format json` with combined prompt

Agents execute inside `$PROJECT_DIR` (the directory you ran `orchestrator` from), so they can read project files, edit code, and run commands. The context below is injected into the prompt as starting knowledge so agents don't waste time exploring — but since they have full tool access, they can also read any file themselves.

### Agent Safety Rules

Agents are constrained by rules in the system prompt:
- **No `rm`**: `--disallowedTools` blocks `rm` — agents must use `trash` (macOS) or `trash-put` (Linux)
- **No commits to main**: Agents must always work in feature branches
- **Required skills**: Skills listed in `workflow.required_skills` are marked `[REQUIRED]` in the agent prompt and must be followed exactly
- **GitHub issue linking**: If a task has a linked issue, the agent receives the issue reference for branch naming and PR linking
- **Cost-conscious sub-agents**: Agents are instructed to use cheap models for routine sub-agent work

### Context Enrichment

Every agent receives a rich context built from multiple sources:

| Context | Source | When | Description |
|---|---|---|---|
| **System prompt** | `prompts/system.md` | Always | Output format, JSON schema, workflow requirements, constraints |
| **Task details** | GitHub Issue | Always | Title, body, labels, agent profile (role/skills/tools/constraints) |
| **Error history** | Issue comments | On retries | Last 5 status transitions with timestamps (agent sees what already failed) |
| **Last error** | Sidecar JSON | On retries | Most recent error message |
| **GitHub issue comments** | GitHub API | If issue linked | Last 10 comments on the linked issue (agent sees discussion) |
| **Prior run context** | `contexts/task-{id}.md` | On retries | Logs from previous attempts + tool call summaries |
| **Repo tree** | `git ls-files` / `find` | Always | Truncated file listing (up to 200 files) |
| **Project instructions** | `CLAUDE.md` + `AGENTS.md` + `README.md` | If files exist | Project-specific instructions and documentation |
| **Skills docs** | `skills/{id}/SKILL.md` | If skills selected | Full skill documentation for each selected skill |
| **Parent context** | Parent issue | For child tasks | Parent task summary + sibling task statuses |
| **Git diff** | `git diff --stat HEAD` | On retries (attempts > 0) | Current uncommitted changes |
| **Output file path** | `.orchestrator/output-{id}.json` | Always | Where the agent writes its JSON results |

### How Context Flows

```
run_task.sh
├── load_task()                    → TASK_TITLE, TASK_BODY, TASK_LABELS, AGENT_PROFILE_JSON, ...
├── fetch_issue_comments()         → ISSUE_COMMENTS   (last 10 GitHub issue comments)
├── task history + last_error      → TASK_HISTORY, TASK_LAST_ERROR
├── load_task_context()            → TASK_CONTEXT     (prior run logs + tool summaries)
├── build_parent_context()         → PARENT_CONTEXT   (parent summary + sibling statuses)
├── build_project_instructions()   → PROJECT_INSTRUCTIONS  (CLAUDE.md + AGENTS.md + README.md)
├── build_skills_docs()            → SKILLS_DOCS      (SKILL.md for each selected skill)
├── build_repo_tree()              → REPO_TREE        (truncated file listing)
├── build_git_diff()               → GIT_DIFF         (on retries only)
│
├── render_template("prompts/system.md")  → SYSTEM_PROMPT  (workflow + output format)
├── render_template("prompts/agent.md")   → AGENT_MESSAGE  (all context above)
│
├── agent invocation (cd $PROJECT_DIR)
│   ├── claude -p --permission-mode acceptEdits --output-format json ...
│   ├── codex -q --json ...
│   └── opencode run --format json ...
│
└── post-agent
    ├── extract tool history         → tools-{id}.json
    ├── extract token usage          → input_tokens, output_tokens
    ├── capture stderr snippet       → stderr_snippet (500 chars)
    ├── calculate duration           → duration (seconds)
    ├── push branch if not main      → git push -u origin <branch>
    └── store metadata in sidecar + issue labels
```

### Output
The agent writes results to `.orchestrator/output-{task_id}.json`. If the file isn't found (e.g. older agents or non-agentic fallback), the orchestrator falls back to parsing JSON from stdout via `normalize_json.py`, which handles Claude's result envelope, markdown fences, and mixed text.

## Usage

| Command | Description |
| --- | --- |
| `orchestrator init` | Initialize orchestrator for current project. |
| `orchestrator dashboard` | Overview: tasks, projects, worktrees. |
| `orchestrator chat` | Interactive chat with the orchestrator. |
| `orchestrator start` | Start server (uses `brew services` if installed via brew). |
| `orchestrator stop` | Stop server. |
| `orchestrator log` | Tail orchestrator log. |
| `orchestrator agents` | List installed agent CLIs. |
| `orchestrator --version` | Show version. |

### Task Commands

| Command | Description |
| --- | --- |
| `orchestrator task status` | Show status counts and recent tasks. |
| `orchestrator task status -g` | Show global status across all projects. |
| `orchestrator task add "Build router" "Add LLM router" "orchestration"` | Add a task (title required, body/labels optional). |
| `orchestrator task plan "Implement auth" "Add login, signup, reset" "backend"` | Add a task that will be decomposed into subtasks first. |
| `orchestrator task list` | List tasks (id, status, agent, parent, title). |
| `orchestrator task tree` | Show parent/child task tree. |
| `orchestrator task route 1` | Route task `1`. If no ID, route next `new` task. |
| `orchestrator task run 1` | Run task `1`. If no ID, run next runnable. |
| `orchestrator task next` | Route + run the next task in one step. |
| `orchestrator task poll` | Run all runnable tasks in parallel (default 4 workers). |
| `orchestrator task retry 1` | Retry a blocked/done task (reset to new). |
| `orchestrator task unblock 1` | Unblock a blocked task (reset to new). |
| `orchestrator task unblock all` | Unblock all blocked tasks. |
| `orchestrator task agent 1 claude` | Force a task to use a specific agent. |
| `orchestrator task stream 1` | Stream live agent output. |
| `orchestrator task watch` | Poll loop every 10s. |

### Scheduled Jobs

| Command | Description |
| --- | --- |
| `orchestrator job add "0 9 * * *" "Daily Sync" "Pull and check" "sync"` | Add a scheduled task job. |
| `orchestrator job add --type bash --command "echo hi" "@hourly" "Ping"` | Add a bash job (no LLM). |
| `orchestrator job list` | List all jobs with status and next run. |
| `orchestrator job remove daily-sync` | Remove a job. |
| `orchestrator job enable daily-sync` | Enable a job. |
| `orchestrator job disable daily-sync` | Disable a job. |

### Skills

| Command | Description |
| --- | --- |
| `orchestrator skills list` | List skills in the catalog. |
| `orchestrator skills sync` | Sync skills from registry to `skills/`. |

## Per-Project Isolation

Each task is tagged with its project directory. When you run commands from a project, you only see that project's tasks:

```bash
cd ~/projects/app-a && orchestrator task add "Task A"
cd ~/projects/app-b && orchestrator task add "Task B"

cd ~/projects/app-a && orchestrator task list  # shows only Task A
cd ~/projects/app-b && orchestrator task list  # shows only Task B
```

A single `orchestrator serve` handles all projects — it reads each task's `dir` field and runs agents in the correct directory. Use `orchestrator dashboard` to see active projects and worktrees.

## Background Service

```bash
orchestrator start      # start (delegates to brew services)
orchestrator stop       # stop
orchestrator restart    # restart
orchestrator info       # check status
```

Auto-starts on login, auto-restarts on crash via `brew services`.

## Scheduled Jobs (Cron)

Jobs are defined in `jobs.yml` and create regular tasks on a schedule. They flow through the full pipeline (route, run, review, delegate, GitHub sync).

### How It Works
1. Define a job with a cron expression and a task template.
2. On each tick, the scheduler checks which jobs are due.
3. Each job tracks its `active_task_id`. If that task is still in-flight (any status except `done`), the job waits — no duplicates.
4. When the previous task completes, the job is free to create a new one on the next matching schedule.

### Running the Scheduler
The server runs `job tick` on every poll cycle automatically:
```bash
orchestrator start
```

### Job Definition

Two job types:
- **task** (default): creates a task that goes through routing and agent execution
- **bash**: runs a shell command directly, no LLM involved

```yaml
# jobs.yml
jobs:
  - id: daily-sync
    schedule: "0 9 * * *"
    type: task              # default, creates agent task
    task:
      title: "Daily code sync"
      body: "Pull latest changes, run linting, check for issues"
      labels: [sync]
      agent: ""             # empty = let router decide
    enabled: true
    last_run: null
    last_task_status: null
    active_task_id: null

  - id: hourly-ping
    schedule: "@hourly"
    type: bash              # runs command directly
    command: "curl -s https://example.com/health"
    enabled: true
```

### Schedule Expressions
Standard 5-field cron: `minute hour day_of_month month day_of_week`

Aliases: `@hourly`, `@daily`, `@weekly`, `@monthly`, `@yearly`

Supports: wildcards (`*`), ranges (`1-5`), steps (`*/15`, `1-5/2`), lists (`1,3,5`).

### Dedup & Safety
- Each job has `active_task_id` tracking its current in-flight task.
- If the task is `new`, `routed`, `in_progress`, or `blocked` — the job waits.
- Tasks can delegate children, get reviewed, or get blocked — the job won't interfere.
- Only when the task reaches `done` does the job create a new one.
- Tasks from jobs get `scheduled` and `job:{id}` labels for easy filtering.
- All job-created tasks sync to GitHub issues like any other task.

## Dynamic Agent Profiles
The router generates a profile for each task, stored in the sidecar file. You can override routing via issue labels.

Example:
```yaml
agent_profile:
  role: backend specialist
  skills: [api, sql, testing]
  tools: [git, rg]
  constraints: ["no migrations"]
```

## Skills Catalog
`skills.yml` defines approved skill repositories and a catalog of skills. The router selects skill ids and stores them in `selected_skills`.

### Required Skills
Skills listed in `workflow.required_skills` are always injected into agent prompts, marked `[REQUIRED]`, and enforced regardless of what the router selects. Configure in `config.yml`:
```yaml
workflow:
  required_skills:
    - github              # GitHub CLI operations
    - gh-pr-polish        # PR titles and bodies
```

### Commit Pinning
Skill repositories are pinned to audited commit SHAs in `skills.yml` to prevent supply chain attacks:
```yaml
repositories:
  - name: gabrielkoerich
    url: https://github.com/gabrielkoerich/skills
    pin: 226f5f11346eddceebf017746aa5cd660ef3af20
```
When pinned, `skills-sync` checks out the exact commit instead of pulling latest.

### Syncing
Clone or update skills with:
```bash
orchestrator skills sync
```

## Error Handling & GitHub Issue Feedback

When an agent fails, the orchestrator classifies the error and blocks the task:

### Error Classification
| Error Type | Detection | Action |
|---|---|---|
| **Auth/billing** | Pattern match on stderr/stdout (401, 403, expired key, quota, etc.) | Block + comment on issue |
| **Timeout** | Exit code 124 | Block + comment on issue |
| **Generic failure** | Any non-zero exit | Block + comment on issue |
| **Invalid response** | No JSON in output file or stdout | Block + comment on issue |

### Retry Loop Detection
If the same error repeats 3 times (4+ attempts), the orchestrator detects a retry loop and blocks the task permanently with a clear message.

### What Happens on Failure
1. Task status set to `blocked` (no auto-retry)
2. Error details saved to sidecar file and posted as issue comment
3. Error logged to task history with timestamp
4. **GitHub issue comment** posted with full details (see below)
5. **Red `blocked` label** added to the GitHub issue (auto-created if missing)

### Unblocking
Tasks stay blocked until you manually investigate and unblock them:
1. Check the error on the GitHub issue
2. Fix the underlying problem (e.g. add API key, fix code)
3. Remove the `blocked` label from the issue
4. Set the task status back to `new` — the orchestrator picks it up again

### Agent-Reported Blocks
Agents can also report blocks in their JSON response:
- `status: blocked` — waiting for a dependency or missing information
- The agent must provide a `reason` field explaining what happened, what it tried, and what it needs

The reason is posted as an issue comment, logged to the sidecar, and appended to `contexts/task-{id}.md`.

## Logging & Observability

All state lives under `~/.orchestrator` (the install dir). Logs and runtime files are in the `.orchestrator/` subdirectory within it.

### Log Files

| File | Path | Description |
|---|---|---|
| **Server log** | `.orchestrator/orchestrator.log` | Main loop output: ticks, poll, gh sync, restarts |
| **Archive log** | `.orchestrator/orchestrator.archive.log` | Previous server sessions (rotated on each start) |
| **Jobs log** | `.orchestrator/jobs.log` | Scheduled job execution: triggers, task creation, bash output |
| **Brew log** | `/opt/homebrew/var/log/orchestrator.log` | stdout when running via `brew services` |

View the server log:
```bash
orchestrator log          # tail last 50 lines
orchestrator log 200      # tail last 200 lines
```

Or tail it live while the server runs:
```bash
TAIL_LOG=1 orchestrator serve
```

### Per-Task Logs

| File | Path | Description |
|---|---|---|
| **Task context** | `contexts/task-{id}.md` | Appended after each run: timestamp, status, summary, reason, files, tool summary |
| **Task history** | Issue comments | Status transitions with timestamps and notes |
| **Agent output** | `.orchestrator/output-{id}.json` | Structured JSON from the last agent run |
| **Agent prompt** | `.orchestrator/prompt-{id}.txt` | Full system prompt + agent message (with SHA-256 hash) |
| **Agent response** | `.orchestrator/response-{id}.txt` | Raw stdout from the agent |
| **Agent stderr** | `.orchestrator/stderr-{id}.txt` | Stderr captured from the agent (auth errors, warnings) |
| **Tool history** | `.orchestrator/tools-{id}.json` | Every tool call the agent made (Bash, Edit, Read, etc.) with error flags |
| **Route prompt** | `.orchestrator/route-prompt-{id}.txt` | The prompt sent to the router (for debugging routing decisions) |
| **Failed response** | `contexts/response-{id}.md` | Raw agent output when JSON parsing fails |

### GitHub Issue Comments

When GitHub sync is enabled, each status update posts a structured comment on the linked issue:

```
## 🟣 Claude Fixed the authentication bug     ← agent badge + summary as title

| | |
|---|---|
| **Status** | `done` |                       ← metadata table
| **Agent** | claude |
| **Model** | `claude-sonnet-4-5-20250929` |
| **Attempt** | 2 |
| **Duration** | 3m 42s |
| **Tokens** | 15k in / 3k out |
| **Prompt** | `a1b2c3d4` |

### Errors & Blockers                          ← only when blocked
**Reason:** SSH key not configured
> `git push: Permission denied (publickey)`
- Need SSH key configured for git push

### Accomplished                               ← bullet list
- Fixed memcmp offset from 40 to 48
- Added test coverage for edge case

### Remaining
- Deploy to staging

### Files Changed
- `src/auth.ts`
- `tests/auth.test.ts`

### Agent Activity                             ← tool call summary
| Tool | Calls |
|------|-------|
| Bash | 12 |
| Edit | 5 |
| Read | 8 |

<details><summary>Agent stderr</summary>       ← collapsed
...
</details>

<details><summary>Prompt sent to agent</summary> ← collapsed
...
</details>
```

Features:
- **Agent badges**: 🟣 Claude, 🟢 Codex, 🔵 OpenCode
- **Content-hash dedup**: identical comments are not re-posted (prevents spam)
- **Atomic sync**: `gh_synced_at` copies `updated_at` at write time (no stale variable bugs)
- **Blocked label**: red `blocked` label auto-applied/removed based on status
- **Owner ping**: `@owner` tagged on blocked tasks

### What Gets Logged Where

| Event | Server log | Task context | Task history | GitHub comment | Per-task files |
|---|---|---|---|---|---|
| Tick/poll cycle | x | | | | |
| Task started | x | | x | | prompt saved |
| Agent completed | x (duration, tokens, tools) | x (tool summary) | x | x (full report) | response + stderr + tools |
| Agent blocked/stuck | x | x | x | x (comment + label) | response + stderr + tools |
| Auth/billing error | x | | x | x (comment + label) | stderr |
| Timeout | x | | x | x (comment + label) | |
| Invalid response | x | x (raw saved) | x | x (comment + label) | response |
| Retry loop (3x same error) | x | | x | x (comment + label) | |
| Review result | x | | x | x | |
| Delegation | x | | x | x | |
| Job triggered | x | | | | |
| Config/code restart | x | | | | |

## Per-Project Config

Place a `.orchestrator.yml` in your project root to override the global config for that project. Only include the keys you want to override — everything else falls through to `~/.orchestrator/config.yml`.

```yaml
# myproject/.orchestrator.yml
required_tools: ["bun"]
gh:
  repo: "myorg/myproject"
  project_id: "PVT_..."
  sync_label: ""
workflow:
  auto_close: false
  review_owner: "@myhandle"
router:
  model: "sonnet"
```

This lets you:
- Use a different GitHub repo/project per project
- Customize workflow settings (review, auto-close) per project
- Override the router model or fallback agent
- Keep project-specific config in version control

The server restarts automatically when `.orchestrator.yml` changes.

## Config Reference
All runtime configuration lives in `config.yml`.

| Section | Key | Description | Default |
| --- | --- | --- | --- |
| top-level | `project_dir` | Override project directory (auto-detected from CWD). | `""` |
| top-level | `required_tools` | Tools that must exist on PATH before launching an agent. | `[]` |
| `workflow` | `auto_close` | Auto-close GitHub issues when tasks are `done`. | `true` |
| `workflow` | `review_owner` | GitHub handle to tag when review is needed. | `@owner` |
| `workflow` | `enable_review_agent` | Run a review agent after completion. | `false` |
| `workflow` | `review_agent` | Fallback reviewer when opposite agent unavailable. | `claude` |
| `workflow` | `max_attempts` | Max attempts before marking task as blocked. | `10` |
| `workflow` | `timeout_seconds` | Task execution timeout (0 disables timeout). | `1800` |
| `workflow` | `timeout_by_complexity` | Per-complexity task timeouts (takes precedence). | `{}` |
| `workflow` | `required_skills` | Skills always injected into agent prompts (marked `[REQUIRED]`). | `[]` |
| `workflow` | `disallowed_tools` | Tool patterns blocked via `--disallowedTools`. | `["Bash(rm *)","Bash(rm -*)"]` |
| `router` | `agent` | Default router executor. | `claude` |
| `router` | `model` | Router model name. | `haiku` |
| `router` | `timeout_seconds` | Router timeout (0 disables timeout). | `120` |
| `router` | `disabled_agents` | Agents to exclude from routing (e.g. `[opencode]`). | `[]` |
| `router` | `fallback_executor` | Fallback executor when router fails. | `codex` |
| `router` | `allowed_tools` | Default tool allowlist used in routing prompts. | `[yq, jq, bash, ...]` |
| `router` | `default_skills` | Skills always included in routing. | `[gh, git-worktree]` |
| `llm` | `input_format` | CLI input format override. | `""` |
| `llm` | `output_format` | CLI output format override. | `"json"` |
| `gh` | `enabled` | Enable GitHub sync. | `true` |
| `gh` | `repo` | Default repo (`owner/repo`). | `"owner/repo"` |
| `gh` | `sync_label` | Only sync tasks/issues with this label (empty = all). | `"sync"` |
| `gh` | `project_id` | GitHub Project v2 ID. | `""` |
| `gh` | `project_status_field_id` | Status field ID in Project v2. | `""` |
| `gh` | `project_status_names` | Mapping for `backlog/in_progress/review/done` status option names (used to resolve option IDs). | `{}` |
| `gh` | `project_status_map` | Mapping for `backlog/in_progress/review/done` option IDs. | `{}` |
| `gh.backoff` | `mode` | Rate-limit behavior: `wait` or `skip`. | `"wait"` |
| `gh.backoff` | `base_seconds` | Initial backoff duration in seconds. | `30` |
| `gh.backoff` | `max_seconds` | Max backoff duration in seconds. | `900` |

## Context Persistence
Task and profile contexts are persisted under `contexts/`:
- `contexts/task-<id>.md` — logs from each run (status, summary, reason, files)
- `contexts/profile-<role>.md` — role-specific context

The orchestrator loads both into the prompt and appends a log entry after each run.

## Task Decomposition (Plan Mode)

Complex tasks can be broken down into smaller subtasks before execution. This happens in two ways:

### Automatic (router decides)
The router evaluates task complexity and sets `decompose: true` when a task touches multiple systems, requires many file changes, or has multiple deliverables. The task gets a `plan` label automatically.

### Manual (user decides)
Add the `plan` label when creating a task:
```bash
orchestrator task plan "Implement user auth" "Add login, signup, password reset with JWT tokens" "backend"
```

Or add a task with the `plan` label directly:
```bash
orchestrator task add "Redesign the API" "..." "plan,backend"
```

### How It Works
1. The agent receives `prompts/plan.md` instead of the execution prompt
2. It reads the codebase, analyzes the task, and returns only delegations (no code changes)
3. Each subtask gets a clear title, detailed body with acceptance criteria, labels for routing, and a suggested agent
4. The parent blocks until all children complete, then resumes with the execution prompt

### Guidelines in the planning prompt
- Each subtask should be completable in a single agent run
- Subtasks are listed in dependency order
- Bodies include specific file paths, function names, and expected behavior
- Prefers 3-7 subtasks (not too granular, not too broad)
- Includes a testing/verification subtask at the end

## Delegation
If the agent returns this:
```json
{
  "needs_help": true,
  "delegations": [
    {
      "title": "Add unit tests",
      "body": "Test routing logic",
      "labels": ["tests"],
      "suggested_agent": "codex"
    }
  ]
}
```
The orchestrator will:
- Create child tasks
- Block the parent until children are done
- Re-run the parent via `poll` or `rejoin`

## Concurrency + Locking
- `poll` runs new tasks in parallel.
- SQLite WAL mode handles concurrent reads; per-task locks prevent double-run.
- Each task also has a per-task lock to prevent double-run.
- Stale locks are auto-cleared after `LOCK_STALE_SECONDS` (default 600).

## Review Agent (Optional)

Automatically review PRs using a different agent from the one that wrote the code.

```yaml
workflow:
  enable_review_agent: true
```

When enabled, after an agent completes a task and a PR is open:

1. **Opposite agent selected** — if codex wrote the code, claude reviews (and vice versa). Falls back to `workflow.review_agent` config if only one agent is available.
2. **PR diff fetched** — the actual PR diff via `gh pr diff` (first 500 lines).
3. **Real GitHub review posted** — via `gh pr review`:
   - `approve` → green checkmark review on the PR
   - `request_changes` → red X review, task goes to `needs_review`
   - `reject` → review + PR closed, task goes to `needs_review`

The `review_agent` config key is now an optional fallback — `opposite_agent()` handles primary selection.

Override the reviewer for a specific run:
```bash
REVIEW_AGENT=claude orchestrator task run <id>
```

See [Review Agent docs](docs/content/review-agent.md) for full details.

## GitHub Setup
GitHub Issues is the native task backend. Authentication is handled by `gh`.

<details>
<summary>GitHub Integration: Token Type and Permissions</summary>

**Recommended:** Fine-grained PAT scoped to the target repo(s).

Repository permissions:
- Issues: Read + Write
- Metadata: Read
- Contents: Read (optional)

Organization permissions (for Projects v2):
- Projects: Read + Write

Classic PATs will also work but are broader in scope.
</details>

### GitHub Setup
1. Install and authenticate:
```bash
gh auth login
```
2. Verify access:
```bash
gh repo view
```
Project fields belong in `config.yml`:
```yaml
gh:
  project_id: ""
  project_status_field_id: ""
  project_status_map:
    backlog: ""
    in_progress: ""
    review: ""
    done: ""
```
To discover Project field and option IDs:
```bash
orchestrator project info
```
To auto-fill the Status field/options into config:
```bash
orchestrator project info --fix
```

### Manual sync commands
```bash
orchestrator gh pull    # import new issues as tasks
orchestrator gh push    # push local status updates to GitHub
orchestrator gh sync    # both directions
```
Note: The background service runs `gh sync` automatically every 120s.

### Error Comments & Blocking
When a task fails (any error), the orchestrator:
1. Posts a structured comment on the linked GitHub issue (see "GitHub Issue Comments" above)
2. Adds a red `blocked` label to the issue (auto-created if missing)
3. Sets the task status to `blocked`
4. Tags `@owner` for attention

Comments include: agent badge, status metadata table (agent, model, attempt, duration, tokens), error details, blockers, tool activity summary, and collapsed stderr/prompt. Content-hash dedup prevents duplicate comments on repeated syncs.

### Notes
- The repo is resolved from `config.yml` or `gh repo view`.
- Issues are created for tasks without `gh_issue_number`.
- If a task has label `no_gh` or `local-only`, it will not be synced.
- If `config.yml` `gh.sync_label` is set, only tasks/issues with that label are synced.
- If `config.yml` `gh.enabled` is `false`, GitHub sync is disabled.
- Task status is synced to issue labels using `status:<status>`.
- When a task is `done`, `auto_close` controls whether to close the issue or tag the owner for review.
- Scheduled job tasks get `scheduled` and `job:{id}` labels and sync to GitHub like any other task.
- Agents never call GitHub directly; the orchestrator posts comments and status updates so it can back off safely when rate-limited.

### GitHub Backoff
When GitHub rate limits or abuse detection triggers, the orchestrator sleeps and retries instead of hammering the API.

Config keys:
- `gh.backoff.mode` — `wait` (default) or `skip`
- `gh.backoff.base_seconds` — initial backoff duration
- `gh.backoff.max_seconds` — maximum backoff duration

The backoff is shared across pull/push/comment/project updates, so a single rate limit event pauses all GitHub writes.

### Projects (Optional)
Provide in `config.yml`:
- `gh.project_id`
- `gh.project_status_field_id`
- `gh.project_status_map` (Backlog/In Progress/Review/Done option IDs)

#### Finding IDs
1. Project ID (GraphQL):
```bash
gh api graphql -f query='query($org:String!, $num:Int!){ organization(login:$org){ projectV2(number:$num){ id } } }' -f org=YOUR_ORG -f num=PROJECT_NUMBER
```
2. Status field ID + option IDs (or use `orchestrator project info`):
```bash
gh api graphql -f query='query($project:ID!){ node(id:$project){ ... on ProjectV2 { fields(first:50){ nodes{ ... on ProjectV2SingleSelectField { id name options{ id name } } } } } } }' -f project=YOUR_PROJECT_ID
```

## Notes
- GitHub Issues are the source of truth for tasks. Labels drive status.
- Routing and profiles are LLM-generated; you can override them via issue labels.
- Agents run in agentic mode inside isolated git worktrees with full tool access.
- The router stays non-agentic (`--print`) — it's a classification task.

---

## Development

```bash
git clone https://github.com/gabrielkoerich/orchestrator.git
cd orchestrator
bats tests          # run tests
just                # list available commands
```

Requires: `yq`, `jq`, `just`, `python3`, `rg`, `fd`, `bats`.
