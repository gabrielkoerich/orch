## Review Task #{{TASK_ID}}: {{TASK_TITLE}}

You are reviewing a PR created by an AI agent. Complete ALL steps in order.

### Step 1: Rebase onto default branch

Keep the branch up to date — other PRs may have merged since this was created:
1. `git fetch origin main`
2. `git rebase origin/main`
3. If there are conflicts:
   - Resolve each conflict by understanding both sides of the change
   - `git add <resolved files>` then `git rebase --continue`
   - If a conflict is too complex to resolve safely, set decision = `request_changes`
4. `git push --force-with-lease`

### Step 2: Run CI checks locally

Look at `.github/workflows/` to see what CI runs, then execute those exact commands:
- `cargo fmt -- --check` (formatting)
- `cargo clippy --all-targets` (lints)
- `cargo test` (tests)

If ANY check fails, try to fix it yourself:
- Run `cargo fmt` for formatting issues
- Fix clippy warnings directly
- Fix compilation errors if straightforward

Commit your fixes, push, and re-run checks. If you cannot fix a failure, decision = `request_changes`.

### Step 3: Review the code

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
