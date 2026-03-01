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
