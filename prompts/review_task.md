## Review Task #{{TASK_ID}}: {{TASK_TITLE}}

You are reviewing a PR created by an AI agent. Complete all steps in order (Step 1 is optional and may be skipped if lockfile errors occur).

### Step 1: Rebase onto default branch (optional)

Keep the branch up to date — other PRs may have merged since this was created:
1. Fetch remote refs and rebase onto the default branch:
   - `git fetch origin && git rebase origin/{{DEFAULT_BRANCH}}`
2. If there are conflicts:
   - Resolve each conflict by understanding both sides of the change
   - `git add <resolved files>` then `git rebase --continue`
   - If a conflict is too complex to resolve safely, set decision = `request_changes`
3. **Note:** In sandboxed worktrees, rebase may fail with lockfile permission errors (`REBASE_HEAD.lock`, `AUTO_MERGE.lock`). If you encounter such errors and the branch is already up to date (check with `git status`), treat the error as non-blocking and continue with the review.

**Do NOT push** — orch handles pushing after review completes and before posting the review decision.

### Step 2: Verify CI status

GitHub CI is the authoritative test environment. Check it:

```bash
timeout 300 gh pr checks {{PR_NUMBER}} --watch --fail-fast || true
```

- **CI passes AND branch is up to date** (Step 1 rebase was a no-op or already applied) → proceed to Step 3 (skip local test runs)
- **CI passes BUT branch was rebased** (Step 1 changed commits) → CI results are stale. Note in your review that CI needs to re-run post-rebase and proceed with code review. Orch will push the rebased branch (before posting the review decision) and CI will re-run before merging.
- **CI fails** → check if the failure is related to files in this PR. If not, it's pre-existing — note it and proceed. If it is, set decision = `request_changes`
- **CI not run yet or pending** → run local checks as fallback: read `.github/workflows/` to identify what CI runs and execute those commands. Do NOT hardcode language-specific commands

**Do NOT request changes for local-only test failures when GitHub CI is green.**

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
- **request_changes**: GitHub CI fails on PR-related code, there are bugs, scope creep, or the code doesn't meet requirements
