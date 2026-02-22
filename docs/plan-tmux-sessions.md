# Plan: Interactive Agent Sessions via tmux

> **Status**: Planning
> **Goal**: Run agents inside tmux sessions so the user can watch live and interact with running agents.

## Problem

Agents currently run in non-interactive "print mode" (`claude -p`, `codex exec`, `opencode run`). The user has zero visibility into what the agent is doing until it finishes. If an agent gets stuck, there's no way to guide it. The orchestrator is a black box.

## Proposed Architecture

Each agent run gets its own tmux session. The orchestrator manages session lifecycle, the user can attach/detach freely.

```
orchestrator serve
  └── poll.sh
        └── run_task.sh (task 42)
              └── tmux new-session -d -s "orch-42"
                    └── claude --model opus ... (interactive, NOT -p)

User:
  $ orch task attach 42     → tmux attach -t orch-42
  $ orch task stream 42     → tmux capture-pane / pipe-pane (read-only)
  $ orch task list --live   → shows active tmux sessions
```

## Design

### 1. Session Naming

```
orch-{task_id}       — e.g., orch-42, orch-86
```

Simple, predictable, easy to tab-complete. Check for conflicts: `tmux has-session -t "orch-42" 2>/dev/null`.

### 2. Agent Invocation (run_task.sh)

**Current** (non-interactive):
```bash
RESPONSE=$(claude -p --model "$MODEL" --output-format json \
  --append-system-prompt "$SYSTEM_PROMPT" "$MESSAGE" 2>"$STDERR_FILE")
```

**Proposed** (tmux session):
```bash
SESSION="orch-${TASK_ID}"
RESPONSE_FILE="${STATE_DIR}/${FILE_PREFIX}-response.json"
DONE_MARKER="${STATE_DIR}/${FILE_PREFIX}-done"

# Create a wrapper script that runs the agent and signals completion
RUNNER="${STATE_DIR}/${FILE_PREFIX}-runner.sh"
cat > "$RUNNER" <<RUNNER_EOF
#!/usr/bin/env bash
set -euo pipefail

# Write the prompt to a temp file for claude to read
PROMPT_FILE="\$(mktemp)"
cat > "\$PROMPT_FILE" <<'PROMPT'
${AGENT_MESSAGE}
PROMPT

# Run agent interactively with initial message piped via --resume or direct
claude --model "$MODEL" \
  --allowedTools ... \
  --append-system-prompt "$SYSTEM_PROMPT" \
  "\$(cat "\$PROMPT_FILE")"

# When claude exits (user types /exit or agent finishes), signal done
# Save the conversation for later resume
touch "$DONE_MARKER"
RUNNER_EOF
chmod +x "$RUNNER"

# Start tmux session
tmux new-session -d -s "$SESSION" -x 200 -y 50 "$RUNNER"

# Wait for completion (poll the done marker)
while [ ! -f "$DONE_MARKER" ]; do
  sleep 5
  # Check if tmux session still exists
  if ! tmux has-session -t "$SESSION" 2>/dev/null; then
    break
  fi
done
```

### 3. Capturing Output

Challenge: in interactive mode, claude doesn't output JSON to stdout. We need to extract results differently.

**Options:**

**A. Use `-p` mode inside tmux (watch-only, no interaction)**
```bash
tmux new-session -d -s "$SESSION" -- bash -c \
  'claude -p --model opus --output-format json ... | tee /path/to/response.json'
```
User can `tmux attach` to watch but can't type. Output is captured normally.
To interact, kill the session and resume with `claude --resume`.

**B. Interactive mode + conversation export**
Run claude interactively. When it exits, use `claude --resume $SESSION_ID --print "export summary as JSON"` to extract structured output. Or parse the tmux pane buffer.

**C. Hybrid: `-p` mode by default, `--interactive` flag for tmux**
```bash
# Default: autonomous (current behavior, fast)
orch task run 42

# Interactive: tmux session, user can watch and type
orch task run 42 --interactive

# Attach to running session (works in both modes)
orch task attach 42
```

**Recommendation**: Option C — keep `-p` mode as default for autonomous operation, add `--interactive` flag for tmux. The `attach` command works for both: in `-p` mode it shows a read-only tail of output, in interactive mode it's a full tmux attach.

### 4. CLI Commands

```bash
# Run interactively in tmux
orch task run 42 --interactive
orch task run 42 -i

# Attach to running agent session
orch task attach 42
# → tmux attach-session -t orch-42
# Detach with Ctrl-B D (standard tmux)

# Stream output (read-only, works without tmux attach)
orch task stream 42
# → tmux pipe-pane -t orch-42 or tail -f output file

# List active agent sessions
orch task live
# Shows:
#   SESSION     TASK  AGENT   STARTED      TITLE
#   orch-42     42    claude  5m ago       Enhance GIT_DIFF for retries
#   orch-86     86    claude  2m ago       Add environment validation

# Kill a running agent session
orch task kill 42
# → tmux kill-session -t orch-42
# Sets task status back to needs_review
```

### 5. Autonomous Mode with tmux (best of both worlds)

Even in autonomous mode (`-p`), wrap the agent in a tmux session for observability:

```bash
SESSION="orch-${TASK_ID}"

# Run -p mode inside tmux so user can watch
tmux new-session -d -s "$SESSION" -x 200 -y 50 -- bash -c "
  claude -p --model $MODEL \
    --output-format json \
    --append-system-prompt '$SYSTEM_PROMPT' \
    '$MESSAGE' \
    > '$RESPONSE_FILE' 2>'$STDERR_FILE'
  echo 'EXIT_CODE='\$? > '$DONE_MARKER'
"

# Wait for completion
while ! tmux has-session -t "$SESSION" 2>/dev/null || [ ! -f "$DONE_MARKER" ]; do
  sleep 5
done

# Read response
RESPONSE=$(cat "$RESPONSE_FILE")
```

This gives us:
- Same autonomous behavior (JSON output, orchestrator manages lifecycle)
- User can `tmux attach -t orch-42` to watch the agent work in real-time
- Detach without interrupting (`Ctrl-B D`)
- Multiple terminal windows can attach to the same session

### 6. Interactive Mode Flow

When `--interactive` or `-i` is passed:

```bash
SESSION="orch-${TASK_ID}"

# Start claude in interactive mode (no -p)
tmux new-session -d -s "$SESSION" -x 200 -y 50 -- bash -c "
  cd '$WORKTREE_DIR'
  export GIT_AUTHOR_NAME='${TASK_AGENT}[bot]'
  claude --model $MODEL \
    --allowedTools ... \
    --append-system-prompt '$SYSTEM_PROMPT'
"

# Send the initial message as the first input
sleep 2  # wait for claude to initialize
tmux send-keys -t "$SESSION" "$AGENT_MESSAGE" Enter

# Attach the user's terminal
tmux attach-session -t "$SESSION"

# After user detaches or claude exits:
# - Parse results from tmux buffer or session export
# - Update task status
```

The user sees claude start up, receives the task prompt, and begins working. The user can:
- Watch silently
- Type additional instructions ("focus on the tests first", "skip the docs")
- Ask questions ("what's blocking you?")
- Detach and come back later
- Let it finish autonomously

### 7. Agent-Specific Behavior

**Claude Code**:
- Interactive: `claude --model $MODEL --append-system-prompt ...`
- Send initial message via `tmux send-keys`
- Supports `--resume $SESSION_ID` for reconnecting to previous conversations
- Capture session ID from `~/.claude/projects/` for later resume

**Codex**:
- Codex doesn't have an interactive mode — always `codex exec`
- Run in tmux for observability but no interaction
- `-p` mode only, user can watch but not type

**OpenCode**:
- `opencode` has an interactive TUI mode
- `tmux new-session -d -s "$SESSION" -- opencode`
- Send message via `tmux send-keys`
- User can interact natively

### 8. Session Resume (Claude)

Claude Code persists conversation state. After a session ends or is interrupted:

```bash
# Find the session ID from the last run
SESSION_ID=$(ls -t ~/.claude/projects/*/conversations/*.jsonl | head -1 | ...)

# Resume interactively
orch task resume 42
# → claude --resume $SESSION_ID
```

This is powerful for debugging: if an agent failed, you can resume its exact conversation and guide it past the error.

### 9. Serve Mode Integration

In `serve.sh`, the poll cycle needs to be tmux-aware:

```bash
# Check if a task already has a running tmux session
if tmux has-session -t "orch-${TASK_ID}" 2>/dev/null; then
  # Skip — agent is still working
  continue
fi
```

This replaces/supplements the current lock mechanism. A tmux session IS the lock.

### 10. Session Cleanup

When a tmux session exits (agent finished or crashed):
- Read exit code from done marker
- Parse response from output file
- Update task status (via GitHub labels in v2, or SQLite in current)
- Remove tmux session if still hanging: `tmux kill-session -t "orch-$TASK_ID" 2>/dev/null`
- Clean up temp files (runner script, done marker)

On orchestrator shutdown (`serve.sh` exit trap):
- List all `orch-*` sessions: `tmux list-sessions -F '#{session_name}' | grep '^orch-'`
- Either kill them or leave them running (user preference)
- Default: leave running — user can reattach later

## Files to Modify

| File | Change |
|------|--------|
| `scripts/run_task.sh` | Wrap agent invocation in tmux session |
| `scripts/poll.sh` | Check for existing tmux sessions before dispatching |
| `scripts/serve.sh` | Session cleanup on shutdown |
| `justfile` | Add `task attach`, `task stream`, `task live`, `task kill`, `task resume` recipes |
| `scripts/lib.sh` | Add tmux helper functions (`session_name`, `session_exists`, `session_attach`) |

## New Files

| File | Purpose |
|------|---------|
| `scripts/task_attach.sh` | Attach to running agent tmux session |
| `scripts/task_stream.sh` | Tail agent output (read-only) |
| `scripts/task_live.sh` | List active tmux sessions |
| `scripts/task_kill.sh` | Kill a running agent session |
| `scripts/task_resume.sh` | Resume a previous claude conversation |

## Open Questions

1. **Default mode**: Should autonomous tasks always get tmux sessions (for observability), or only when `--interactive` is passed? tmux-always adds a dependency but gives free observability.
2. **tmux dependency**: Is tmux always available? On brew-managed macOS yes. On Linux servers maybe not. Fallback to `screen` or `nohup`?
3. **Window size**: tmux sessions need a terminal size. `200x50` is reasonable but some agents may render differently. Configurable?
4. **Multiple users**: If multiple people attach to the same session, tmux handles this natively (shared session). Is this desirable?
5. **Codex interaction**: Codex has no interactive mode. Worth wrapping in tmux anyway for watch-only? Or skip tmux for codex?
6. **Output parsing in interactive mode**: How to reliably extract structured results (summary, accomplished, files changed) from an interactive session? Options: post-session `--resume` query, tmux buffer parsing, or just accept that interactive mode produces less structured output.
