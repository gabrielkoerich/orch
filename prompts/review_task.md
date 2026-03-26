## Review Task #{{TASK_ID}}: {{TASK_TITLE}}

You are reviewing a PR created by an AI agent. Complete ALL steps in order.

### Step 1: Rebase onto default branch

Keep the branch up to date — other PRs may have merged since this was created:
1. Fetch remote refs and rebase onto the default branch:
   - `git fetch origin && git rebase origin/{{DEFAULT_BRANCH}}`
2. If there are conflicts:
   - Resolve each conflict by understanding both sides of the change
   - `git add <resolved files>` then `git rebase --continue`
   - If a conflict is too complex to resolve safely, set decision = `request_changes`

**Do NOT push** — the orchestrator handles pushing after review completes.

### Step 2: Run local checks and fix failures

1. Read `.github/workflows/` to identify what the CI pipeline runs (lint, format, test, build, etc.)
2. Execute those exact commands locally — do not guess or hardcode language-specific commands
3. Run only the checks that apply to the changed code (skip deploy/publish steps)

If ANY check fails, try to fix it yourself:
- Apply auto-fixers if available (e.g. formatter --fix, linter --fix)
- Fix errors directly if straightforward

Commit your fixes and re-run checks.

If you cannot fix a failure, **before setting `request_changes`, check whether it pre-exists on `{{DEFAULT_BRANCH}}`**:

1. List files changed by this PR: `git diff origin/{{DEFAULT_BRANCH}} --name-only`
2. If the failing test/check does not touch any of those files, verify on the base branch:
   ```bash
   git stash
   cargo nextest run <failing_test_name>   # or the equivalent failing command
   git stash pop
   ```
3. If the failure reproduces on `{{DEFAULT_BRANCH}}` (pre-existing) → it is **not** caused by this PR.
   - Do NOT block the PR for a pre-existing failure.
   - Note it in your review summary and proceed with the rest of the review.
4. If the failure only occurs with the PR changes → it is a regression. Set decision = `request_changes`.

Do NOT push — the orchestrator handles that.

### Step 2b: Verify GitHub CI status on the PR

```bash
timeout 300 gh pr checks {{PR_NUMBER}} --watch --fail-fast || true
```

**If all required checks pass on GitHub CI AND the branch is rebased on the latest default branch, skip local test runs entirely** — CI runs in a clean, reproducible environment. Local worktrees have sandbox restrictions and shared state paths that cause false failures.

If you rebased in Step 1 and the rebase changed anything, the orchestrator will push and CI will re-run. In that case, wait for the new CI run or note in your review that CI needs to re-run post-rebase.

Only run local checks if:
- CI has not run yet (no checks reported)
- CI is still pending after 5 minutes
- You need to verify a fix you applied during rebase

If you do run local checks, read `.github/workflows/` to identify what CI runs and execute those commands. Do NOT request changes for local-only test failures when GitHub CI is green.

### Step 3: Check architecture alignment

Before reviewing the diff, read any project spec, architecture or plan on `docs/` and `AGENTS.md`/`CLAUDE.md` if present.

Flag `request_changes` if the PR:
- Conflicts with settled decisions documented in `AGENTS.md`/`CLAUDE.md`
- Reimplements something the plan says is already done or intentionally out of scope
- Solves a problem that was deliberately designed to work a specific way

### Step 4: Review the code

1. **Requirements met** — does the code satisfy the task description?
2. **Scope** — is it doing only what was asked? Reject unnecessary refactors or scope creep.
3. **Code quality** — no obvious bugs, security issues, or regressions
4. **Completeness** — all files committed, no TODOs left behind
5. **Simplicity** — is the solution as simple as it can be? Flag unnecessary complexity.

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

You MUST output the JSON block below even if you already ran this review earlier.
Do NOT respond with prose summaries.

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
- **approve**: GitHub CI passes, branch is rebased, code meets requirements, no major issues
- **request_changes**: GitHub CI fails, there are bugs, scope creep, or the code doesn't meet requirements. Do NOT request changes for local-only test failures when CI is green.
