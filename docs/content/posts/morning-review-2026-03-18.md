+++
title = "Morning Review — 2026-03-18"
date = 2026-03-18T12:30:00Z
[extra]
job = "morning-review"
+++

## Summary

Clean start: both 5th-carry-over items from the retro landed this morning before
the review ran. Zero open issues. One minor noise fix applied directly.

---

## Recent Commits (last 24h)

| Commit | Description |
|--------|-------------|
| `c45d599` | test: regression test for dispatch_key race in sync_tick (#690) — closes #686 |
| `1db3b63` | fix: check closed issues before filing new ones in agent system prompt (#689) — closes #687 |
| `fad13b3` | docs: evening retrospective 2026-03-17 (#688) |
| `40a624c` | fix: store_reset_counters wipes review_cycles on RequestChanges (#685) |
| `64e70a5` | feat: AGE + TRIES columns in `orch task list` (#683) |

---

## Carry-over Status

| Priority from Retro | Status |
|---------------------|--------|
| Bean internal:3419 outcome | ✅ PR 63 merged, worktree cleaned up at 20:53 |
| dispatch_key regression test (#686) | ✅ Landed `c45d599` — 5th carry-over resolved |
| code-development prompt gap (#687) | ✅ Fixed `1db3b63` — 3rd carry-over resolved |
| "profile missing skills" warning | ✅ Fixed this session (see below) |

All four retro priorities resolved. No open issues.

---

## Service Health

- Service running cleanly (v0.14.74)
- 4 tasks dispatched at 12:01–12:03: internal:3444–3447 (code-review, code-development, morning-review, morning-briefing)
- Both projects connected: `gabrielkoerich/orch` and `gabrielkoerich/bean`
- Bean first-task milestone confirmed: internal:3419 completed, PR 63 auto-merged

---

## Fix Applied: "profile missing skills" warning suppressed

**Root cause**: `check_routing_sanity()` in `src/engine/router/llm.rs` warned on
every task where `profile.skills.is_empty()`. For analysis tasks (retrospectives,
morning reviews, code reviews), the LLM correctly returns no skills since no
catalog skill applies. The warning fired regardless — producing constant false-
positive noise in the log.

**Fix**: Removed the `profile.skills.is_empty()` guard from `check_routing_sanity`
and deleted the corresponding `sanity_warns_empty_skills_in_profile` test. The
other sanity checks (backend-to-claude, docs-to-codex) remain intact.

**Result**: 692 tests pass, clippy clean.

---

## Log Analysis

**`orch.error.log`** (brew stderr): Still emitting repeated "no valid projects
configured — all backends failed health checks" messages. This pattern has been
noted in every morning review since 03-13. The message does NOT appear in the
current binary or source — it's likely emitted by agent CLI invocations (`orch`
commands run from worktrees where no `.orch.yml` is present). The service itself
is unaffected. This is a recurring annoyance but not a correctness issue; no
task filed (it's been tagged benign repeatedly and the root cause is understood).

---

## Task Queue

No tasks filed — all carry-overs resolved, no new patterns found, one fix
applied directly.

---

## Tomorrow's Priority

1. **Monitor internal:3444 (code-review) and internal:3445 (code-development)**
   — both dispatched this morning; verify they complete and produce quality output.
2. **Continue bean project monitoring** — second internal task dispatched today
   (internal:3447 morning-briefing). Verify outcome.
3. **"no valid projects" stderr noise** — if still appearing in next retro,
   consider filing a proper investigation task (root cause: orch CLI in worktrees
   without `.orch.yml`; fix: suppress error when running in global config mode
   with no local `.orch.yml`).
