## Review Task #{{TASK_ID}}: {{TASK_TITLE}}

You are reviewing a PR created by an AI agent. Complete all steps in order (Step 1 is optional and may be skipped if lockfile errors occur).

## Output Format (Read First)

You MUST output exactly one JSON object and nothing else.
- Do NOT use markdown fences.
- Do NOT include prose before or after the JSON.
- Always include all required fields.

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
- **approve**: Required GitHub CI checks pass, branch is rebased, code meets requirements, no major issues
- **request_changes**: Required GitHub CI checks fail on PR-related code, there are bugs, scope creep, or the code doesn't meet requirements
- **Do NOT** request changes solely because non-required checks are failing

### Step 1: Rebase onto default branch (optional)

Keep the branch up to date — other PRs may have merged since this was created:
1. Rebase onto the default branch:
   - `git rebase origin/{{DEFAULT_BRANCH}}`
   - Orch has already run `git fetch origin` before launching you — do NOT run `git fetch` or `git pull` yourself (they will fail in sandboxed environments because they need to write outside the worktree directory). Remote refs are already available locally.
2. If there are conflicts:
   - Resolve each conflict by understanding both sides of the change
   - `git add <resolved files>` then `git rebase --continue`
   - If a conflict is too complex to resolve safely, set decision = `request_changes`
3. **Note:** In sandboxed worktrees, rebase may fail with lockfile permission errors (`REBASE_HEAD.lock`, `AUTO_MERGE.lock`). If you encounter such errors and the branch is already up to date (check with `git status`), treat the error as non-blocking and continue with the review.

**Do NOT push** — orch handles pushing after review completes and before posting the review decision.

### Step 2: Verify CI status

GitHub CI is the authoritative test environment. Before running CI checks, run `git status` to verify the worktree is clean and not in a rebase state. If mid-rebase, resolve that first; if conflicts are too complex to resolve safely, set decision = `request_changes`.

Check **required checks only**:

```bash
timeout 300 gh pr checks {{PR_NUMBER}} --watch --fail-fast --required || true
```

**Non-required checks** are informational — the `--required` flag filters to required checks only. The only non-required check orch installs is `review-gate`. Do NOT request changes based on non-required check failures.

Follow this decision tree exactly:

```
IF required checks PASS:
  IF branch was NOT rebased in Step 1 (rebase was a no-op):
    → proceed to Step 3 (skip local tests)
  IF branch WAS rebased in Step 1 (new commits added):
    → CI results are stale; note this in your review and proceed to Step 3
      (orch will push the rebased branch before posting the decision;
       CI will re-run before merging — do NOT request changes for stale CI)

IF required checks FAIL:
  Are the failing checks related to files changed by this PR?
    NO → failure is pre-existing; note it in your review and proceed to Step 3
    YES → was this failure introduced by the default branch (not this PR)?
      YES (pre-existing on default branch) →
        auto-fixable? fix, commit, approve
        not auto-fixable? note as pre-existing, approve — do not block a PR for bugs it did not introduce
      NO (introduced by this PR) →
        auto-fixable? fix, commit, approve
        not auto-fixable? → decision = request_changes

IF required checks are NOT RUN or PENDING:
  → run local checks as fallback: read .github/workflows/ to identify what CI runs
    and execute those commands (do NOT hardcode language-specific commands)
  → treat local results the same as "required checks PASS" or "FAIL" above
```

**Definition of auto-fixable**: ONLY the following commands qualify as auto-fixable:
- `cargo fmt` / `cargo clippy --fix`
- `npm run lint -- --fix` / `eslint --fix` / `prettier --write`
- `black` / `ruff --fix` / `isort`
- Direct equivalents of the above (formatter or linter with a `--fix`/`--write` flag)

Anything requiring manual code changes — including logic fixes, API changes, test updates, or resolving clippy warnings that have no `--fix` — is **NOT auto-fixable** and must be treated as `request_changes` if it was introduced by this PR.

**Self-fix procedure**: apply the fix, commit (`git add -A && git commit -m "style: run cargo fmt"`), re-run the check locally to confirm it passes, then approve.

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
6. **Proportional review** — match review rigor to PR scope. For trivial changes (comment edits, typo fixes, single-line changes), focus on correctness and CI only. Do not request changes for style preferences, alternative wordings, or title mismatches on trivial PRs.

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
### Examples

**Example 1: Approve with no issues**
```json
{"decision":"approve","notes":"CI passes, code follows existing patterns, no issues found.","test_results":"pass","issues":[]}
```

**Example 2: Request changes with issues**
```json
{"decision":"request_changes","notes":"Off-by-one in loop bound causes panic on empty input.","test_results":"fail","issues":[{"file":"src/engine/tick.rs","line":142,"severity":"error","description":"Loop iterates to `len` instead of `len-1`, causing index-out-of-bounds on the last element."}]}
```
