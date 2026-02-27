# Orch — Feature Specs

Missing features identified in the bash `orchestrator` → Rust `orch` parity audit.

---

## 1. Review Agent + Auto-Merge

**Priority:** Critical
**Status:** Not implemented
**Files:** `src/engine/runner/mod.rs`, `src/engine/mod.rs`, `src/backends/github.rs`

### Problem

When an agent completes a task and creates a PR, there's no quality gate. The PR sits in `status:in_review` until a human reviews it. We need:

1. An automated review agent that checks the PR after the task agent finishes
2. If the review passes → auto-merge the PR
3. If the review requests changes → send the agent back to fix them

### Design

#### Flow

```
Agent completes task
  → status:done, PR created
  → Engine detects completion
  → Spawns review agent on same worktree
  → Review agent checks: tests pass? code quality? requirements met?
  → Returns: approve | request_changes

If approve:
  → gh pr merge --squash
  → status:done (final)
  → Close issue (if auto_close=true)
  → Cleanup worktree + branch

If request_changes:
  → Post review notes as comment on PR
  → Re-dispatch original task agent with review feedback
  → Agent fixes issues, pushes to same branch
  → Loop back to review (max 2 review cycles)
```

#### Review Agent Invocation

The review agent runs as a separate agent invocation in the **same worktree** as the task. It doesn't get a new worktree — it reviews what's already there.

```rust
// In engine/mod.rs, after task completes with status:done
async fn review_and_merge(
    &self,
    task: &ExternalTask,
    backend: &dyn ExternalBackend,
    tmux: &TmuxManager,
) -> anyhow::Result<ReviewDecision> {
    // 1. Check config
    let enabled = config::get("workflow.enable_review_agent")
        .map(|v| v == "true")
        .unwrap_or(false);
    if !enabled {
        return Ok(ReviewDecision::Skip);
    }

    // 2. Load sidecar for worktree path, branch, agent
    let sidecar = sidecar::read(&self.repo, &task.id.0)?;
    let worktree = sidecar.get("worktree");
    let branch = sidecar.get("branch");

    // 3. Build review prompt
    let review_prompt = build_review_prompt(task, &sidecar);

    // 4. Pick review agent (config: workflow.review_agent, default: claude)
    let review_agent = config::get("workflow.review_agent")
        .unwrap_or_else(|_| "claude".to_string());
    let review_model = config::get_model_for_complexity("review", &review_agent);

    // 5. Spawn review agent in tmux (same worktree)
    let inv = AgentInvocation {
        agent: review_agent,
        model: Some(review_model),
        work_dir: PathBuf::from(worktree),
        system_prompt: review_system_prompt(),
        agent_message: review_prompt,
        task_id: format!("{}-review", task.id.0),
        // ... same branch, same repo
    };
    let session = spawn_in_tmux(tmux, &inv).await?;

    // 6. Wait for completion, parse response
    // 7. Return ReviewDecision
}
```

#### Review Prompt

The review agent gets:
- The task title and body (requirements)
- `git diff main` (all changes)
- `git log main..HEAD` (commit history)
- Test results (if available — run tests before review)
- The original agent's summary

```markdown
## Review Task #{id}: {title}

You are reviewing a PR created by an AI agent. Check:

1. **Requirements met** — does the code satisfy the task description?
2. **Tests pass** — run the test suite, report failures
3. **Code quality** — no obvious bugs, security issues, or regressions
4. **Completeness** — all files committed, no TODOs left behind

### Task Description
{task.body}

### Agent Summary
{agent_summary}

### Changes
```diff
{git_diff}
```

### Commits
{git_log}

## Output Format

```json
{
  "decision": "approve|request_changes",
  "notes": "Detailed review feedback",
  "test_results": "pass|fail|skipped",
  "issues": [
    {
      "file": "src/foo.rs",
      "line": 42,
      "severity": "error|warning",
      "description": "What's wrong and how to fix it"
    }
  ]
}
```
```

#### Review Response

```rust
#[derive(Debug, Deserialize)]
pub struct ReviewResponse {
    pub decision: String,           // "approve" | "request_changes"
    pub notes: String,
    pub test_results: Option<String>,
    pub issues: Vec<ReviewIssue>,
}

#[derive(Debug, Deserialize)]
pub struct ReviewIssue {
    pub file: String,
    pub line: Option<u32>,
    pub severity: String,           // "error" | "warning"
    pub description: String,
}

pub enum ReviewDecision {
    Approve,
    RequestChanges { notes: String, issues: Vec<ReviewIssue> },
    Skip,                           // Review agent disabled
    Failed(String),                 // Review agent crashed
}
```

#### Auto-Merge

When review approves:

```rust
async fn auto_merge(
    &self,
    task: &ExternalTask,
    branch: &str,
    backend: &dyn ExternalBackend,
) -> anyhow::Result<()> {
    // 1. Get PR number from branch
    let pr_number = gh_cli::get_pr_number(&self.repo, branch).await?;

    // 2. Merge via gh CLI
    let output = Command::new("gh")
        .args(["pr", "merge", &pr_number.to_string(),
               "--squash", "--delete-branch",
               "--repo", &self.repo])
        .output().await?;

    if !output.status.success() {
        // Merge failed (conflicts, branch protection, etc.)
        // Mark needs_review so human can intervene
        backend.update_status(&task.id, Status::NeedsReview).await?;
        backend.post_comment(&task.id,
            &format!("Auto-merge failed: {}", String::from_utf8_lossy(&output.stderr))
        ).await?;
        return Ok(());
    }

    // 3. Update status
    backend.update_status(&task.id, Status::Done).await?;

    // 4. Close issue if auto_close enabled
    if config::get("workflow.auto_close").map(|v| v == "true").unwrap_or(true) {
        // gh api to close issue
    }

    // 5. Cleanup worktree
    cleanup_worktree(&self.repo, &task.id.0).await?;

    // 6. Post final comment
    backend.post_comment(&task.id, "PR merged and branch cleaned up.").await?;

    Ok(())
}
```

#### Request Changes → Re-dispatch

When review requests changes, re-dispatch the original agent:

```rust
async fn handle_review_changes(
    &self,
    task: &ExternalTask,
    review: &ReviewResponse,
    backend: &dyn ExternalBackend,
) -> anyhow::Result<()> {
    // 1. Check review cycle count (max 2 review rounds)
    let sidecar = sidecar::read(&self.repo, &task.id.0)?;
    let review_cycles: u32 = sidecar.get("review_cycles")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    if review_cycles >= 2 {
        // Too many review cycles — escalate to human
        backend.update_status(&task.id, Status::NeedsReview).await?;
        backend.post_comment(&task.id, &format!(
            "Review agent requested changes after {} cycles. Escalating to human.\n\n{}",
            review_cycles, review.notes
        )).await?;
        return Ok(());
    }

    // 2. Post review feedback as comment
    let comment = format_review_comment(review);
    backend.post_comment(&task.id, &comment).await?;

    // 3. Update sidecar
    sidecar::merge(&self.repo, &task.id.0, &serde_json::json!({
        "review_cycles": (review_cycles + 1).to_string(),
        "review_notes": review.notes,
        "status": "in_progress",
    }))?;

    // 4. Re-dispatch — set status back to routed (keeps same agent/branch/worktree)
    backend.update_status(&task.id, Status::Routed).await?;
    // Engine will pick it up on next tick, agent sees review feedback in context
}
```

#### Configuration

```yaml
workflow:
  enable_review_agent: true         # default: false
  review_agent: "claude"            # which agent reviews (default: claude)
  auto_merge: true                  # merge on approve (default: true)
  max_review_cycles: 2              # max review rounds before escalating (default: 2)
  auto_close: true                  # close issue after merge (default: true)
```

#### Task Lifecycle with Review

```
new → routed → in_progress → done (agent finishes)
  → in_review (review agent spawned)
  → done + merged (review approved, auto-merge)
  OR
  → routed (review requested changes, re-dispatch)
  → in_progress → done → in_review → ... (loop, max 2 cycles)
  OR
  → needs_review (max cycles exceeded, human escalation)
```

#### Implementation Steps

1. Add `ReviewResponse`, `ReviewIssue`, `ReviewDecision` types to `src/engine/runner/response.rs`
2. Add `build_review_prompt()` and `review_system_prompt()` to `src/engine/runner/agent.rs`
3. Add `review_and_merge()` to `src/engine/mod.rs` — called after Phase 3b dispatch completes with `status:done`
4. Add `auto_merge()` using `gh pr merge --squash --delete-branch`
5. Add `handle_review_changes()` to re-dispatch with review context
6. Add `review_cycles` tracking in sidecar
7. Add config keys: `workflow.enable_review_agent`, `workflow.review_agent`, `workflow.auto_merge`, `workflow.max_review_cycles`
8. Add review context to `build_agent_message()` — on re-dispatch, agent sees review notes
9. Tests: review response parsing, cycle counting, auto-merge flow

---

## 2. PR Review Comments → Fix Dispatch

**Priority:** Critical
**Status:** Partially implemented (parsing exists, dispatch missing)
**Files:** `src/engine/mod.rs` (`review_open_prs()`), `src/engine/runner/`

### Problem

When a human reviewer requests changes on a PR via GitHub's review UI, the orchestrator should automatically dispatch the original agent to fix the issues. Currently `review_open_prs()` creates internal follow-up tasks, but doesn't re-dispatch to the same worktree/branch.

### Current State

`review_open_prs()` in the sync tick:
1. Lists tasks with `status:in_review`
2. Fetches PR reviews via GitHub API
3. Filters for `CHANGES_REQUESTED` reviews
4. Creates internal follow-up tasks with `pr-review-followup` label

### What's Missing

The follow-up tasks are internal SQLite tasks — they don't have the original worktree, branch, or PR context. The agent starts fresh instead of fixing the existing PR.

### Design

Instead of creating a new internal task, re-dispatch the **original external task** with review context:

```rust
async fn handle_pr_review_changes(
    &self,
    task: &ExternalTask,
    review_comments: Vec<GitHubReviewComment>,
    backend: &dyn ExternalBackend,
) -> anyhow::Result<()> {
    // 1. Build review context from comments
    let review_context = review_comments.iter()
        .map(|c| format!("**{}** on `{}` (line {}):\n> {}",
            c.user.login, c.path, c.position.unwrap_or(0), c.body))
        .collect::<Vec<_>>()
        .join("\n\n");

    // 2. Store review context in sidecar (agent will see it in build_agent_message)
    sidecar::merge(&self.repo, &task.id.0, &serde_json::json!({
        "pr_review_context": review_context,
        "status": "routed",
    }))?;

    // 3. Set task back to routed (same agent, same worktree, same branch)
    backend.update_status(&task.id, Status::Routed).await?;

    // 4. Post comment acknowledging the review
    backend.post_comment(&task.id, &format!(
        "Received {} review comment(s). Re-dispatching agent to address feedback.",
        review_comments.len()
    )).await?;
}
```

The agent sees the review context in `build_agent_message()`:

```rust
// In context.rs or agent.rs
if !context.pr_review_context.is_empty() {
    msg.push_str("## PR Review Feedback\n\n");
    msg.push_str("A reviewer has requested changes. Address each comment:\n\n");
    msg.push_str(&context.pr_review_context);
    msg.push('\n');
}
```

### Implementation Steps

1. Add `pr_review_context` field to `TaskContext` struct in `context.rs`
2. Load it from sidecar in `build_task_context()`
3. Include in `build_agent_message()` when non-empty
4. Change `review_open_prs()` to call `handle_pr_review_changes()` instead of creating internal tasks
5. Remove internal task creation for `pr-review-followup` (or keep as fallback)
6. Dedup: track `last_review_handled_at` in sidecar to avoid re-dispatching for same review

---

## 3. Task Delegation

**Priority:** High
**Status:** DB supports parent/child, no agent-side delegation
**Files:** `src/engine/runner/mod.rs`, `src/parser.rs`, `src/engine/mod.rs`

### Problem

When an agent encounters a complex task, it should be able to delegate subtasks. Currently the `AgentResponse` JSON schema doesn't include a `delegations` field, and the runner doesn't process subtask creation.

### Design

#### Extended Agent Response

Add `delegations` to the JSON output format:

```json
{
  "status": "blocked",
  "summary": "Task requires 3 subtasks",
  "accomplished": ["Analyzed requirements"],
  "remaining": ["Waiting on subtasks"],
  "files_changed": [],
  "blockers": ["Waiting on delegated subtasks"],
  "reason": "Decomposed into subtasks",
  "delegations": [
    {
      "title": "Implement user login API",
      "body": "Create POST /api/login endpoint with JWT auth...",
      "labels": ["backend", "auth"]
    },
    {
      "title": "Add login form UI",
      "body": "Create React login component with email/password fields...",
      "labels": ["frontend", "auth"]
    }
  ]
}
```

#### Parser Changes

```rust
// In parser.rs — add to AgentResponse
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResponse {
    // ... existing fields ...

    /// Subtask delegations (agent requests child tasks).
    #[serde(default)]
    pub delegations: Vec<Delegation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delegation {
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub labels: Vec<String>,
}
```

#### Runner Processing

After parsing a successful response with delegations:

```rust
// In engine/runner/mod.rs or engine/mod.rs
async fn process_delegations(
    &self,
    parent_task: &ExternalTask,
    delegations: &[Delegation],
    backend: &dyn ExternalBackend,
) -> anyhow::Result<()> {
    if delegations.is_empty() {
        return Ok(());
    }

    for delegation in delegations {
        // 1. Create child issue on GitHub
        let mut labels = delegation.labels.clone();
        labels.push("status:new".to_string());
        labels.push(format!("parent:{}", parent_task.id.0));

        let child_body = format!(
            "{}\n\n---\n_Delegated from #{}_",
            delegation.body, parent_task.id.0
        );

        let child_id = backend.create_task(
            &delegation.title,
            &child_body,
            &labels,
        ).await?;

        tracing::info!(
            parent = parent_task.id.0,
            child = child_id.0,
            title = delegation.title,
            "created delegated subtask"
        );
    }

    // 2. Mark parent as blocked
    backend.update_status(&parent_task.id, Status::Blocked).await?;

    // 3. Post comment on parent
    let summary = delegations.iter()
        .enumerate()
        .map(|(i, d)| format!("{}. {}", i + 1, d.title))
        .collect::<Vec<_>>()
        .join("\n");

    backend.post_comment(&parent_task.id, &format!(
        "Delegated {} subtask(s):\n\n{}\n\nParent task is blocked until all subtasks complete.",
        delegations.len(), summary
    )).await?;

    Ok(())
}
```

#### Engine Integration

Phase 4 (unblock parents) already exists and calls `get_sub_issues()`. When all children are `status:done`, parent transitions back to `status:new` and gets re-dispatched. The agent sees previous context and can continue where it left off.

#### System Prompt Addition

Add to the agent system prompt:

```markdown
## Delegation

If a task is too complex, you can delegate subtasks. Include a `delegations` array in your response:

```json
{
  "status": "blocked",
  "delegations": [
    {"title": "Subtask title", "body": "Detailed description", "labels": ["label1"]}
  ]
}
```

Rules:
- Set status to `blocked` when delegating
- Each delegation becomes a separate GitHub issue
- You will be re-run after all subtasks complete
- Only delegate when the task genuinely requires parallel workstreams
- Don't delegate trivial work — just do it
```

#### Implementation Steps

1. Add `Delegation` struct and `delegations` field to `AgentResponse` in `parser.rs`
2. Add `process_delegations()` to `src/engine/mod.rs`
3. Call it after successful task completion in Phase 3b dispatch
4. Add delegation instructions to system prompt in `agent.rs`
5. Tests: delegation parsing, child task creation, parent blocking

---

## 4. Skills Sync

**Priority:** Medium
**Status:** Missing
**Issue:** To be created

### Problem

The bash orchestrator had `skills_sync.sh` which clones/pulls skill repositories listed in config. Skills are markdown docs that agents reference for specialized workflows. The Rust context builder already loads skill docs from disk — but there's no auto-sync from git repos.

### Design

```yaml
# In .orch.yml
skills:
  - repo: "owner/skill-repo"
    path: "skills/"              # subdirectory within repo
  - repo: "owner/another-repo"
    path: "docs/skills/"
```

On each sync tick (120s), pull latest for each skill repo. Store in `~/.orch/skills/{repo}/`.

### Implementation

- Add `skills_sync()` to sync tick in `engine/mod.rs`
- Git clone/pull via `tokio::process::Command`
- Config: `skills` array with `repo` and `path` keys
- Context builder reads from `~/.orch/skills/` in addition to project-local paths

---

## 5. Dashboard CLI

**Priority:** Low
**Status:** Missing
**Issue:** To be created

### Design

`orch dashboard` — pretty-printed task overview grouped by status with counts, recent activity, and active sessions.

```
$ orch dashboard

Tasks (23 total)
  new:          3   ████
  routed:       1   █
  in_progress:  2   ██
  in_review:    4   ████
  done:        12   ████████████
  needs_review: 1   █

Active Sessions
  orch-myproject-42  claude  #42 Add login page        12m ago
  orch-myproject-45  codex   #45 Fix CSS regression     3m ago

Recent (last 24h)
  ✅ #41 Update README           claude  done        2h ago
  ✅ #40 Add tests for parser    codex   done        5h ago
  ⚠️ #39 Migrate database        claude  needs_review 8h ago
```

### Implementation

- Combine `orch task status` + `orch task live` + `orch metrics` into one view
- Bar chart using unicode block characters
- Color output via ANSI codes (when terminal supports it)

---

## 6. Task Tree CLI

**Priority:** Low
**Status:** Missing
**Issue:** To be created

### Design

`orch task tree` — ASCII tree of parent-child task relationships.

```
$ orch task tree

#30 [blocked] Implement auth system
├── #31 [done] Add user model
├── #32 [in_progress] Add login API
└── #33 [new] Add session middleware

#35 [in_progress] Refactor database layer
    (no children)
```

### Implementation

- Query all tasks, build adjacency list from `parent:*` labels
- Render with unicode box-drawing characters
- Show status, agent, title for each node

---

## 7. Owner Commands

**Priority:** Medium
**Status:** Missing
**Issue:** To be created

### Design

Detect slash commands in GitHub issue comments and execute them:

| Command | Action |
|---------|--------|
| `/retry` | Reset task to `status:new`, re-dispatch |
| `/reroute [agent]` | Clear agent, re-route (optionally force agent) |
| `/close` | Mark `status:done`, close issue |
| `/block [reason]` | Mark `status:blocked` with reason |
| `/unblock` | Mark `status:new`, re-dispatch |
| `/review` | Trigger review agent on current PR |

### Implementation

- In `scan_mentions()` or a new `scan_commands()` in sync tick
- Parse comment body for `/command` prefix
- Validate author is repo owner or collaborator
- Execute command and post confirmation comment
- Dedup by comment ID (same as mention scanning)
