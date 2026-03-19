+++
title = "Channels — Telegram & Discord"
description = "Set up and operate the Telegram and Discord integrations"
weight = 8
+++

Orch supports bidirectional communication via Telegram and Discord. Both channels receive incoming commands and stream task completion notifications back to you.

## Overview

When a channel is configured:

- **Incoming**: messages/commands sent to the bot are received by the engine and can trigger internal tasks
- **Outgoing**: task completion notifications are pushed to the configured chat/channel
- **Live streaming**: agent output is captured from tmux and forwarded in real-time (rate-limited per platform)

The engine initialises channels at startup. A failed health check (`getMe` / `GET /users/@me`) causes the channel to be skipped with a warning — the service continues without it.

---

## Telegram

### 1. Create a bot

1. Open Telegram and message **@BotFather**
2. Send `/newbot`, follow the prompts to pick a name and username
3. Copy the token BotFather returns: `123456789:ABCDef...`

### 2. Get your chat ID

Send any message to your bot, then open:

```
https://api.telegram.org/bot<TOKEN>/getUpdates
```

The `chat.id` field in the response is your `chat_id`. It is a signed integer (negative for groups, positive for private chats).

### 3. Configure orch

Add the following to `~/.orch/config.yml`:

```yaml
channels:
  telegram:
    bot_token: "${TELEGRAM_BOT_TOKEN}"   # required
    chat_id: "123456789"                  # optional: restrict sends to one chat
```

Store the token in `~/.private` so the brew service picks it up:

```bash
echo 'export TELEGRAM_BOT_TOKEN="123456789:ABCDef..."' >> ~/.private
chmod 600 ~/.private
```

### 4. Config reference

| Key | Required | Description |
|-----|----------|-------------|
| `channels.telegram.bot_token` | Yes | Bot token from BotFather |
| `channels.telegram.chat_id` | No | Restrict outgoing messages to this chat. If omitted, the bot can only receive (no outgoing sends) |

### How it works

- **Polling**: the channel uses long-polling (`getUpdates` with `timeout=30`) — no webhook setup is needed
- **Offset tracking**: the offset is advanced after each batch so messages are never re-processed across reconnects
- **Empty messages**: messages with no text body are silently dropped
- **Parse mode**: outgoing messages use Telegram's `Markdown` parse mode (MarkdownV1). Special characters (`_`, `*`, `[`, `` ` ``) in task titles and summaries are automatically escaped

### Notification format

```
✅ *Task #42*: done
*Fix auth bug*
Agent: `claude` | Duration: 2m 0s

Fixed the OAuth flow — PAT token refresh now retries on 401.
```

Status emojis: ✅ done · 🔄 in_progress · 🔍 in_review · ⚠️ needs_review · 🚫 blocked · ❌ failed

---

## Discord

### 1. Create a bot

1. Go to the [Discord Developer Portal](https://discord.com/developers/applications)
2. Click **New Application**, give it a name
3. Go to **Bot** → **Add Bot**
4. Under **Token**, click **Reset Token** and copy the token
5. Enable these **Privileged Gateway Intents** (required):
   - `SERVER MEMBERS INTENT` — not needed
   - `MESSAGE CONTENT INTENT` — **required** (privileged, must be enabled explicitly)
6. Go to **OAuth2 → URL Generator**, select scopes `bot` and permission `Send Messages`
7. Open the generated URL in your browser to invite the bot to your server

### 2. Get your channel ID

In Discord, enable **Developer Mode** (Settings → Advanced → Developer Mode), then right-click the target text channel and select **Copy Channel ID**.

### 3. Configure orch

```yaml
channels:
  discord:
    bot_token: "${DISCORD_BOT_TOKEN}"      # required
    channel_id: "1234567890123456789"       # optional: restrict to one channel
    shard_id: 0                             # optional (default: 0)
    shard_count: 1                          # optional (default: 1)
```

Store the token in `~/.private`:

```bash
echo 'export DISCORD_BOT_TOKEN="MTI3..."' >> ~/.private
chmod 600 ~/.private
```

### 4. Config reference

| Key | Required | Description |
|-----|----------|-------------|
| `channels.discord.bot_token` | Yes | Bot token from the Developer Portal |
| `channels.discord.channel_id` | No | Restrict incoming and outgoing messages to this channel ID. If omitted, the bot listens to all channels in the server |
| `channels.discord.shard_id` | No | Shard index (default: `0`). Only needed for bots in 2,500+ guilds |
| `channels.discord.shard_count` | No | Total shard count (default: `1`). Only needed for bots in 2,500+ guilds |

### Required Gateway Intents

| Intent | Bit | Purpose |
|--------|-----|---------|
| `GUILDS` | `1 << 0` | Receive guild context |
| `GUILD_MESSAGES` | `1 << 9` | Receive MESSAGE_CREATE events |
| `MESSAGE_CONTENT` | `1 << 15` | Read message text (privileged — must be enabled in the portal) |

The bitmask `1 | (1 << 9) | (1 << 15) = 33281` is sent in the Identify payload automatically.

### How it works

The Discord channel uses Discord's **Gateway websocket API** (`wss://gateway.discord.gg`), not HTTP polling. This delivers `MESSAGE_CREATE` events in real-time with sub-second latency.

**Protocol flow:**

1. **Connect** — open persistent websocket to `wss://gateway.discord.gg/?v=10&encoding=json`
2. **Hello** (op=10) — server sends `heartbeat_interval` (≈41 s)
3. **Identify** (op=2) — client sends bot token + intents + shard info
4. **Ready** (op=0/READY) — server responds with `session_id` and `resume_gateway_url`
5. **Events** — `MESSAGE_CREATE` dispatches arrive in real-time
6. **Heartbeat** — client sends op=1 every `heartbeat_interval` ms; server ACKs with op=11
7. **Reconnect** — on disconnect, client resumes using `session_id + seq`; falls back to re-identify on invalid session (op=9 with `resumable=false`), with a 5 s delay before re-identifying

**Backoff:** failed connect attempts start at 1 s and double up to 60 s.

**Bot messages are ignored:** the channel filters out messages where `author.bot == true` to prevent feedback loops.

### Notification format

```
✅ **Task #42**: done
**Fix auth bug**
Agent: `claude` | Duration: 2m 0s

Fixed the OAuth flow.
```

Discord uses standard Markdown bold (`**`) instead of Telegram-style `*`.

---

## Notification levels

Both channels respect the `notifications.level` config key:

```yaml
notifications:
  level: all        # all | errors_only | none
```

| Level | Sends notifications for |
|-------|------------------------|
| `all` | Every task completion (default) |
| `errors_only` | `needs_review`, `blocked`, `failed` only |
| `none` | Disabled |

---

## Troubleshooting

### Channel not registering at startup

Check the service log:

```bash
tail -f ~/.orch/state/orch.log | grep -E "telegram|discord"
```

Possible log messages:

| Message | Cause |
|---------|-------|
| `telegram channel health check failed, skipping` | Invalid token or no network |
| `discord gateway health check failed, skipping` | Invalid token or `MESSAGE_CONTENT` intent not enabled |
| `telegram channel registered` | Success |
| `discord gateway registered` | Success |

### Telegram: bot not responding

1. Verify the token with `curl https://api.telegram.org/bot<TOKEN>/getMe`
2. Make sure the service sources `~/.private`: `grep TELEGRAM ~/.private`
3. Check `chat_id` format — it must be a numeric string (e.g. `"-100123456789"` for supergroups)

### Telegram: messages not sent

`send` fails if `chat_id` is not configured. Add it to `channels.telegram.chat_id` and restart the service.

### Discord: no events received

1. Confirm `MESSAGE_CONTENT` privileged intent is enabled in the Developer Portal
2. Verify the bot is a member of the server (was invited via OAuth2 URL)
3. If `channel_id` is set, verify the bot has `View Channel` + `Read Message History` permissions on that channel
4. Check the log for `discord gateway: ready` — if missing, authentication failed

### Discord: heartbeat zombie detection

If the service log shows `heartbeat not acknowledged (zombie connection), reconnecting`, the gateway dropped the connection silently. The client reconnects automatically using the resume URL.

### Restart after config changes

Config changes to `channels.*` require a service restart:

```bash
orch service restart
```
