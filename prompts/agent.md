Task:
ID: {{TASK_ID}}
Title: {{TASK_TITLE}}
Labels: {{TASK_LABELS}}
Branch: {{BRANCH_NAME}}
Worktree: {{WORKTREE_DIR}}
GitHub Issue: {{GH_ISSUE_REF}}
Body:
{{TASK_BODY}}

Agent profile (JSON):
{{AGENT_PROFILE_JSON}}

{{#if TASK_HISTORY}}
Previous attempts on this task:
{{TASK_HISTORY}}
{{/if}}

{{#if TASK_LAST_ERROR}}
Last error: {{TASK_LAST_ERROR}}
{{/if}}

{{#if TASK_HISTORY}}
IMPORTANT: If you see repeated failures above, try a DIFFERENT approach. Do not repeat what already failed.
{{/if}}

{{#if ISSUE_COMMENTS}}
GitHub issue comments (most recent):
{{ISSUE_COMMENTS}}
{{/if}}

{{#if TASK_CONTEXT}}
Context from prior runs:
{{TASK_CONTEXT}}
{{/if}}

{{#if PARENT_CONTEXT}}
Parent context:
{{PARENT_CONTEXT}}
{{/if}}

Project instructions:
{{PROJECT_INSTRUCTIONS}}

{{SKILLS_DOCS}}

Repository structure:
{{REPO_TREE}}

{{#if GIT_DIFF}}
Git diff (prior progress from previous attempts):
{{GIT_DIFF}}
{{/if}}
