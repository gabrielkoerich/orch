## Review Task #{{TASK_ID}}: {{TASK_TITLE}}

You are reviewing a PR created by an AI agent. Complete ALL steps in order.

### Step 1: Rebase onto default branch

Keep the branch up to date — other PRs may have merged since this was created:
1. Ensure the service has pre-fetched remote refs, then rebase onto the default branch:
   - `git rebase origin/main`  # service pre-fetches refs for agent worktrees
3. If there are conflicts:
   - Resolve each conflict by understanding both sides of the change
   - `git add <resolved files>` then `git rebase --continue`
   - If a conflict is too complex to resolve safely, set decision = `request_changes`
4. `git push --force-with-lease`

### Step 2: Run CI checks locally

Look at `.github/workflows/` to see what CI runs, then execute those exact commands:
- `cargo fmt -- --check` (formatting)
- `cargo clippy --all-targets -- -D warnings` (lints)
- `cargo test` (tests)

If ANY check fails, try to fix it yourself:
- Run `cargo fmt` for formatting issues
- Fix clippy warnings directly
- Fix compilation errors if straightforward

Commit your fixes, push, and re-run checks. If you cannot fix a failure, decision = `request_changes`.

### Step 3: Check architecture alignment

Before reviewing the diff, read any project spec or plan (`PLAN.md`, `SPEC.md`, `ARCHITECTURE.md`, `docs/`) and `AGENTS.md`/`CLAUDE.md` if present.

Flag `request_changes` if the PR:
- Conflicts with settled decisions documented in `AGENTS.md`/`CLAUDE.md`
- Reimplements something the plan says is already done or intentionally out of scope
- Solves a problem that was deliberately designed to work a specific way

### Step 4: Review the code

1. **Requirements met** — does the code satisfy the task description?
2. **Scope** — is it doing only what was asked? Reject unnecessary refactors or scope creep.
3. **Code quality** — no obvious bugs, security issues, or regressions
4. **Completeness** — all files committed, no TODOs left behind

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
