# Research: Mid-run messages to a working agent

> Status: Research
> Issue: [#4104](https://github.com/gabrielkoerich/orch/issues/4104)
> Date: 2026-11-17

A person watching a running task wants to tell its agent something before the run ends: smaller steps, skip the migration, leave a flaky test. Today they wait for review, or kill the task and retry it with a new body.

This note answers whether orch can deliver that message, and how. Building the feature is a separate issue.

Checked on 2026-11-17 against Claude Code 2.1.311, Codex 0.169.0, OpenCode v2.3.0, and Grok 1.0.88. Evidence is CLI `--help`, Anthropic headless and Agent SDK docs, orch's launch scripts, recorded NDJSON fixtures, and a dummy process launched like a task agent that received `tmux send-keys` during a sleep.

## Summary

Keys from `tmux send-keys` stay in the pane. The agent process reads its prompt from a file (or `--prompt-file`), so it never sees those keys. Channel free-text already calls `forward_to_tmux`, which is this path.

Every current CLI can take a later prompt if orch stops the process, keeps the worktree, and resumes the vendor session with the operator text. That needs orch to store the session or thread id that already appears in NDJSON. Each agent loses the in-flight turn.

Live steering without a restart is possible for Claude-family agents (JSONL on a kept-open stdin) and OpenCode (HTTP to its local server). Codex `queue` talks to an app-server daemon orch does not start. Grok's headless `--prompt-file` path has no mid-run stdin.

## How orch launches agents

Task runs spawn a tmux session whose shell is `runner.sh`. That script runs `script -q /dev/null /bin/bash agent_cmd.sh | tee output.json` so stdout looks like a tty (`src/engine/runner/agent.rs`). `agent_cmd.sh` is the vendor CLI with stderr split off.

The prompt is closed at start:

| Agent | Command shape | How the prompt arrives |
|---|---|---|
| claude, kimi, minimax | `claude -p --verbose --output-format stream-json --append-system-prompt sys < msg_file` | stdin redirected from a file, then EOF |
| codex | `codex exec ... --json - < <(cat sys msg)` | process substitution, then EOF |
| opencode | `opencode run --format json - < <(cat sys msg)` | process substitution, then EOF |
| grok | `grok --prompt-file msg --system-prompt-override "$(cat sys)"` | flags only |

Kimi and MiniMax are `claude` wrappers that set `ANTHROPIC_BASE_URL`. They inherit Claude's flags.

Control chat already passes `--session-id` / `--resume` for claude, kimi, minimax, and grok (`insert_session_flag` in `src/engine/agents/direct.rs`). Task runs skip that. `src/engine/runner/fallback.rs` treats a stale-session error on a task as a generic failure because tasks do not use those flags.

## send-keys

`forward_to_tmux` in `src/engine/service/channel_handler.rs` sends a TaskSession free-text body with `tmux send-keys -t SESSION TEXT Enter` (`src/channels/tmux.rs`). Slash commands run in orch. Unknown text is forwarded this way.

A dummy process was launched the same way orch launches agents: `python3 dummy.py < prompt.txt` inside `script | tee`. During an 8 second sleep, `tmux send-keys STEER_MSG Enter` went to the session.

The dummy's stdin was only `INITIAL_PROMPT`. `STEER_MSG` appeared in the pane capture and in the tee file, because `script` copies PTY keystrokes onto stdout.

The agent process never reads a forwarded channel message. The same keystrokes can inject a non-JSON line into `output.json`, which the parsers then have to skip.

## Live input during a run

### Claude, Kimi, MiniMax

`claude -p --input-format stream-json --output-format stream-json --verbose` reads JSONL user turns on stdin while it works. Anthropic's [Send messages to a running agent](https://code.claude.com/docs/en/agent-sdk/streaming-vs-single-mode) page says a message sent during tool calls is handled after those calls finish, in one combined response. `--replay-user-messages` echoes each stdin turn back on stdout so the caller can ack it.

The recorded `system/init` event in `tests/fixtures/claude_stream_success.jsonl` lists `interrupt_receipt_v1`, `interrupt_cancel_queued_v1`, `interrupt_send_now_v1`, and `interrupt_requeue_taken_v1`, and includes `messaging_socket_path`.

To use this, orch would keep stdin a pipe instead of `< msg_file`, write the first user turn, then later write more JSONL. The parser already waits for a `result` event, so extra turns fit if the process stays alive.

SIGTERM (`kill`) exits 143 and leaves the current turn unfinished ([headless docs](https://code.claude.com/docs/en/headless)). SIGINT ends the turn first. `CLAUDE_CODE_RESUME_INTERRUPTED_TURN=1` continues that turn on `--resume`.

Cost: a named pipe or control socket per task, a runner that leaves stdin open, and engine logic that a mid-run message is the same task.

### Codex

`codex exec` is one-shot. Stdin is the prompt, then EOF.

`codex queue --thread <id> --message <text> [--resume]` queues a follow-up. `--resume` tries the local daemon or a `--remote` app-server. Orch never starts that daemon, so `queue` has nothing to attach to during a live `exec`.

Live steer would mean running Codex under app-server and keeping `thread_id` from `thread.started`.

### OpenCode

`opencode run` is one-shot on stdin, same shape as Codex `exec`.

`opencode run -s <sessionID>` continues a session after the previous `run` exits.

A background `opencode serve --service` is often already listening. `opencode api` can POST to `/session/:id/message` or `prompt_async`, and abort with `/session/:id/abort`. `GET /api/session` on the observed local port returned `UnauthorizedError`; callers need the auth the local server expects.

`--standalone` gives the `run` a private server, which hides it from `opencode api` unless orch records that URL.

Orch does not store `sessionID` today. The field is already on every `step_start` line in `tests/fixtures/opencode_success.jsonl`.

### Grok

Headless task runs use `--prompt-file` and `--system-prompt-override`. Help lists no stdin reader for that path.

`--resume` / `-r` and `--session-id` / `-s` exist. `grok agent stdio`, `grok agent serve`, and `grok agent leader` (default socket `~/.grok/leader.sock`) are a different process model from the current runner.

### A file the agent reads between steps

The system prompt could tell the agent to `Read` a well-known file (for example `.orch/steer.md`) between tool calls. That works for every CLI. The agent only sees it on the next Read, so a message during a long test run sits until that run returns. The agent can also skip the file.

## Stop, keep the worktree, resume

This works if the operator can wait for the current tool to die. Git state in the worktree stays. The in-flight model turn is dropped.

| Agent | Resume command | Where the id already is | Lost on kill |
|---|---|---|---|
| claude, kimi, minimax | `claude -p --resume <uuid>` plus the operator text | `session_id` on `system/init` and `result` | The killed turn. Tool calls that had not finished. Thinking for that turn. |
| codex | `codex exec resume <thread_id> <prompt>` | `thread.started.thread_id` | Same, plus whatever `exec` does not flush before SIGTERM. |
| opencode | `opencode run -s <sessionID>` | `sessionID` on `step_start` | Same. Native `export` / `import` exist if orch wants a snapshot. |
| grok | `grok --resume <id>` plus `--prompt-file` | vendor session under `~/.grok/sessions`; orch does not parse it yet | Same. `--restore-code` restores a snapshot; leave it off so the worktree wins. |

Shared costs:

- Persist the id as soon as NDJSON prints it. Control chat already stores UUIDs under `control:session_id:{name}`. Tasks have no column for this.
- Treat a steer-kill as a continue, not as `AgentError` / cooldown. SIGTERM on Claude is exit 143.
- Replay history costs tokens. Claude's `--resume` `total_cost_usd` includes earlier runs.
- Half-written files stay in the worktree.

## Recommended first feature

`send-keys` types into the pane, which the agent does not read, and can dirty `output.json`. A first feature that works for every agent:

1. Parse and store the vendor session / thread id from the live NDJSON stream.
2. On an operator message to a running task, stop the tmux process group, keep the worktree and branch, and re-dispatch the same agent with resume plus the new text as the next prompt.
3. Put the box on the running-task page in the web UI, and on TaskSession once delivery works.

Live stdin JSONL for Claude-family, and HTTP for OpenCode, can follow if waiting for the kill is too slow.

Session storage formats are in [cross-agent-session-handoff.md](cross-agent-session-handoff.md) (#1149).
