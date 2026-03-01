## Review Task #{{TASK_ID}}: {{TASK_TITLE}}

You are reviewing a PR created by an AI agent. You MUST do ALL of the following:

### Step 1: Run CI checks locally

Look at `.github/workflows/` to see what CI runs, then run those exact commands in your worktree. For example:
- `cargo fmt -- --check` (formatting)
- `cargo clippy --all-targets` (lints)
- `cargo test` (tests)

If ANY check fails → decision is `request_changes`. Period.

If you can fix the failure yourself (e.g., run `cargo fmt`, fix a clippy warning), do it, commit, and push. Then re-run checks to verify.

### Step 2: Review the code

1. **Requirements met** — does the code satisfy the task description?
2. **Code quality** — no obvious bugs, security issues, or regressions
3. **Completeness** — all files committed, no TODOs left behind

### Task Description
{{TASK_BODY}}

{{#if AGENT_SUMMARY}}
### Agent Summary
{{AGENT_SUMMARY}}

{{/if}}
{{#if GIT_DIFF}}
### Changes
```diff
{{GIT_DIFF}}
```

{{/if}}
{{#if GIT_LOG}}
### Commits
{{GIT_LOG}}

{{/if}}
## Output Format

```json
{
  "decision": "approve|request_changes",
  "notes": "Detailed review feedback",
  "test_results": "pass|fail|skipped",
  "issues": [
    {
      "file": "src/foo.rs",
      "line": 42,
      "severity": "error|warning",
      "description": "What's wrong and how to fix it"
    }
  ]
}
```

Decision rules:
- **approve**: CI checks pass locally, code meets requirements, no major issues
- **request_changes**: CI fails, there are bugs, or the code doesn't meet requirements

You MUST run CI checks. Do NOT just read the diff and approve. Actually execute the commands.
