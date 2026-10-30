# Ideas from infinity — Plan

Status: plan, 2026-10-29. Six changes taken from [hydro-project/infinity](https://github.com/hydro-project/infinity), an agent runtime with its own model loop. Infinity solves a different problem, since orch drives CLI agents that run their own loops, so most of its design does not carry over. These six do. Line numbers are from `main` at v0.98.0.

| # | Idea | Size | Files |
|---|------|------|-------|
| 1 | A re-run parent sees what its children did | Small | `src/engine/runner/context.rs`, `prompts/agent_system.md` |
| 2 | A delegated task cannot delegate again | Tiny | `src/engine/runner/mod.rs`, `src/engine/runner/context.rs` |
| 3 | Each instruction file is inlined once | Done in #3871 | `src/engine/runner/context.rs` |
| 4 | An agent asks for sandbox permission through the channels | Medium | `src/parser.rs`, `src/engine/runner/sandbox.rs`, `src/engine/runner/mod.rs`, `src/engine/channel_handler.rs`, a new migration |
| 5 | A fatal agent error ends the run when it is printed | Medium | `src/engine/runner/session.rs`, `src/engine/runner/agents/mod.rs` |
| 6 | A live diff for each running task | Small | `src/channels/capture.rs`, `src/engine/events.rs`, `src/cli/stream.rs` |

None of them adds a token or cost budget, a per-model case, a routing timer, an external endpoint or a config write by an agent.

## 1. A re-run parent sees what its children did

**Infinity:** a child thread ends with `report_to_parent` or `close_thread(report)`, and the runtime puts the report into the parent's history as a tool result (`docs/docs/infinity-runtime/built-in/threading.md`).

**Orch today:** `process_delegations` (`src/engine/runner/mod.rs:1605`) creates one issue per delegation and blocks the parent. The tick sets the parent back to `New` when every child is done (`src/engine/tick.rs:2649`). The prompt promises "you will be re-run after all subtasks complete" (`prompts/agent_system.md:128`). On that re-run, `build_full_context` (`context.rs:396`) gives a task its parent and siblings through `build_parent_context` (`context.rs:53`), but nothing gives a parent its children.

**Change:**

- `context.rs`: a `build_children_context(task, backend, store)` beside `build_parent_context`. For each child: its title, status, the `summary` the child's agent stored, its PR number and whether that PR merged. `build_full_context` adds it when the task has children.
- `prompts/agent_system.md`: a `{{CHILDREN_CONTEXT}}` section, and the delegation rule says the re-run gets the children's results.

**Why it is better:** the parent's second run starts from what the children did instead of reading their issues and PRs again. That run decides whether the work is complete, so it should see the results it is judging.

**Cost:** one backend read per child on a parent's re-run. Parents are rare, and the sibling code already makes the same reads.

**Rejected:** posting each child's summary as a comment on the parent issue. The agent would have to find and read them, and the comments add noise for people reading the issue.

## 2. A delegated task cannot delegate again

**Infinity:** `spawn_thread` takes `child_of` and refuses the call unless it matches the caller's full thread stack, so a child that inherited its parent's plan cannot re-run the parent's spawns (`crates/infinity-agent-core/src/tools/thread.rs`).

**Orch today:** `process_delegations` (`mod.rs:1605`) accepts delegations from any task. Its only guard is an exact title match against open tasks (`mod.rs:1626`). A child can delegate again, and a reworded title passes the check.

**Change:**

- `mod.rs`: when the task has a `parent_id`, log the delegations, drop them and handle the response as a run without delegations.
- `context.rs`: a child's prompt says that it cannot delegate, so it does the work or reports it blocked.

**Why it is better:** it stops recursion and fan-out from a child that copies its parent's plan, which costs a run per issue and floods the tracker. A child exists to do one piece of work.

**Cost:** a task that needs a second level of split cannot get one. Its agent reports blocked and a person splits it. That is rare, and a person should look at a tree that deep anyway.

**Rejected:** a depth limit of N. It still allows fan-out at each level, and nothing so far needs more than one level.

## 3. Each instruction file is inlined once

**Infinity:** a steering server lists the instruction files and the agent loads them on demand (`crates/rap-steering-server/src/lib.rs`).

**Done in #3871.** `build_project_instructions` (`context.rs:127`) read `CLAUDE.md`, `AGENTS.md` and `README.md` and inlined every one it found. In this repository `CLAUDE.md` is a symlink to `AGENTS.md`, which is 45 KB, so every task prompt carried the same 45 KB twice. It now resolves each path with `canonicalize` and skips a real path it already included.

**Left:** two separate files with the same content are both still inlined. Skipping a file whose content hash was already included would cover that, but no project is known to have such copies, so it waits until one does.

**Why it is better:** the prompt stops repeating a 45 KB file for every task, and so does any project that symlinks one name to the other, a common setup.

**Cost:** a hash per file. Nothing else.

**Rejected:**

- Loading the files on demand, as infinity does. Orch's agents are CLIs without a tool for that, and the review agent reads `AGENTS.md` from the prompt.
- Skipping the files that claude and codex load on their own. It needs a flag on each agent's runner, close to the rule against per-agent cases in the runner.

## 4. An agent asks for sandbox permission through the channels

**Infinity:** a command that needs to write outside its sandbox asks for `write-orig` or `write:/path`. The tool server sends the user a choice, scoped to this thread, an ancestor or once, and saves the grant for the thread (`crates/sandbox-core/src/server.rs:846-945`).

**Orch today:** with `workflow.sandbox: os`, the policy is fixed per run (`policy()` in `src/engine/runner/sandbox.rs:196`). A write the policy denies fails with `Operation not permitted`, and the task blocks or fails. A person then has to find the path and change something by hand.

**Change:**

- `src/parser.rs`: `AgentResponse` (`parser.rs:18`) gets an optional `sandbox_request` with the paths and a reason. `prompts/agent_system.md` tells a sandboxed agent to use it, with `blocked`, after a denied write it cannot work around.
- A new migration, `032_sandbox_grants.sql`, with a `sandbox_grants` table (task, path, scope, granted_at), and its line in `migrations/checksums.txt`.
- `src/engine/runner/mod.rs`: a response with a `sandbox_request` blocks the task with the paths in `block_reason`, and posts the request to the task's channels.
- `src/engine/channel_handler.rs`: buttons for approve once, approve for this task and deny, as `buttons_for_picker` builds for the repo picker. Approving saves the grant and sets the task back to `routed`.
- `src/engine/runner/sandbox.rs`: `argv` reads the task's grants and adds the paths to `writable`. A grant cannot reach a hidden path, the git hooks or config, or `~/.orch`. Those stay refused, and the request says why.

**Why it is better:** the operator decides from Telegram or Discord in a minute, without reading logs to find the denied path. The agent resumes in its worktree, and the exception exists only for that task, never in a config file.

**Cost:** a table, a response field, a channel flow and a policy read. A grant widens the sandbox for one task, so the channel message names the paths, and the paths that must stay protected are refused in code.

**Rejected:** grants in `.orch.yml` or `config.yml`. Config is operator-only, a grant there applies to every task, and the agent's own request would be editing it.

## 5. A fatal agent error ends the run when it is printed

**Infinity:** observers get each event as it happens, typed: text, a tool call, usage, an error (`docs/docs/infinity-runtime/agent-systems/observers.md`).

**Orch today:** `run_agent_session` (`src/engine/runner/session.rs:26`) waits for tmux to finish (`session.rs:67`), and only then parses the output with the agent's `classify_error` (`agents/mod.rs:893`). A CLI that prints a usage limit, an auth failure or `model not found` and then hangs is caught only by silence detection or the task timeout. Its slot and the time are spent until then.

**Change:**

- `session.rs`: while waiting, read the new lines of the attempt's output file every poll, as `orch stream` already does (`src/cli/stream.rs`).
- `agents/mod.rs`: a trait method that classifies one output line with the patterns `classify_error` already uses (`detect_rate_limit`, `detect_auth_error`, `ModelUnavailable`). It returns only errors that end the run.
- On a fatal error the session is killed and the error goes to the same cooldown calls as an error found after the run.

**Why it is better:** a run that already failed ends at once instead of minutes later. The agent or model goes into cooldown sooner, and the task is re-routed sooner. It uses each agent's existing patterns, so nothing names a model.

**Cost:** a file read per poll per running task, which orch's poll loop already pays for the tmux pane. A line that matches a pattern but is not fatal could end a good run, so only errors the classifier marks as ending the run count, and the tests use the existing fixtures from `codex.rs` and `claude.rs`.

**Rejected:** a lower silence threshold. It would also end long, quiet builds.

## 6. A live diff for each running task

**Infinity:** after each command the sandbox pushes a diff view of the thread's changes, keeps the latest one, and replays it to a client that attaches (`docs/docs/rap/spec/server/view-updates.md`).

**Orch today:** an operator follows a running agent with `orch task watch <id>` (`src/main.rs:842`), `orch stream`, or tmux pane capture (`src/channels/capture.rs`). `orch task watch` runs the same code as `orch stream`, `cli::stream::run` (`main.rs:1324`), and also follows the review. `orch log watch` follows the service's own log with `tail -f` (`src/cli/mod.rs:153`). All of them show what the agent or orch prints. None of them shows the files an agent changed.

**Change:**

- `capture.rs`: on its tick, run `git diff --stat` against the task's base in each running worktree, and publish the result when it changed.
- `src/engine/events.rs`: a diff event on the `EventBus` (`events.rs:93`) beside `TaskEvent`, so the localhost websocket carries it.
- `src/cli/stream.rs`: when it attaches, `cli::stream::run` prints the latest diff and each update after. `orch task watch` and `orch stream` both get it.

**Why it is better:** the operator sees which files an agent touched while it works, and can stop one that strays from the task before it opens a PR.

**Cost:** one `git diff --stat` per running task per capture tick, bounded by the number of running tasks. It is information only and changes no task.

**Rejected:** posting the diff to the issue as the agent works. It is noise for people and costs a GitHub write per update.

## Order

1 and 2 first. They are small, independent and verified against the code. 3 is done. Then 5, then 4 and 6. 4 matters once `workflow.sandbox: os` is on for real projects, and 6 is a convenience.

## Not taken

| Infinity idea | Why not |
|---------------|---------|
| The GitHub Events API to skip unchanged repos | Orch's ETag caching already makes an unchanged poll free, and GitHub documents that events lag by 30 s to 6 h |
| jj workspaces squashed into the parent | Orch's work goes through PRs, and each child opens its own |
| Model providers as separate processes | Orch's agents are CLIs, and its per-agent runners already play this role |
| Tool servers started lazily per session | Orch runs no tool servers |
| A message injected into a running agent | Headless `-p` agents cannot take input. Orch already forwards channel text to tmux for interactive ones |
