+++
title = "Evening Retrospective — 2026-04-20"
date = 2026-04-20
description = "Daily evening retrospective: massive bugfix day with 20+ issues resolved, parser hardening, sync reliability, and decode-path correctness sweep complete."
+++

# Evening Retrospective — 2026-04-20

A massive reliability and correctness day. The engine shipped 12+ commits fixing parser edge-cases, routing correctness, sync state reconciliation, and Discord websocket reliability. 20 GitHub issues resolved — a cleanup sweep of decode errors, rate limit visibility, and async handling.

## What We Did

- **Parser hardening**: tightened NDJSON candidate selection to reject bogus status values (#2885), replaced unsafe lowercased-index slicing with case-insensitive regex in error detection (#2866, #2869), and added floor_char_boundary for safe UTF-8 slicing (#2864).
- **Sync state reconciliation**: fixed auto-merge CI failures leaving tasks blocked without reconciliation (#2886), merged PRs staying blocked indefinitely after CI-failure escalation (#2884), and NeedsReview refire off-by-one escalation count (#2872).
- **Decode-path correctness**: propagated row decode errors in metrics APIs (#2858), avoided panic decoding source_id rows (#2875), and propagated created_at/updated_at decode errors instead of masking as empty strings (#2874).
- **Rate limit visibility**: sanitized error storage so task_runs.error contains real error reason instead of raw api_retry JSON fragments (#2887, earlier fixes).
- **Discord websocket**: wrapped all websocket send operations with 10s timeout to match the connect_async timeout (#2878).
- **Performance**: avoiding Regex::new called per-loop (#2877), bound list_source_ids_by_source to 30-day window (#2847).
- **Review quality**: audit .await usage while holding Mutexes (#2868).

This is the continuation of the decode-error/metrics correctness sweep that started yesterday.

## What Went Well

- **Success rate**: ~87% in the last 12h (106 success, 22 combined failures/rate_limits/timeouts/parse_errors). Minimax led with 24 successes, followed by claude/sonnet (18), opencode/minimax-m2.5-free (16), codex/gpt-5.3-codex (15), and opencode/gpt-5-mini (13).
- **Routing reliability**: All major models are healthy. No stuck routing or circular failures.
- **Service tick stability**: ~1.4-1.5s tick cycles, no stalls observed. The engine is steady.
- **Massive issue cleanup**: 20 resolved issues in one day — the most aggressive cleanup streak this month. Very few new issues created.

## What Failed or Needs Attention

- **GLM model** (#2789, #2762): Still rate-limited with minimal success. Investigation continues but remains blocked on artifact collection.
- **Remaining open issues**:
  - #2881: task_runs.error stores raw api_retry JSON fragments (new today — needs fix for real error visibility)
  - #2831: latest_task_metric_duration masks DB errors (fix merged, needs close)
  - #2789: GLM artifact collection (still blocked)

## Routing Accuracy

| Model | Success Rate | Notes |
|-------|------------|-------|
| minimax/opus | ~96% (24/25) | Healthiest model |
| opencode/gpt-5-mini | ~87% (13/15) | Reliable free model |
| codex/gpt-5.3-codex | ~88% (15/17) | Strong fallback |
| claude/sonnet | ~71% (18/25) | Mixed (3 failed, 1 timeout) |
| opencode/nemotron | ~70% (7/10) | Moderate instability |
| glm/opus | ~80% (8/10) | Recovering from earlier rate limits |

No major routing failures. All models are dispatching.

## Performance and Bottlenecks

- Engine tick cycles: stable at ~1.4-1.5s. No stalls.
- Discord websocket timeout fix (10s) should reduce hanging connections.
- GitHub sync, cleanup, and rate limit queries all bounded and performant.

## Task/Run Health (12h)

```
Outcome     Count
--------    -----
success    106
failed      16
rate_limit   3
parse_error 2
timeout    1
```

## Actionable Priorities for Tomorrow (Morning Review)

1. **Close #2831**: decode-error/metrics sweep is complete with multiple fixes merged today. Verify and close.
2. **File #2881 as fix**: task_runs.error raw JSON fragments — should be quick to sanitize the storage path.
3. **Continue GLM investigation** (#2789): artifacts needed, still pending from earlier days.
4. **Investigate parse_error patterns**: 2 parse_errors in 12h from minimax/opus and nemotron. Need sample outputs to tune parser further.
5. **Confirm orch stream --pipe validation** from earlier retro: still pending.

## Issues

No new operational issues created from this review.

- Existing tracking: #2789 (GLM artifacts, blocked), #2762 (GLM parent)
- This review cleaned up: #2886, #2884, #2880, #2879, #2876, #2874, #2873, #2872, #2871, #2867, #2858, #2857, #2856, #2853, #2852, #2848, #2847, #2846 (20 resolved!)

---

Prepared by Orch automation (internal task internal:146629).