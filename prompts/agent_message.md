# Task #{{TASK_ID}}: {{TASK_TITLE}}

{{TASK_BODY}}

{{#if TASK_CONTEXT}}
## Previous Context

{{TASK_CONTEXT}}
{{/if}}

{{#if PARENT_CONTEXT}}
{{PARENT_CONTEXT}}
{{/if}}

{{#if ISSUE_COMMENTS}}
## Recent Comments

{{ISSUE_COMMENTS}}
{{/if}}

{{#if PR_REVIEW_CONTEXT}}
## PR Review Feedback

A reviewer has requested changes on your PR. Please address the following feedback:

{{PR_REVIEW_CONTEXT}}
{{/if}}

{{#if GIT_DIFF}}
## Current Changes (from previous attempt)

```diff
{{GIT_DIFF}}
```

{{/if}}

{{#if ATTEMPT_NUMBER}}
This is attempt #{{ATTEMPT_NUMBER}} (previous attempts may have made partial progress).
{{/if}}

{{#if MEMORY_SECTION}}
## Previous Attempts Memory

Learnings from previous task attempts (to help you avoid repeating mistakes):

{{MEMORY_SECTION}}
{{/if}}

---

**CRITICAL: YOUR FINAL OUTPUT MUST BE A JSON OBJECT.**

Output ONLY a valid JSON object as your final response — no prose, no explanation, no markdown fences. Required fields: `status`, `summary`, `accomplished`, `remaining`.

Valid status values and examples:

`done` — all work committed, tests pass:
```json
{"status":"done","summary":"Fixed the off-by-one in parser.rs","accomplished":["patched parser.rs","all 342 tests pass"],"remaining":[],"files_changed":["src/parser.rs"],"blockers":[],"reason":""}
```

`in_progress` — partial work committed, more remains:
```json
{"status":"in_progress","summary":"Added auth middleware, tests pending","accomplished":["created src/auth.rs","wired into router"],"remaining":["write tests","handle refresh tokens"],"files_changed":["src/auth.rs","src/router.rs"],"blockers":[],"reason":""}
```

`blocked` — missing info, unresolvable dependency, or clarifying question needed (post a comment on the issue first):
```json
{"status":"blocked","summary":"Cannot proceed without DB schema","accomplished":[],"remaining":["implement migration"],"files_changed":[],"blockers":["schema for users table not defined"],"reason":"Need clarification on the users table schema — posted question as issue comment"}
```

`needs_review` — work is complete and committed, but warrants a review pass before merging:
```json
{"status":"needs_review","summary":"Refactored auth module, all tests pass","accomplished":["refactored auth module","342 tests pass"],"remaining":[],"files_changed":["src/auth.rs"],"blockers":[],"reason":"Complex change — review recommended"}
```

Any non-JSON final output is treated as a failure and your work will not be recorded.
