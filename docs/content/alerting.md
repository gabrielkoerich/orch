+++
title = "Alerting"
description = "Multi-agent degradation detection and alert thresholds"
weight = 9
+++

Orch emits structured logs and KV-backed metrics that can be scraped by any log aggregation or monitoring pipeline to detect when the agent fleet is degraded.

## Multi-Agent Degradation Alert

### What it measures

Every sync tick (~45 s) the engine iterates all configured agents and checks whether each one is **degraded**. An agent is degraded when:

- It is in **agent-level cooldown** (e.g. repeated failures, credit exhaustion, silence detection), **or**
- All of its configured model pools are individually in **model-level cooldown** (every model across `simple`, `medium`, `complex`, and `review` tiers is cooled).

This metric counts cooldowns only. The router's own degraded set, shown by `orch cooldown list`, also holds an agent with `dispatch.degraded_rate_limit_threshold` (default 3) rate-limit rows within `dispatch.health_check_window_hours` (default 6). Failed runs and timeouts do not count toward that threshold. `orch cooldown clear` resets the window for the agents it clears.

### Metrics

| KV key | Type | Written | Description |
|--------|------|---------|-------------|
| `metrics:orch.agents_degraded.count` | gauge | every sync tick | Number of currently degraded agents (0 when healthy) |
| `metrics:orch.agents_degraded.alert` | flag | every sync tick | `"1"` when `count >= 3`, `"0"` otherwise |

Read metrics from the SQLite KV store:

```bash
sqlite3 ~/.orch/orch.db \
  "SELECT key, value FROM kv WHERE key LIKE 'metrics:orch.agents_degraded%';"
```

### Log signal

When `count >= 3` a `WARN`-level structured log is emitted with the following fields:

| Field | Example | Description |
|-------|---------|-------------|
| `degraded_count` | `3` | Total degraded agent count |
| `degraded_agents` | `["claude", "codex", "opencode"]` | Names of degraded agents |
| `cooled_models` | `claude:[opus,sonnet]; codex:[o3]` | Per-agent list of individually cooled models |
| `cooldown_reasons` | `claude=silence_agent_cooldown; codex=agent_error` | Per-agent cooldown reason string |

Example log line (JSON mode):

```json
{
  "level": "WARN",
  "message": "multi-agent degradation detected",
  "degraded_count": 3,
  "degraded_agents": ["claude", "codex", "opencode"],
  "cooled_models": "claude:[opus,sonnet]",
  "cooldown_reasons": "claude=silence_agent_cooldown; codex=agent_error; opencode=credit_exhaustion_out_of_credits"
}
```

### Suggested alert thresholds

| Threshold | Severity | Suggested action |
|-----------|----------|-----------------|
| `count >= 1` | Info | No action required; one agent recovering is normal |
| `count >= 2` | Warning | Monitor; tasks will still dispatch but at reduced throughput |
| `count >= 3` | **Page / Alert** | Orch fires the built-in WARN + alert metric. Investigate cooldown reasons. Consider restarting the service or topping up credits. |
| `count == total` | Critical | All agents degraded; no tasks can be dispatched. Immediate action required. |

### Cooldown reasons reference

| Reason string | Cause | Typical duration |
|---------------|-------|-----------------|
| `agent_error` | Repeated agent failures | 5 min → 15 min → 45 min → 2 h → 4 h (cap) |
| `model_error` | Specific model failure | 5 min → 15 min → 45 min → 2 h → 4 h (cap) |
| `transient_model_error` | One-off model failure: empty or cut-off output, an unparseable review, an opaque provider error | 5 min → 15 min (cap) |
| `persistent_model_error` | Model not found or unsupported, billing cycle exhausted on a known model | 4 h, escalating to 7 d (cap) |
| `rate_limit_usage_window` | Rate limit that names a usage window, such as a 5-hour limit | The window length |
| `vendor_reset_at` | Rate limit with a "try again at" time | Until that time |
| `silence_agent_cooldown` | Agent produced no output | 2 min |
| `silence_detected` | Model went silent (model-level) | `engine.silence_cooldown` (1 h), 4 h after 5 silences within 24 h |
| `credit_exhaustion_out_of_credits` | Per-model credit exhaustion | 1 h → 3 h → 8 h (cap) |
| `credit_exhaustion_org_level_disabled` | Org billing disabled | 2 h → 6 h → 8 h (cap) |
| `billing_cycle_exhausted` | Monthly billing cycle quota | 24 h → 3 d → 7 d (cap) |
| `persisted` | Cooldown loaded from previous run | Remaining original duration |

### Pagerduty / Alertmanager example

For systems that ingest orch's log stream (e.g. via `journald`, `Vector`, or `Grafana Loki`):

```yaml
# Grafana Loki alert rule (example)
- alert: OrchMultiAgentDegradation
  expr: |
    count_over_time(
      {job="orch"} |= "multi-agent degradation detected" [5m]
    ) > 0
  for: 1m
  labels:
    severity: critical
  annotations:
    summary: "3+ orch agents are simultaneously degraded"
    description: "Check orch logs for cooldown_reasons field to identify the root cause."
```

Or poll the KV metric directly:

```bash
# Returns "1" when alert is active
sqlite3 ~/.orch/orch.db \
  "SELECT value FROM kv WHERE key = 'metrics:orch.agents_degraded.alert';"
```
