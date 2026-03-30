# Multi-Project Channel Routing — Design Spec

## Problem

Channels (Telegram, Discord) are currently single-target: one chat_id, one channel_id. All channel-created tasks go to the first configured project. Notifications broadcast to all channels without project filtering. There is no `/stats` command.

## Solution

Map projects to Telegram forum topics and Discord channels within the same server/bot. Add interactive project picker for the General channel. Add per-project notification routing. Add `/stats` command and `orch stats` CLI.

---

## Config Architecture

### Global config (`~/.orch/config.yml`) — credentials only

```yaml
channels:
  telegram:
    bot_token: "${TELEGRAM_BOT_TOKEN}"
    chat_id: "supergroup_id"
    general_topic_id: "1"
  discord:
    bot_token: "${DISCORD_BOT_TOKEN}"
    guild_id: "1234567890"
    general_channel_id: "9876543210"
```

### Per-project config (`.orch.yml`) — routing

```yaml
gh:
  repo: gabrielkoerich/orch
channels:
  telegram:
    topic_id: "42"
  discord:
    channel_id: "1111111111"
```

Per-project can override `bot_token`, `chat_id` (Telegram) or `bot_token`, `guild_id` (Discord) for projects that use a dedicated bot.

### Resolution

At engine startup, read all project configs and build reverse lookup maps:
- Telegram: `(bot_token, chat_id, topic_id)` → `repo`
- Discord: `(bot_token, channel_id)` → `repo`

Projects sharing the same bot token share the same connection.

---

## Channel Architecture

### Connections vs Targets

- **Connection**: One per unique bot token. Telegram long-polls once, Discord has one websocket.
- **Target**: `(channel_name, topic_id/channel_id)` — where to send messages. Each project has one target per channel.

### IncomingMessage changes

Add `topic_id: Option<String>` field. Telegram sets this from `message_thread_id`. Discord already uses `channel_id` as `thread_id`.

### OutgoingMessage changes

Add `topic_id: Option<String>` field so `send()` can target specific topics/channels.

### Message routing

1. Message arrives with topic/channel identifier
2. Look up project from reverse map
3. If found → scope to that project
4. If not found → General channel behavior

---

## Interactive Project Picker

When free text arrives in General (not a command):

1. Bot replies with inline buttons — one per configured project
2. User taps a project
3. Bot creates task in chosen project, binds thread, starts streaming

### Implementation

- Telegram: `InlineKeyboardMarkup` with `callback_data` per project. Listen for `callback_query` updates.
- Discord: Message with `ActionRow` buttons. Listen for `INTERACTION_CREATE` events.
- Pending picks stored in-memory `HashMap<message_id, PendingPick>` with 60s timeout.

---

## Notification Routing

### Automatic

Dedicated project channels/topics get notifications automatically. No subscription needed — engine knows the mapping from config.

### Subscriptions

General (or any other channel) can subscribe to project notifications via `/subscribe <project>`.

SQLite table:
```sql
CREATE TABLE channel_subscriptions (
    channel TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    repo TEXT NOT NULL,
    PRIMARY KEY (channel, thread_id, repo)
);
```

### Format

General channel notifications include project prefix:
```
[orch] internal-3500: Fix cron DOW normalization
Agent: claude | Duration: 2m 30s
```

Dedicated channel notifications omit the prefix (project is implicit).

---

## Commands

| Command | Dedicated Channel | General Channel |
|---------|------------------|-----------------|
| `/status` | In-progress tasks for this project | All projects, grouped |
| `/stats` | Metrics for this project | All projects, per-project |
| `/subscribe <project>` | N/A | Subscribe to notifications |
| `/unsubscribe <project>` | N/A | Unsubscribe |
| `/stream <task_id>` | Stream task output here | Stream task output here |
| Free text | Create task in project | Project picker → create task |
| `/retry`, `/close`, etc. | When bound to task | When bound to task |

---

## `orch stats` CLI

Default: per-project tables stacked.

```
$ orch stats

-- orch (gabrielkoerich/orch) -----------------
  Tasks (24h):  12 completed, 2 failed
  Success rate: 85.7%
  Avg duration: simple 1.2m | medium 4.5m | complex 12.3m
  Agents:       claude: 8 (87%) | codex: 4 (75%)
  Cost (24h):   $1.23

-- bean (gabrielkoerich/bean) ------------------
  Tasks (24h):  5 completed, 0 failed
  Success rate: 100%
  Avg duration: simple 0.8m | medium 3.2m
  Agents:       claude: 5 (100%)
  Cost (24h):   $0.45
```

`--all` flag aggregates into one table.

---

## Scope Boundaries

**In scope:**
- Per-project channel mapping via config
- Telegram forum topic support
- Discord multi-channel support
- Interactive project picker (inline buttons)
- Notification routing (auto + subscription)
- `/status`, `/stats`, `/subscribe`, `/unsubscribe`, `/stream` commands
- `orch stats` CLI command

**Out of scope (future):**
- Discord threads per task
- Slack multi-channel
- `orch dashboard` fixes
- Per-project bot token multiplexing (acknowledge in design, defer implementation — all projects share one bot for v1)
