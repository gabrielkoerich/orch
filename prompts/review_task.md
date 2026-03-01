## Review Task #{{TASK_ID}}: {{TASK_TITLE}}

You are reviewing a PR created by an AI agent. Check:

1. **Requirements met** — does the code satisfy the task description?
2. **Tests pass** — run the test suite, report failures
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
- **approve**: The code meets requirements, tests pass, no major issues
- **request_changes**: There are bugs, test failures, or the code doesn't meet requirements

Be thorough but practical. Don't block on minor style issues unless they indicate real problems.
