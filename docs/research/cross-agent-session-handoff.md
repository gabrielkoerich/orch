# Cross-Agent Session Handoff — Research Findings

> Research for [#1149](https://github.com/gabrielkoerich/orch/issues/1149) Part 2

## Session Storage Formats

### Claude Code

- **Location:** `~/.claude/projects/<encoded-path>/<session-uuid>.jsonl`
- **Format:** JSONL (one event per line)
- **Event types:** `user`, `assistant`, `queue-operation`, `last-prompt`, `system`
- **Data:** Full conversation, thinking blocks (with signatures), tool use/results, token usage, model, cwd, git branch
- **System prompt:** NOT stored — reconstructed from CLAUDE.md files
- **Resume:** `claude --continue`, `claude -r <session-id>`, `claude --fork-session`
- **Export/Import:** None — read JSONL files directly
- **Session metadata:** `~/.claude/sessions/<pid>.json`

### Codex (OpenAI)

- **Location:** Dual storage
  - JSONL: `~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<uuid>.jsonl`
  - SQLite: `~/.codex/state_5.sqlite` (threads table)
- **Event types:** `session_meta`, `turn_context`, `response_item`, `event_msg`
- **Data:** Full system prompts, developer instructions, skills, permissions, tool calls, model responses, token usage, git context, sandbox config
- **Resume:** `codex resume [--last | <id>]`, `codex fork [--last | <id>]`
- **Export/Import:** None — read JSONL + SQLite directly

### OpenCode

- **Location:** SQLite at `~/.local/share/opencode/opencode.db`
- **Schema:** `session` → `message` → `part` (normalized three-table)
- **Part types:** `text`, `tool`, `step-start`, `step-finish`, `reasoning`, `patch`, `compaction`, `file`
- **Data:** Full conversation, tool calls with I/O, reasoning, patches/diffs, compaction summaries, token usage, cost, model/provider
- **Resume:** `opencode -c`, `opencode -s <id>`, `opencode --fork`
- **Export:** `opencode export [session-id]` → JSON
- **Import:** `opencode import <file|url>`
- **Direct SQL:** `opencode db [query]`, `opencode db path`

## Common Data Across All Agents

| Field | Claude | Codex | OpenCode |
|-------|--------|-------|----------|
| User messages | ✓ | ✓ | ✓ |
| Assistant text | ✓ | ✓ | ✓ |
| Tool calls (name + input + output) | ✓ | ✓ | ✓ |
| Model name | ✓ | ✓ | ✓ |
| Token usage | ✓ | ✓ | ✓ |
| Working directory | ✓ | ✓ | ✓ |
| Git context | ✓ | ✓ | ✓ |
| Timestamps | ✓ | ✓ | ✓ |

## Key Differences

| Aspect | Claude Code | Codex | OpenCode |
|--------|------------|-------|----------|
| Format | JSONL files | JSONL + SQLite | SQLite |
| Export cmd | None | None | `opencode export` |
| Import cmd | None | None | `opencode import` |
| System prompt stored | No | Yes (in rollout) | Yes (as message parts) |
| Thinking/reasoning | Yes (with signatures) | N/A | Yes (reasoning parts) |
| Tool format | Anthropic tool_use/tool_result | OpenAI function calling | Custom tool/step parts |
| Session ID format | UUID | UUID | `ses_*` custom ID |
| Fork support | `--fork-session` | `codex fork` | `--fork` |

## Feasibility Assessment

### What's transferable

Cross-agent transfer is feasible at the **conversation content level**:
- User messages and assistant text responses transfer cleanly
- Tool calls can be summarized as context (input/output pairs)
- Git context (branch, working dir) transfers directly

### What's NOT transferable

- **Tool replay:** Each agent has different tool implementations — tool calls from one agent can't be replayed on another
- **Thinking tokens:** Claude's thinking blocks include cryptographic signatures; can't be re-created
- **System prompts:** Agent-specific; would need to be regenerated for the target agent
- **Context window state:** Token counts differ; agents may have different context limits

### Import/Export Maturity

| Agent | Can export? | Can import? | Effort to add |
|-------|-------------|-------------|---------------|
| Claude Code | No native cmd, read JSONL | No — write JSONL to disk | Medium (well-documented format) |
| Codex | No native cmd, read JSONL + SQLite | No — write JSONL + SQLite | High (dual storage, schema migration) |
| OpenCode | `opencode export` | `opencode import` | Zero (already works) |

### Practical Use Cases

1. **Rate limit handoff:** Agent A hits rate limit → dump conversation summary → continue on Agent B
   - Feasible via orch's existing sidecar + prompt injection. No session import needed — just inject the conversation history as context into the system prompt.

2. **Strength-based handoff:** Claude for architecture → Codex for implementation
   - Best approach: use orch's worktree + git as the transfer medium. Agent A commits work + writes a handoff doc. Agent B reads the doc + git log and continues.
   - Session-level transfer adds little value here since the code is the primary artifact.

3. **Review → fix handoff:** Review agent passes context to fix agent
   - Already implemented in orch via `pr_review_context` field in sidecar. The fix agent reads the review comments and PR diff.

## Recommended Approach

Rather than building a universal session interchange format (high effort, fragile), orch should:

1. **Use git + file system as the handoff medium** — agents already share worktrees
2. **Use prompt injection for context transfer** — summarize previous agent's work into the next agent's system prompt (already done for review cycles)
3. **Build an `orch session export` command** that reads any agent's native format and produces a human-readable summary with key decisions, changes made, and outstanding items
4. **Only build native import for OpenCode** (it already supports it) — for Claude and Codex, prompt-based context injection is more robust than trying to reconstruct their internal session format
