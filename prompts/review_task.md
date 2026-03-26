## Review Task #{{TASK_ID}}: {{TASK_TITLE}}

You are reviewing a PR created by an AI agent. Complete ALL steps in order.

### Step 1: Rebase onto default branch

Keep the branch up to date — other PRs may have merged since this was created:
1. The service has pre-fetched remote refs. Rebase onto the default branch:
   - `git rebase origin/{{DEFAULT_BRANCH}}`
2. If there are conflicts:
   - Resolve each conflict by understanding both sides of the change
   - `git add <resolved files>` then `git rebase --continue`
   - If a conflict is too complex to resolve safely, set decision = `request_changes`

**Do NOT push** — the orchestrator handles pushing after review completes.

### Step 2: Run CI checks locally

1. Read `.github/workflows/` to identify what the CI pipeline runs (lint, format, test, build, etc.)
2. Execute those exact commands locally — do not guess or hardcode language-specific commands
3. Run only the checks that apply to the changed code (skip deploy/publish steps)

If ANY check fails, try to fix it yourself:
- Apply auto-fixers if available (e.g. formatter --fix, linter --fix)
- Fix errors directly if straightforward

Commit your fixes and re-run checks. If you cannot fix a failure, decision = `request_changes`. Do NOT push — the orchestrator handles that.

### Step 2b: Verify GitHub CI status on the PR

Local checks can diverge from CI (e.g. different toolchain versions). After local checks pass, also verify GitHub CI:

```bash
gh pr checks {{PR_NUMBER}} --watch --fail-fast
```

If GitHub CI has failures that your local checks missed, fix them, commit, and re-run. If CI is still pending after 5 minutes, proceed with local results but note it in your review.

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
- **approve**: CI checks pass locally, code meets requirements, no major issues
- **request_changes**: CI fails, there are bugs, or the code doesn't meet requirements

You MUST run CI checks. Do NOT just read the diff and approve. Actually execute the commands.
