# Research: Train and Use a Local Model for Orch

> Status: Research / Proposal
> Issue: #2194
> Date: 2026-04-08

---

## Summary

This document evaluates training and deploying a local open-source model to replace or supplement the cloud LLM calls orch currently makes for task routing, classification, and potentially code review. The goal is to reduce cost, latency, and external dependency while maintaining or improving routing quality.

**Verdict: Feasible and high-value for task routing. Orch already has enough data (1,601 tasks, 3,518 runs) to fine-tune a small model that matches or exceeds the current haiku-based router. Code review and full agent execution should remain on frontier models.**

---

## Where Orch Uses LLMs Today

| Use Case | Current Model | Calls/Task | Latency | Cost/Call |
|----------|--------------|------------|---------|-----------|
| Task routing | Claude Haiku (via pool) | 1 | 1-3s | ~$0.001 |
| Agent execution | Sonnet/Opus (by complexity) | 1-5 | 30-300s | $0.05-2.00 |
| Code review | Sonnet (round-robin) | 1-3 | 30-120s | $0.05-0.50 |
| Control chat | Sonnet (default) | per message | 2-10s | ~$0.01 |

**Primary candidate for local model: task routing.** It's a narrow classification task (select agent + complexity + skills from a fixed set), runs on every task, and a local model would eliminate the latency and cost entirely.

**Secondary candidate: control chat.** For simple status queries, a local model could handle the majority of requests without a cloud call.

**Not candidates: agent execution and code review.** These require frontier-level reasoning, long context, and tool use that local models cannot match.

---

## Available Training Data

Orch's SQLite database (`~/.orch/orch.db`) contains comprehensive telemetry from 1,601 completed tasks:

### Task Routing Data

| Source | Records | Fields Available |
|--------|---------|-----------------|
| `tasks` table | 1,601 | title, body, labels, agent, model, complexity, route_reason, selected_skills, outcome |
| `task_runs` (with prompts) | 2,339 | full prompt sent, agent response, outcome, duration, tokens |
| `task_activity` | thousands | status transitions, agent assignments, re-routes |
| `task_metrics` | per task | aggregated outcomes, duration, cost by agent/model |

### Distribution by Agent and Complexity

| Agent | Simple | Medium | Complex | Total |
|-------|--------|--------|---------|-------|
| claude | 98 | 634 | 110 | 842 |
| opencode | 85 | 392 | 78 | 555 |
| minimax | 4 | 55 | 3 | 62 |
| codex | 3 | 45 | 8 | 56 |
| kimi | 11 | 28 | 6 | 45 |

### Data Quality Assessment

**Strengths:**
- 1,594 out of 1,601 tasks completed successfully (99.6% success rate)
- Every task has a routing reason explaining the decision
- Full prompt and response pairs are stored in `task_runs`
- Outcome data allows filtering for only successful routing decisions
- Rich metadata: labels, complexity, selected skills, review cycles

**Weaknesses:**
- Imbalanced distribution: 72% medium, 19% complex, 12% simple (fixable with oversampling)
- Agent distribution skewed toward claude (53%) and opencode (35%)
- No explicit "wrong routing" labels — must infer from re-routes and failures
- Route reasons are free-text, not structured ground truth

### Is It Enough?

**Yes, for task routing.** 1,601 labeled examples with rich metadata exceeds the minimum viable threshold of 500-1,000 for a narrow classification task with QLoRA fine-tuning. The data is high quality (real production decisions with outcomes) and diverse (multiple repos, agents, complexity levels).

**Data augmentation options to strengthen the dataset:**
1. Use re-routed tasks as negative examples (task was routed to agent X but failed, then succeeded with agent Y)
2. Generate synthetic tasks using a frontier model, validated against the real distribution
3. Active learning: log low-confidence routing decisions and manually label them

---

## Model Selection

### Recommended: Qwen2.5-Coder-3B-Instruct

| Criteria | Qwen2.5-Coder-3B | Qwen2.5-Coder-7B | Phi-4-14B | DeepSeek-Coder-V2-16B |
|----------|-------------------|-------------------|-----------|----------------------|
| HumanEval | ~55% | 65.9% | ~82% | 73.5% |
| License | Apache 2.0 | Apache 2.0 | MIT | DeepSeek License |
| RAM (Q4_K_M) | ~1.8GB | ~4.0GB | ~8.0GB | ~9.0GB |
| Inference speed (M-series) | ~80-120 tok/s | ~40-60 tok/s | ~20-30 tok/s | ~15-25 tok/s |
| Routing latency | <100ms | ~200ms | ~400ms | ~500ms |
| Fine-tuning RAM (QLoRA) | ~4GB | ~8GB | ~14GB | ~16GB |
| Fine-tuning friendliness | Excellent | Excellent | Excellent | Good |

**For task routing, Qwen2.5-Coder-3B-Instruct is the sweet spot.** The routing task is classification (pick from a fixed set of agents, complexity levels, and skills), not open-ended code generation. A 3B model fine-tuned on orch's actual routing data will outperform a general-purpose 70B model prompted zero-shot, because:

1. The output schema is fixed and narrow (JSON with ~5 fields)
2. The input is short (task title + body + labels, typically <500 tokens)
3. The model only needs to learn orch's specific agent capabilities and routing heuristics
4. Fine-tuning on real outcomes means the model learns from what actually worked

**If 3B proves insufficient after evaluation, upgrade to 7B.** Both fit comfortably on any Apple Silicon Mac with 16GB+ RAM.

### Other Models Worth Considering

- **Phi-4 (14B)**: Stronger reasoning, MIT license, but 4x the RAM and latency. Only needed if routing requires deeper task analysis.
- **Llama 3.3 (8B)**: Good generalist, 131K native context, but not coding-specialized.
- **Granite Code 3B/8B (IBM)**: Apache 2.0, trained on permissively-licensed code only. Conservative IP choice.

---

## Fine-Tuning Approach

### Method: QLoRA (Quantized Low-Rank Adaptation)

QLoRA is the optimal approach for orch's use case:

- Updates only 0.1-1% of model parameters (rank 16-32 adapters)
- Base model stays quantized to 4-bit, adapters trained in bf16
- Achieves 95-98% of full fine-tuning quality for classification tasks
- Fits in 4-8GB RAM for 3B-7B models

### Training Data Format

Extract from orch.db and format as instruction-tuning JSONL:

```json
{"messages": [
  {"role": "system", "content": "You are a task router for orch, a software engineering orchestrator. Given a task description, select the best agent, complexity level, and skills. Available agents: claude, codex, opencode, kimi, minimax. Complexity levels: simple, medium, complex."},
  {"role": "user", "content": "Route this task:\nRepo: gabrielkoerich/orch\nTitle: Fix race condition in websocket handler\nLabels: bug, backend\nBody: The Discord gateway reconnect sometimes drops messages when two events arrive simultaneously..."},
  {"role": "assistant", "content": "{\"agent\": \"claude\", \"complexity\": \"complex\", \"reason\": \"Race condition debugging requires deep concurrency reasoning and careful state analysis\", \"profile\": {\"role\": \"backend specialist\", \"skills\": [\"git-worktree\", \"gh\"], \"tools\": [\"cargo\", \"rg\", \"git\"], \"constraints\": [\"Do not modify config files\"]}, \"selected_skills\": [\"gh-issue-worktree\"]}"}
]}
```

### Data Extraction Pipeline

```sql
-- Extract routing training data from orch.db
SELECT
    t.title,
    t.body,
    t.labels,
    t.repo,
    t.agent,
    t.complexity,
    t.route_reason,
    t.agent_profile,
    t.selected_skills,
    t.status,
    t.review_cycles,
    t.attempts
FROM tasks t
WHERE t.status = 'done'
  AND t.agent IS NOT NULL
  AND t.complexity IS NOT NULL
ORDER BY t.created_at;
```

### Training Configuration

```yaml
# QLoRA config for Qwen2.5-Coder-3B
model: Qwen/Qwen2.5-Coder-3B-Instruct
method: qlora
lora_rank: 16
lora_alpha: 32
lora_dropout: 0.05
target_modules: [q_proj, k_proj, v_proj, o_proj, gate_proj, up_proj, down_proj]
quantization: 4bit  # NF4
batch_size: 4
gradient_accumulation_steps: 4
learning_rate: 2e-4
epochs: 3
max_seq_length: 2048
warmup_ratio: 0.03
optimizer: adamw_8bit
```

### Training Tools

| Tool | Platform | Pros | Cons |
|------|----------|------|------|
| **MLX + mlx-lm** | macOS (Apple Silicon) | Native, fast, no CUDA needed | Smaller ecosystem, fewer tutorials |
| **Unsloth** | Any (GPU preferred) | 2-5x faster, memory efficient | GPU-focused, macOS via MLX adapter |
| **Axolotl** | Linux/GPU | Config-driven, many features | Requires NVIDIA GPU |
| **HF transformers + PEFT** | Any | Most flexible, largest community | Slower, more boilerplate |

**Recommendation: MLX for Apple Silicon development, Unsloth/Axolotl for cloud GPU training.**

For a quick experiment on a Mac:

```bash
# Install MLX fine-tuning tools
pip install mlx-lm

# Convert training data
python scripts/extract_training_data.py  # from orch.db → training.jsonl

# Fine-tune with LoRA
mlx_lm.lora \
    --model Qwen/Qwen2.5-Coder-3B-Instruct \
    --train \
    --data ./training_data \
    --adapter-path ./adapters \
    --lora-layers 16 \
    --batch-size 4 \
    --iters 1000 \
    --learning-rate 2e-4
```

### Hardware Requirements and Cost

| Setup | Training Time (1.6K examples) | Cost |
|-------|-------------------------------|------|
| M2/M3 Max 32GB (MLX) | 1-3 hours | $0 |
| M2/M3 Ultra 128GB (MLX) | 30-90 min | $0 |
| 1x A100 40GB (Lambda/RunPod) | 15-30 min | $2-4 |
| 1x A6000 48GB (RunPod) | 20-40 min | $1-2 |

The entire fine-tuning pipeline can run on a developer's Mac. No cloud GPU required.

---

## Inference and Deployment

### Recommended: Ollama HTTP API

Ollama wraps llama.cpp with a simple REST API and model management. It's already packaged as a macOS service.

```bash
# Install and start
brew install ollama
brew services start ollama

# Import fine-tuned model
ollama create orch-router -f Modelfile
```

Where `Modelfile` is:
```dockerfile
FROM ./orch-router-q4_k_m.gguf
PARAMETER temperature 0.1
PARAMETER num_predict 256
SYSTEM "You are a task router for orch..."
```

### Rust Integration

Orch already makes HTTP requests. Adding Ollama as a routing backend requires minimal changes:

```rust
// In src/engine/router/mod.rs — new routing strategy
async fn route_local(&self, task: &Task) -> Result<RouteResult> {
    let prompt = self.build_routing_prompt(task)?;
    let response = self.ollama_client
        .post("http://localhost:11434/api/generate")
        .json(&json!({
            "model": "orch-router",
            "prompt": prompt,
            "stream": false,
            "options": { "temperature": 0.1, "num_predict": 256 }
        }))
        .send()
        .await?;

    let body: OllamaResponse = response.json().await?;
    parse_route_result(&body.response)
}
```

**The existing `parse_route_result` logic stays unchanged** — the local model outputs the same JSON schema the LLM router already expects.

### Integration Architecture

```
                    ┌─────────────────┐
                    │   orch engine    │
                    └────────┬────────┘
                             │
                    ┌────────▼────────┐
                    │  router module   │
                    └────────┬────────┘
                             │
              ┌──────────────┼──────────────┐
              │              │              │
     ┌────────▼───┐  ┌──────▼─────┐  ┌────▼────────┐
     │ local model │  │  LLM pool  │  │ round-robin │
     │  (ollama)   │  │  (haiku)   │  │  (fallback) │
     └─────────────┘  └────────────┘  └─────────────┘
```

**Routing priority:**
1. Label-based override (unchanged)
2. Local model via Ollama (new, fast, free)
3. LLM pool fallback if Ollama is down
4. Round-robin fallback if all LLMs fail

### Performance Comparison

| Metric | Current (Haiku API) | Local (Qwen2.5-3B Q4) |
|--------|--------------------|-----------------------|
| Latency | 1-3 seconds | 50-150 milliseconds |
| Cost per route | ~$0.001 | $0 |
| Availability | Depends on API/credits | Always available |
| Monthly cost (100 tasks/day) | ~$3 | $0 |
| Cold start | N/A | ~2s (model load, then cached) |
| Quality (expected) | Baseline | Equal or better after fine-tuning |

---

## Python vs Rust

### Decision: Keep Rust, Use Python Only for Training

| Component | Language | Rationale |
|-----------|----------|-----------|
| Training data extraction | Python | pandas/SQLite for data wrangling, standard ML tooling |
| Model fine-tuning | Python | MLX/Unsloth/HF are all Python-native |
| Model conversion (GGUF) | Python | llama.cpp conversion scripts are Python |
| Inference runtime | Rust (via Ollama HTTP) | No new dependencies, zero-copy integration |
| Router integration | Rust | Existing codebase, type-safe RouteResult parsing |
| Evaluation harness | Python | sklearn metrics, confusion matrices, easy iteration |

**The training pipeline is inherently Python** — every ML framework (MLX, transformers, Unsloth) is Python-first. Fighting this by implementing training in Rust would be impractical and slow.

**The inference path stays in Rust** — Ollama's HTTP API is a clean boundary. Orch calls `localhost:11434`, gets JSON back, parses it with serde. No Python in the hot path.

**Do not add a Python sidecar for inference.** It adds deployment complexity (Python environment, venv management, startup ordering) for no benefit over Ollama's HTTP API. Ollama is a single binary, managed by `brew services`, and provides the same interface.

### Training Scripts Location

```
scripts/
  training/
    extract_data.py       # SQLite → JSONL training data
    train_router.py       # MLX/Unsloth fine-tuning
    evaluate.py           # accuracy, confusion matrix, A/B test
    convert_gguf.py       # merge LoRA + quantize to GGUF
    Modelfile             # Ollama model definition
```

These are developer tools, not part of the orch binary. They run manually when retraining.

---

## Training Pipeline

### Step 1: Extract Training Data

```python
import sqlite3, json

conn = sqlite3.connect("~/.orch/orch.db")
tasks = conn.execute("""
    SELECT title, body, labels, repo, agent, complexity,
           route_reason, agent_profile, selected_skills
    FROM tasks
    WHERE status = 'done' AND agent IS NOT NULL
""").fetchall()

# Format as instruction-tuning JSONL
training_data = []
for t in tasks:
    training_data.append({
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": format_task(t)},
            {"role": "assistant", "content": format_route_result(t)}
        ]
    })
```

### Step 2: Data Validation and Split

- Deduplicate by task title similarity
- Validate JSON output format
- Stratified split: 80% train, 10% validation, 10% test
- Ensure all agents and complexity levels appear in each split

### Step 3: Fine-Tune

```bash
# On Apple Silicon with MLX
mlx_lm.lora --model Qwen/Qwen2.5-Coder-3B-Instruct \
    --train --data ./data --adapter-path ./adapters \
    --lora-layers 16 --batch-size 4 --iters 1000

# Or on GPU with Unsloth
python train_router.py --base-model Qwen/Qwen2.5-Coder-3B-Instruct \
    --data ./data/train.jsonl --output ./output \
    --lora-rank 16 --epochs 3 --lr 2e-4
```

### Step 4: Evaluate

```python
# Compare local model vs haiku on held-out test set
metrics = {
    "agent_accuracy": 0.0,      # correct agent selection
    "complexity_accuracy": 0.0,  # correct complexity level
    "skill_f1": 0.0,            # F1 for skill selection (multi-label)
    "json_validity": 0.0,       # % of outputs that are valid JSON
    "latency_p50": 0.0,         # median inference time
    "latency_p99": 0.0,         # tail latency
}
```

**Acceptance criteria:**
- Agent accuracy >= 85% (vs haiku baseline)
- Complexity accuracy >= 80%
- JSON validity >= 99%
- Latency p99 < 500ms

### Step 5: Deploy

```bash
# Merge LoRA adapters into base model
python convert_gguf.py --base Qwen/Qwen2.5-Coder-3B-Instruct \
    --adapter ./adapters --output ./orch-router.gguf --quant q4_k_m

# Import into Ollama
ollama create orch-router -f Modelfile

# Test
curl -s localhost:11434/api/generate -d '{
    "model": "orch-router",
    "prompt": "Route this task:\nTitle: Fix typo in README\nLabels: docs\nBody: ...",
    "stream": false
}' | jq .response
```

### Step 6: A/B Test in Production

Add a `router.mode: "local"` option that uses the local model, with automatic fallback to the LLM pool:

```yaml
router:
  mode: "local"              # new: "local", "llm", "round_robin"
  local_model: "orch-router" # Ollama model name
  local_fallback: "llm"     # fallback if Ollama is unavailable
```

Run both routers on incoming tasks for 1-2 weeks, compare:
- Routing agreement rate (do they pick the same agent?)
- Task success rate by router
- Re-route frequency by router
- Cost savings

---

## Continuous Improvement Loop

Once deployed, the local model improves over time:

```
  ┌────────────┐     ┌───────────────┐     ┌──────────────┐
  │ New tasks   │────▶│ Local router   │────▶│ Agent runs   │
  └────────────┘     └───────────────┘     └──────┬───────┘
                                                   │
                                          ┌────────▼───────┐
                                          │ Outcome logged  │
                                          │ (success/fail)  │
                                          └────────┬───────┘
                                                   │
                            ┌──────────────────────▼──────┐
                            │ Periodic retrain (monthly)   │
                            │ on accumulated data           │
                            └──────────────────────────────┘
```

Each completed task becomes a new training example. Monthly retraining incorporates:
- New tasks and routing decisions
- Corrected labels from re-routes (task failed with agent X, succeeded with Y)
- New agents or skills added to the system
- Changes in agent capabilities (model upgrades)

---

## Future Extensions

### Phase 2: Local Review Triage

Train a second small model to triage PR reviews — classify whether a review is likely to approve, request changes, or need human escalation. This doesn't replace the full review agent but reduces unnecessary review agent invocations.

### Phase 3: Control Chat for Simple Queries

Fine-tune a local model on the 233 control chat messages to handle common queries ("what's running?", "show blocked tasks", "cost today") without a cloud LLM call. Complex queries still go to sonnet.

### Phase 4: Prompt Optimization

Use the stored prompt/response pairs in `task_runs` to optimize agent system prompts. A local model could learn which prompt patterns lead to faster task completion and fewer review cycles.

---

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Local model routes worse than haiku | Medium | Low | Fallback to LLM pool; A/B test before full switch |
| Training data too homogeneous (one repo) | Medium | Medium | Add synthetic diversity; support multi-project data |
| Ollama goes down silently | Low | Low | Health check (like webhook health check); auto-fallback |
| Model drift after agent updates | Medium | Low | Monthly retraining; monitor routing accuracy |
| Apple Silicon RAM pressure | Low | Low | 3B Q4 uses <2GB; negligible vs browser/IDE |

---

## Recommendations

1. **Start with task routing** — highest ROI, lowest risk, most data available
2. **Use Qwen2.5-Coder-3B-Instruct** — best coding model per parameter, Apache 2.0, fits anywhere
3. **Fine-tune with QLoRA on MLX** — zero cost, runs on any Mac, 1-3 hours
4. **Deploy via Ollama** — one binary, `brew services`, HTTP API, no new Rust deps
5. **Keep Rust for inference, Python for training** — right tool for each job
6. **A/B test before switching** — run both routers in parallel, measure outcomes
7. **Retrain monthly** — each task is a new training example, continuous improvement
8. **Do not attempt local agent execution** — frontier models are needed for actual coding tasks; the local model handles the fast, cheap routing layer only
