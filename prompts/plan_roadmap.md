Turn the user's plan into an orch roadmap.

Output only the roadmap markdown. No preamble, no commentary, no code fences.

## Format

A milestone is a level-2 heading. The first token is the id, then a period, then the title:

```
## M1. Workspace
```

A task is a level-3 heading under that milestone. The first token is the id, then the title:

```
### M1-01 Create the crate
```

Each task uses these list fields. Goal and Acceptance are required. Scope is optional. Depends on is required, even when the value is `none`.

```
- Goal: one sentence.
- Scope: what to touch.
- Acceptance:
  - a check that proves the work is done
- Depends on: none
```

Add `- Owner: human` only when a person must do the work (accounts, legal, store setup, recordings). Omit Owner otherwise.

`Depends on` is `none` or a comma-separated list of task ids that already appear in this roadmap (`M1-01, M1-02`). Never invent an id that is not a heading.

Use headings for every milestone. Do not use a table.

## Ids

Milestone ids look like `M1`, `M2`, `M2S`. Task ids look like `M1-01`, `M1-02`, `M2S-01`. Number tasks in order inside each milestone.

If the Existing roadmap section below is not empty, keep every task id whose work still exists so a later import updates the same issues. Give new work a new id. Drop a task that is no longer in the plan.

If Existing roadmap is empty, start at `M1-01`.

If the plan already uses this id shape, keep those ids.

Split the work so one agent can finish a task in one session. Put human-only tasks in their own milestone at the end.

## Existing roadmap

{{EXISTING}}

## Plan

{{PLAN}}
