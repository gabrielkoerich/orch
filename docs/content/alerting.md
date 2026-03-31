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

### Metrics

| KV key | Type | Written | Description |
|--------|------|---------|-------------|
| `metrics:orch.agents_degraded.count` | gauge | every tick | Number of currently degraded agents (0 when healthy) |
| `metrics:orch.agents_degraded.alert` | flag | every tick | `"1"` when `count >= 3`, `"0"` otherwise |

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
| `agent_error` | Repeated agent failures | 30 min |
| `model_error` | Specific model failure | 1 h |
| `silence_agent_cooldown` | Agent produced no output | 2 min |
| `silence_detected` | Model silently exited (model-level) | 30 min–4 h |
| `credit_exhaustion_out_of_credits` | Per-model credit exhaustion | 6 h |
| `credit_exhaustion_org_level_disabled` | Org billing disabled | 12 h |
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

---

## Weight-Threshold Skip Alert (weighted_round_robin mode)

When `router.weighted_round_robin` is enabled, the router also applies a configurable weight-threshold guard. Agents whose routing weight has decayed below `router.skip_limited_threshold` (default `0.3`) are skipped before any cooldown check is performed. This is a **pre-emptive** signal: the agent has not yet entered formal cooldown but is performing poorly.

### What it measures

Each call to `agent_is_routable()` that rejects an agent due to weight-decay increments an in-memory counter (`weight_skipped_total`). When 3 or more agents are simultaneously excluded from the candidate pool (due to weight decay **or** formal degradation), a `WARN`-level log is emitted:

| Field | Example | Description |
|-------|---------|-------------|
| `skipped_count` | `3` | Number of agents excluded from this routing decision |
| `skipped_agents` | `["claude", "codex", "kimi"]` | Names of excluded agents |
| `weight_skipped_total` | `42` | Cumulative weight-skip counter since last restart |
| `complexity` | `"medium"` | Complexity tier being routed |

Example log line:

```json
{
  "level": "WARN",
  "message": "3+ agents simultaneously skipped due to weight-decay or degradation",
  "skipped_count": 3,
  "skipped_agents": ["claude", "codex", "kimi"],
  "weight_skipped_total": 42,
  "complexity": "medium"
}
```

### Suggested alert thresholds

| Condition | Severity | Suggested action |
|-----------|----------|-----------------|
| `skipped_count >= 1` | Debug | Normal; one agent recovering from rate limits |
| `skipped_count >= 2` | Info | Monitor; reduced pool but still dispatching |
| `skipped_count >= 3` | **Warn / Alert** | Systemic rate-limiting; investigate upstream quotas |
| `weight_skipped_total` growing rapidly | Warning | Agents are hitting rate limits frequently; consider reducing task throughput |

### Grafana Loki example

```yaml
- alert: OrchWeightThresholdSkip
  expr: |
    count_over_time(
      {job="orch"} |= "3+ agents simultaneously skipped due to weight-decay or degradation" [5m]
    ) > 0
  for: 2m
  labels:
    severity: warning
  annotations:
    summary: "3+ orch agents skipped due to weight-decay"
    description: "Check skipped_agents and weight_skipped_total fields for details."
```

### Configuration

```yaml
router:
  weighted_round_robin: true   # must be enabled for this guard to activate
  skip_limited_threshold: 0.3  # agents below this weight are skipped
```

Set `skip_limited_threshold: 0.0` to disable the guard while keeping weighted routing active.
