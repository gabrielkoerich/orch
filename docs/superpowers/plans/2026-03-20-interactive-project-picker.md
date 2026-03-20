# Interactive Project Picker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When free text arrives in the General channel with multiple projects configured, send an inline keyboard so the user can pick the target project before creating the task.

**Architecture:** Add `send_keyboard` to the `Channel` trait (default returns `Err`). Implement it in `TelegramChannel` and `DiscordGateway`. In the engine's message loop, pass an `Arc<Mutex<HashMap<String, PendingPick>>>` to `handle_channel_message`. In the `NewTask` arm, send the keyboard and store a `PendingPick`. At the top of each `handle_channel_message` call, detect picker-response messages (`body.starts_with("pick:")`) and resolve them to task creation.

**Tech Stack:** Rust, async_trait, tokio, serde_json

---

## File Map

| File | Change |
|------|--------|
| `src/channels/mod.rs` | Add `send_keyboard` to Channel trait with default `Err` impl |
| `src/channels/telegram.rs` | Implement `send_keyboard` using existing `send_inline_keyboard` |
| `src/channels/discord_ws.rs` | Implement `send_keyboard` using existing `send_with_buttons` |
| `src/engine/mod.rs` | Add `PendingPick`, `PendingPicks` type; add `send_channel_keyboard` helper; wire `pending_picks` into the message loop; add picker detection and resolution logic in `handle_channel_message`; show picker in `NewTask` arm |

---

## Task 1: Add `send_keyboard` to the Channel trait

**Files:**
- Modify: `src/channels/mod.rs`

- [ ] **Step 1: Add `send_keyboard` to the Channel trait with a default Err impl**

  In `src/channels/mod.rs`, inside the `Channel` trait after the `send` method, add:

  ```rust
  /// Send a message with inline keyboard / action-row buttons.
  /// Returns the message ID of the sent message (used to key the pending pick).
  /// Default impl returns Err — only channels that support interactive messages
  /// need to override this.
  async fn send_keyboard(
      &self,
      thread_id: &str,
      topic_id: Option<&str>,
      text: &str,
      buttons: &[(String, String)], // (label, callback_data / custom_id)
  ) -> anyhow::Result<String> {
      let _ = (thread_id, topic_id, text, buttons);
      anyhow::bail!("channel does not support interactive messages")
  }
  ```

- [ ] **Step 2: Verify the project compiles**

  ```bash
  cargo check 2>&1 | head -30
  ```
  Expected: no errors (the default impl satisfies all existing Channel impls).

- [ ] **Step 3: Commit**

  ```bash
  git add src/channels/mod.rs
  git commit -m "feat(channels): add send_keyboard to Channel trait with default Err impl"
  ```

---

## Task 2: Implement `send_keyboard` for TelegramChannel

**Files:**
- Modify: `src/channels/telegram.rs`

- [ ] **Step 1: Add a unit test for the keyboard output shape**

  At the bottom of `src/channels/telegram.rs`, inside the `#[cfg(test)]` section (or add one):

  ```rust
  #[cfg(test)]
  mod tests {
      // We can't send HTTP in unit tests, but we verify the method exists on the trait.
      use super::*;
      use crate::channels::Channel;

      #[test]
      fn telegram_channel_is_channel() {
          // Static type check — TelegramChannel implements Channel
          fn assert_channel<T: Channel>() {}
          assert_channel::<TelegramChannel>();
      }
  }
  ```

  Run:
  ```bash
  cargo test -p orch --lib channels::telegram 2>&1
  ```
  Expected: passes (existing behavior).

- [ ] **Step 2: Implement `send_keyboard` on `TelegramChannel`**

  In `src/channels/telegram.rs`, inside the `#[async_trait] impl Channel for TelegramChannel` block, add after `send`:

  ```rust
  async fn send_keyboard(
      &self,
      _thread_id: &str,
      topic_id: Option<&str>,
      text: &str,
      buttons: &[(String, String)],
  ) -> anyhow::Result<String> {
      let chat_id = self
          .chat_id
          .as_ref()
          .ok_or_else(|| anyhow::anyhow!("telegram chat_id not configured"))?
          .parse::<i64>()
          .map_err(|_| anyhow::anyhow!("invalid chat_id"))?;

      let topic_id_i64 = topic_id.and_then(|t| t.parse::<i64>().ok());
      let msg_id = self
          .send_inline_keyboard(chat_id, topic_id_i64, text, buttons)
          .await?;
      Ok(msg_id.to_string())
  }
  ```

- [ ] **Step 3: Verify compilation**

  ```bash
  cargo check 2>&1 | head -30
  ```
  Expected: clean.

- [ ] **Step 4: Commit**

  ```bash
  git add src/channels/telegram.rs
  git commit -m "feat(telegram): implement send_keyboard via send_inline_keyboard"
  ```

---

## Task 3: Implement `send_keyboard` for DiscordGateway

**Files:**
- Modify: `src/channels/discord_ws.rs`

- [ ] **Step 1: Implement `send_keyboard` on `DiscordGateway`**

  In `src/channels/discord_ws.rs`, inside `impl Channel for DiscordGateway`, add after `send`:

  ```rust
  async fn send_keyboard(
      &self,
      thread_id: &str,
      _topic_id: Option<&str>,
      text: &str,
      buttons: &[(String, String)],
  ) -> anyhow::Result<String> {
      // For Discord, thread_id == channel_id
      self.send_with_buttons(thread_id, text, buttons).await
  }
  ```

- [ ] **Step 2: Verify compilation**

  ```bash
  cargo check 2>&1 | head -30
  ```
  Expected: clean.

- [ ] **Step 3: Commit**

  ```bash
  git add src/channels/discord_ws.rs
  git commit -m "feat(discord): implement send_keyboard via send_with_buttons"
  ```

---

## Task 4: PendingPick + send_channel_keyboard in engine/mod.rs

**Files:**
- Modify: `src/engine/mod.rs`

- [ ] **Step 1: Add PendingPick struct and type alias**

  Near the top of `src/engine/mod.rs` (after the `use` statements, before any functions), add:

  ```rust
  /// A pending project-pick waiting for the user to tap a button.
  struct PendingPick {
      /// Original message body (the free-text request to create a task).
      original_body: String,
      /// The topic/thread that should receive follow-up messages.
      msg_topic_id: Option<String>,
      /// When this pick was created (used to enforce 60s timeout).
      created_at: std::time::Instant,
  }

  /// Keyed by `"{channel}:{thread_id}"`.
  type PendingPicks = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, PendingPick>>>;
  ```

- [ ] **Step 2: Add `send_channel_keyboard` helper function**

  After the existing `send_channel_reply` function in `src/engine/mod.rs`, add:

  ```rust
  /// Send an inline keyboard to a specific channel.
  /// Returns the message ID of the keyboard message, or an empty string on failure.
  async fn send_channel_keyboard(
      channels: &Arc<ChannelRegistry>,
      channel_name: &str,
      thread_id: &str,
      topic_id: Option<&str>,
      text: &str,
      buttons: &[(String, String)],
  ) -> String {
      for ch in channels.iter() {
          if ch.name() == channel_name {
              match ch.send_keyboard(thread_id, topic_id, text, buttons).await {
                  Ok(msg_id) => return msg_id,
                  Err(e) => {
                      tracing::warn!(
                          channel = channel_name,
                          ?e,
                          "failed to send project picker keyboard"
                      );
                      return String::new();
                  }
              }
          }
      }
      tracing::debug!(channel = channel_name, "channel not found for keyboard send");
      String::new()
  }
  ```

- [ ] **Step 3: Verify compilation**

  ```bash
  cargo check 2>&1 | head -30
  ```

- [ ] **Step 4: Commit**

  ```bash
  git add src/engine/mod.rs
  git commit -m "feat(engine): add PendingPick, PendingPicks, send_channel_keyboard"
  ```

---

## Task 5: Wire `pending_picks` into the message loop and `handle_channel_message`

**Files:**
- Modify: `src/engine/mod.rs`

- [ ] **Step 1: Create `pending_picks` before spawning the message loop**

  In `src/engine/mod.rs`, find the block that sets up `channel_receivers` (around line 505–528). Just before the `for mut rx in channel_receivers {` loop, add:

  ```rust
  // Shared map for project-picker state: (channel:thread_id) → PendingPick
  let pending_picks: PendingPicks = std::sync::Arc::new(
      std::sync::Mutex::new(std::collections::HashMap::new()),
  );
  ```

- [ ] **Step 2: Clone `pending_picks` into each spawned task**

  Inside the `for mut rx in channel_receivers {` loop, after the existing clones, add:

  ```rust
  let pending_picks = pending_picks.clone();
  ```

  Then add `pending_picks` as a final argument to `handle_channel_message`:

  ```rust
  handle_channel_message(
      msg,
      &transport,
      &tmux,
      &capture,
      &channels,
      &engine_refs,
      &ch_router,
      &pending_picks,   // ← new
  )
  .await;
  ```

- [ ] **Step 3: Add `pending_picks` parameter to `handle_channel_message` signature**

  Find:
  ```rust
  async fn handle_channel_message(
      msg: IncomingMessage,
      transport: &Arc<Transport>,
      _tmux: &Arc<TmuxManager>,
      capture: &Arc<CaptureService>,
      channels: &Arc<ChannelRegistry>,
      engine_refs: &[EngineRef],
      channel_router: &Arc<ChannelRouter>,
  ) {
  ```

  Change to:
  ```rust
  async fn handle_channel_message(
      msg: IncomingMessage,
      transport: &Arc<Transport>,
      _tmux: &Arc<TmuxManager>,
      capture: &Arc<CaptureService>,
      channels: &Arc<ChannelRegistry>,
      engine_refs: &[EngineRef],
      channel_router: &Arc<ChannelRouter>,
      pending_picks: &PendingPicks,
  ) {
  ```

- [ ] **Step 4: Verify compilation**

  ```bash
  cargo check 2>&1 | head -30
  ```
  Expected: clean.

- [ ] **Step 5: Commit**

  ```bash
  git add src/engine/mod.rs
  git commit -m "feat(engine): wire pending_picks into handle_channel_message"
  ```

---

## Task 6: Detect and resolve picker callbacks in `handle_channel_message`

**Files:**
- Modify: `src/engine/mod.rs`

This task handles the callback/interaction response from the user tapping a button. It must be checked **before** `transport.route()` so the callback doesn't fall through to task creation.

- [ ] **Step 1: Write a unit test for the pending-pick key construction**

  Add inside the `#[cfg(test)]` section at the bottom of `src/engine/mod.rs` (or create one):

  ```rust
  #[cfg(test)]
  mod tests {
      #[test]
      fn pending_pick_key_format() {
          let channel = "telegram";
          let thread_id = "-100123456";
          let key = format!("{channel}:{thread_id}");
          assert_eq!(key, "telegram:-100123456");
      }

      #[test]
      fn pick_body_prefix() {
          let body = "pick:owner/orch";
          assert!(body.starts_with("pick:"));
          assert_eq!(&body[5..], "owner/orch");
      }
  }
  ```

  Run:
  ```bash
  cargo test -p orch --lib engine::tests 2>&1
  ```
  Expected: passes.

- [ ] **Step 2: Add picker-response detection at the top of `handle_channel_message`**

  At the top of `handle_channel_message`, after the existing project resolution lines and **before** `match transport.route(&msg).await {`, add:

  ```rust
  // ── Picker response: user tapped a project button ────────────────────────
  let is_picker_response = msg.body.starts_with("pick:")
      && (msg.metadata.get("callback_query_id").is_some()
          || msg.metadata.get("interaction_id").is_some());

  if is_picker_response {
      let repo = msg.body["pick:".len()..].to_string();
      let pick_key = format!("{}:{}", msg.channel, msg.thread_id);
      let pending = {
          let mut map = pending_picks.lock().unwrap_or_else(|e| e.into_inner());
          map.remove(&pick_key)
      };

      match pending {
          None => {
              send_channel_reply(
                  channels,
                  &msg.channel,
                  &msg.thread_id,
                  "No pending project selection (may have timed out).".to_string(),
                  msg.topic_id.as_deref(),
              )
              .await;
          }
          Some(pick) if pick.created_at.elapsed().as_secs() > 60 => {
              send_channel_reply(
                  channels,
                  &msg.channel,
                  &msg.thread_id,
                  "Project selection timed out (60s). Please send your message again.".to_string(),
                  pick.msg_topic_id.as_deref(),
              )
              .await;
          }
          Some(pick) => {
              // Find the target engine ref for the selected repo
              if let Some((_, _, task_manager, _)) =
                  engine_refs.iter().find(|(r, _, _, _)| r == &repo)
              {
                  use crate::channels::stream::fanout_output;
                  use crate::engine::tasks::{CreateTaskRequest, TaskType};

                  let title = if pick.original_body.chars().count() > 80 {
                      let truncated: String = pick.original_body.chars().take(80).collect();
                      format!("{}…", truncated)
                  } else {
                      pick.original_body.clone()
                  };
                  let req = CreateTaskRequest {
                      title,
                      body: pick.original_body.clone(),
                      task_type: TaskType::Internal,
                      labels: vec!["channel-created".to_string()],
                      source: msg.channel.clone(),
                      source_id: msg.thread_id.clone(),
                  };
                  match task_manager.create_task(req).await {
                      Ok(task) => {
                          use crate::engine::tasks::Task;
                          let task_id = match &task {
                              Task::Internal(t) => format!("internal:{}", t.id),
                              Task::External(t) => t.id.0.clone(),
                          };
                          transport
                              .bind(
                                  &task_id,
                                  &format!("orch-{repo}-{task_id}"),
                                  &msg.channel,
                                  &msg.thread_id,
                              )
                              .await;
                          capture
                              .register_session(&task_id, &format!("orch-{repo}-{task_id}"))
                              .await;
                          let transport_clone = transport.clone();
                          let channels_clone = channels.clone();
                          let task_id_clone = task_id.clone();
                          tokio::spawn(async move {
                              fanout_output(task_id_clone, transport_clone, channels_clone).await;
                          });
                          let reply = format!(
                              "Task created: `{task_id}` in [{repo}] — I'll start working on it now."
                          );
                          send_channel_reply(
                              channels,
                              &msg.channel,
                              &msg.thread_id,
                              reply,
                              pick.msg_topic_id.as_deref(),
                          )
                          .await;
                      }
                      Err(e) => {
                          tracing::warn!(repo, err = %e, "failed to create task from picker selection");
                          send_channel_reply(
                              channels,
                              &msg.channel,
                              &msg.thread_id,
                              format!("Failed to create task: {e}"),
                              pick.msg_topic_id.as_deref(),
                          )
                          .await;
                      }
                  }
              } else {
                  send_channel_reply(
                      channels,
                      &msg.channel,
                      &msg.thread_id,
                      format!("Project `{repo}` not found. Please try again."),
                      pick.msg_topic_id.as_deref(),
                  )
                  .await;
              }
          }
      }
      return; // Picker response fully handled — skip normal routing
  }
  ```

- [ ] **Step 3: Verify compilation**

  ```bash
  cargo check 2>&1 | head -30
  ```

- [ ] **Step 4: Commit**

  ```bash
  git add src/engine/mod.rs
  git commit -m "feat(engine): handle picker callback — resolve pending pick and create task"
  ```

---

## Task 7: Show project picker in the NewTask arm

**Files:**
- Modify: `src/engine/mod.rs`

This task replaces the silent `engine_refs.first()` fallback with an interactive picker when in the General channel with multiple projects.

- [ ] **Step 1: Add a unit test for the picker-trigger condition**

  Add to the `#[cfg(test)]` section in `src/engine/mod.rs`:

  ```rust
  #[test]
  fn picker_triggered_when_general_and_multiple_projects() {
      // Simulate the condition: is_general=true, resolved_repo=None, 2+ projects
      let is_general = true;
      let resolved_repo: Option<&str> = None;
      let project_count = 2usize;

      let should_show_picker = is_general && resolved_repo.is_none() && project_count > 1;
      assert!(should_show_picker);
  }

  #[test]
  fn picker_not_triggered_when_single_project() {
      let is_general = true;
      let resolved_repo: Option<&str> = None;
      let project_count = 1usize;

      let should_show_picker = is_general && resolved_repo.is_none() && project_count > 1;
      assert!(!should_show_picker);
  }

  #[test]
  fn picker_not_triggered_when_repo_resolved() {
      let is_general = true;
      let resolved_repo: Option<&str> = Some("owner/orch");
      let project_count = 2usize;

      let should_show_picker = is_general && resolved_repo.is_none() && project_count > 1;
      assert!(!should_show_picker);
  }
  ```

  Run:
  ```bash
  cargo test -p orch --lib engine::tests 2>&1
  ```
  Expected: all pass.

- [ ] **Step 2: Replace the `NewTask` fallback with picker logic**

  In the `MessageRoute::NewTask` arm, find:
  ```rust
  // Resolve target project: resolved_repo → specific project, else first
  let target_engine_ref = if let Some(repo) = resolved_repo {
      engine_refs.iter().find(|(r, _, _, _)| r == repo)
  } else {
      engine_refs.first()  // ← silently picks first project
  };
  ```

  Replace the entire `MessageRoute::NewTask` arm content with:

  ```rust
  MessageRoute::NewTask => {
      let channel = msg.channel.clone();
      let thread_id = msg.thread_id.clone();

      // Show interactive picker when in General with multiple projects configured
      if is_general && resolved_repo.is_none() && engine_refs.len() > 1 {
          let buttons: Vec<(String, String)> = engine_refs
              .iter()
              .map(|(repo, _, _, _)| {
                  // Use the short repo name (after the slash) as button label
                  let label = repo
                      .rsplit('/')
                      .next()
                      .unwrap_or(repo.as_str())
                      .to_string();
                  (label, format!("pick:{repo}"))
              })
              .collect();

          let picker_text = "Which project should I create this task in?";
          send_channel_keyboard(
              channels,
              &channel,
              &thread_id,
              msg_topic_id.as_deref(),
              picker_text,
              &buttons,
          )
          .await;

          // Store the pending pick
          let pick_key = format!("{channel}:{thread_id}");
          let pick = PendingPick {
              original_body: msg.body.clone(),
              msg_topic_id: msg_topic_id.clone(),
              created_at: std::time::Instant::now(),
          };
          {
              let mut map = pending_picks.lock().unwrap_or_else(|e| e.into_inner());
              map.insert(pick_key, pick);
          }
          return; // Wait for the user's button tap
      }

      // Single project or already resolved → create immediately
      let target_engine_ref = if let Some(repo) = resolved_repo {
          engine_refs.iter().find(|(r, _, _, _)| r == repo)
      } else {
          engine_refs.first()
      };

      if let Some((repo, _, task_manager, _)) = target_engine_ref {
          // (existing task-creation code unchanged below this point)
  ```

  > **Note:** The closing brace structure and the existing task-creation code inside the `if let Some(...)` block remains unchanged — only the guard at the top of the arm changes.

- [ ] **Step 3: Verify compilation**

  ```bash
  cargo check 2>&1 | head -30
  ```

- [ ] **Step 4: Run all tests**

  ```bash
  cargo nextest run 2>&1
  ```
  Expected: all pass.

- [ ] **Step 5: Run the full CI gate**

  ```bash
  cargo fmt -- --check && cargo clippy --all-targets -- -D warnings && cargo nextest run
  ```
  Expected: all pass with no warnings.

- [ ] **Step 6: Commit**

  ```bash
  git add src/engine/mod.rs
  git commit -m "feat(engine): show project picker in General when multiple projects configured"
  ```

---

## Task 8: Final integration check

- [ ] **Step 1: Run the full CI gate one final time**

  ```bash
  cargo fmt -- --check && cargo clippy --all-targets -- -D warnings && cargo nextest run
  ```
  Expected: clean.

- [ ] **Step 2: Push branch**

  ```bash
  git push -u origin HEAD
  ```

- [ ] **Step 3: Open PR**

  ```bash
  gh pr create \
    --title "feat: interactive project picker for General channel" \
    --body "$(cat <<'EOF'
  ## Summary
  - Adds `send_keyboard` to the `Channel` trait (default returns `Err`; implemented for Telegram and Discord)
  - When free text arrives in General with 2+ projects configured, sends an inline keyboard instead of silently picking the first project
  - Stores pending pick in `Arc<Mutex<HashMap>>` keyed by `channel:thread_id`; 60s timeout with user-facing message
  - When user taps a button (`pick:{repo}` callback), creates the task in the chosen project, binds thread, and starts output fanout
  - Existing single-project and resolved-project behaviour is unchanged

  ## Test plan
  - [ ] Unit tests for picker-trigger condition pass
  - [ ] Unit tests for pending-pick key format pass
  - [ ] Cargo fmt/clippy/nextest all pass

  Closes #728

  🤖 Generated with [Claude Code](https://claude.com/claude-code)
  EOF
  )"
  ```

---

## Acceptance Criteria Checklist

- [ ] Free text in General when 1 project → create immediately (no picker)
- [ ] Free text in General when 2+ projects → inline keyboard shown
- [ ] Tapping a button creates the task in the selected project
- [ ] Pick times out after 60s with a user-facing message
- [ ] Works for both Telegram (`callback_query`) and Discord (`INTERACTION_CREATE`)
- [ ] All CI checks pass
