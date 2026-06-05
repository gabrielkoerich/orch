You are writing a single git commit message for the change described below.

Follow the Conventional Commits 1.0.0 spec:

```
<type>(optional-scope): <subject>

[optional body]
```

Rules:
- `type` is one of: feat, fix, docs, style, refactor, perf, test, build, ci, chore, revert.
- `subject` is imperative, lowercase, no trailing period, ≤72 characters.
- Pick a single `scope` (a short noun like `cli`, `router`, `runner`, `docs`) only when one is obvious from the file paths; otherwise omit it.
- Add a short body (wrapped at ~72 cols) only when the change is non-trivial. Skip the body for one-line edits.
- If the diff is purely additive new code → `feat`. Bug fix → `fix`. Doc-only → `docs`. Test-only → `test`. Refactor with no behavior change → `refactor`.
- Never invent issue numbers, co-authors, or trailers. Never include backticks, code fences, or quotes around the message.

## Files in this commit

{{FILES}}

## Unified diff

```diff
{{DIFF}}
```

{{TRUNCATED_NOTE}}

## Output

Respond with ONLY the commit message — header line, optional blank line, optional body. No preamble, no explanation, no trailing commentary.
