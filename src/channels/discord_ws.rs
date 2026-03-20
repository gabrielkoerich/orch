//! Discord Gateway websocket client.
//!
//! Delivers real-time events from Discord's Gateway API (wss://gateway.discord.gg)
//! replacing the HTTP polling approach in `discord.rs`.
//!
//! Protocol flow:
//!   connect → Hello → Identify → Ready → receive MESSAGE_CREATE events
//!   heartbeat every `heartbeat_interval` ms to keep the connection alive
//!   on disconnect: resume with session_id+seq, or re-identify from scratch

use super::{Channel, IncomingMessage, OutgoingMessage};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

// ── Gateway opcodes ──────────────────────────────────────────────────────────

const OP_DISPATCH: u64 = 0;
const OP_HEARTBEAT: u64 = 1;
const OP_IDENTIFY: u64 = 2;
const OP_RESUME: u64 = 6;
const OP_RECONNECT: u64 = 7;
const OP_INVALID_SESSION: u64 = 9;
const OP_HELLO: u64 = 10;
const OP_HEARTBEAT_ACK: u64 = 11;

/// Gateway intents bitmask.
/// GUILDS(1) | GUILD_MESSAGES(512) | MESSAGE_CONTENT(32768)
const GATEWAY_INTENTS: u64 = 1 | (1 << 9) | (1 << 15);

const DEFAULT_GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

// ── Public struct ────────────────────────────────────────────────────────────

/// Discord Gateway websocket client.
///
/// Receives real-time events via wss:// and sends messages via REST.
/// Supports sharding via `shard_id`/`shard_count` (both read from config).
pub struct DiscordGateway {
    token: String,
    client: Client,
    channel_id: Option<String>,
    shard_id: u64,
    shard_count: u64,
}

impl DiscordGateway {
    /// Create a new Gateway client.
    ///
    /// `shard_id` and `shard_count` follow Discord sharding conventions.
    /// For a single-shard bot, use `shard_id = 0, shard_count = 1`.
    pub fn new(token: String, channel_id: Option<String>, shard_id: u64, shard_count: u64) -> Self {
        Self {
            token,
            client: Client::new(),
            channel_id,
            shard_id,
            shard_count,
        }
    }

    fn api_url(&self, endpoint: &str) -> String {
        format!("https://discord.com/api/v10{endpoint}")
    }

    /// Send a message with action row buttons to a specific channel.
    ///
    /// Returns the message ID of the sent message.
    #[allow(dead_code)]
    pub async fn send_with_buttons(
        &self,
        channel_id: &str,
        text: &str,
        buttons: &[(String, String)], // (label, custom_id)
    ) -> anyhow::Result<String> {
        let components = vec![serde_json::json!({
            "type": 1, // ActionRow
            "components": buttons.iter().map(|(label, custom_id)| {
                serde_json::json!({
                    "type": 2, // Button
                    "style": 1, // Primary
                    "label": label,
                    "custom_id": custom_id
                })
            }).collect::<Vec<_>>()
        })];

        let body = serde_json::json!({
            "content": text,
            "components": components
        });

        let url = self.api_url(&format!("/channels/{channel_id}/messages"));
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bot {}", self.token))
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("discord API error: {}", body);
        }

        let result: serde_json::Value = response.json().await?;
        Ok(result["id"].as_str().unwrap_or("").to_string())
    }

    async fn send_message(&self, channel_id: &str, content: &str) -> anyhow::Result<()> {
        let url = self.api_url(&format!("/channels/{channel_id}/messages"));
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bot {}", self.token))
            .json(&serde_json::json!({ "content": content }))
            .send()
            .await?;

        if !resp.status().is_success() {
            anyhow::bail!(
                "discord send failed: {}",
                resp.text().await.unwrap_or_default()
            );
        }
        Ok(())
    }
}

// ── Channel impl ─────────────────────────────────────────────────────────────

#[async_trait]
impl Channel for DiscordGateway {
    fn name(&self) -> &str {
        "discord"
    }

    async fn start(&self) -> anyhow::Result<mpsc::Receiver<IncomingMessage>> {
        let (tx, rx) = mpsc::channel(64);
        let token = self.token.clone();
        let shard_id = self.shard_id;
        let shard_count = self.shard_count;
        let client = self.client.clone();

        tracing::info!(shard_id, shard_count, "discord gateway starting");

        tokio::spawn(async move {
            run_gateway(token, shard_id, shard_count, tx, client).await;
        });

        Ok(rx)
    }

    async fn send(&self, msg: &OutgoingMessage) -> anyhow::Result<()> {
        let target_channel = msg
            .topic_id
            .as_deref()
            .or(self.channel_id.as_deref())
            .ok_or_else(|| anyhow::anyhow!("no target channel for discord message"))?;
        self.send_message(target_channel, &msg.body).await
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        let resp = self
            .client
            .get(self.api_url("/users/@me"))
            .header("Authorization", format!("Bot {}", self.token))
            .send()
            .await?;

        if !resp.status().is_success() {
            anyhow::bail!(
                "discord health check failed: {}",
                resp.text().await.unwrap_or_default()
            );
        }
        tracing::info!("discord gateway health check passed");
        Ok(())
    }
}

// ── Gateway protocol ─────────────────────────────────────────────────────────

/// Parsed Gateway payload (op + d + s + t).
#[derive(Deserialize)]
struct GatewayPayload {
    op: u64,
    #[serde(default)]
    d: Value,
    s: Option<u64>,
    t: Option<String>,
}

/// Mutable session state shared across reconnect cycles.
struct GatewayState {
    session_id: Option<String>,
    last_seq: Option<u64>,
    resume_url: Option<String>,
}

impl GatewayState {
    fn new() -> Self {
        Self {
            session_id: None,
            last_seq: None,
            resume_url: None,
        }
    }

    fn clear(&mut self) {
        self.session_id = None;
        self.last_seq = None;
        self.resume_url = None;
    }

    fn ws_url(&self) -> String {
        self.resume_url
            .as_deref()
            .map(|u| format!("{}/?v=10&encoding=json", u.trim_end_matches('/')))
            .unwrap_or_else(|| DEFAULT_GATEWAY_URL.to_string())
    }
}

/// Main gateway loop — connects, handles protocol, reconnects on error.
async fn run_gateway(
    token: String,
    shard_id: u64,
    shard_count: u64,
    tx: mpsc::Sender<IncomingMessage>,
    client: Client,
) {
    let mut state = GatewayState::new();
    let mut backoff = Duration::from_secs(1);

    loop {
        let ws_url = state.ws_url();
        tracing::debug!(url = %ws_url, "connecting to discord gateway");

        match connect_async(&ws_url).await {
            Ok((ws, _)) => {
                backoff = Duration::from_secs(1); // reset on success

                let result =
                    handle_connection(ws, &token, shard_id, shard_count, &mut state, &tx, &client)
                        .await;

                match result {
                    Ok(true) => {
                        tracing::info!("discord gateway: disconnected (will resume)");
                    }
                    Ok(false) => {
                        tracing::info!(
                            "discord gateway: disconnected (not resumable), re-identifying"
                        );
                        state.clear();
                    }
                    Err(e) => {
                        tracing::warn!(?e, "discord gateway: connection error");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    ?e,
                    backoff_secs = backoff.as_secs(),
                    "discord gateway: connect failed"
                );
            }
        }

        if tx.is_closed() {
            tracing::debug!("discord gateway: receiver closed, shutting down");
            return;
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// Handle a single websocket connection lifecycle.
///
/// Returns `Ok(true)` if the session can be resumed, `Ok(false)` if we
/// need to re-identify (invalid session or clean disconnect).
async fn handle_connection(
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    token: &str,
    shard_id: u64,
    shard_count: u64,
    state: &mut GatewayState,
    tx: &mpsc::Sender<IncomingMessage>,
    client: &Client,
) -> anyhow::Result<bool> {
    let (mut write, mut read) = ws.split();

    // ── Step 1: Hello ────────────────────────────────────────────────────────
    let text = next_text_message(&mut read).await?;
    let hello: GatewayPayload = serde_json::from_str(&text)?;
    anyhow::ensure!(
        hello.op == OP_HELLO,
        "expected Hello (op=10), got op={}",
        hello.op
    );

    let hb_interval_ms = parse_heartbeat_interval(&hello.d["heartbeat_interval"]);
    tracing::debug!(hb_interval_ms, "discord gateway: Hello received");

    // ── Step 2: Identify or Resume ───────────────────────────────────────────
    let payload = if let (Some(sid), Some(seq)) = (state.session_id.as_deref(), state.last_seq) {
        tracing::info!(session_id = %sid, seq, "discord gateway: sending Resume");
        serde_json::json!({
            "op": OP_RESUME,
            "d": {
                "token": token,
                "session_id": sid,
                "seq": seq,
            }
        })
    } else {
        tracing::info!(shard_id, shard_count, "discord gateway: sending Identify");
        serde_json::json!({
            "op": OP_IDENTIFY,
            "d": {
                "token": token,
                "intents": GATEWAY_INTENTS,
                "properties": {
                    "os": std::env::consts::OS,
                    "browser": "orch",
                    "device": "orch",
                },
                "shard": [shard_id, shard_count],
            }
        })
    };

    write
        .send(Message::Text(payload.to_string()))
        .await
        .map_err(|e| anyhow::anyhow!("identify send failed: {e}"))?;

    // ── Step 3: Event loop ───────────────────────────────────────────────────
    let mut hb_ticker = tokio::time::interval(Duration::from_millis(hb_interval_ms));
    hb_ticker.tick().await; // consume the immediate first tick
    let mut ack_received = true;

    loop {
        tokio::select! {
            raw = read.next() => {
                let msg = match raw {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(e.into()),
                    None => {
                        tracing::debug!("discord gateway: stream closed");
                        return Ok(true); // try to resume
                    }
                };

                match msg {
                    Message::Text(text) => {
                        let payload: GatewayPayload = match serde_json::from_str(&text) {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::warn!(?e, "discord gateway: failed to parse payload");
                                continue;
                            }
                        };

                        if let Some(seq) = payload.s {
                            state.last_seq = Some(seq);
                        }

                        match payload.op {
                            OP_DISPATCH => {
                                if let Err(e) = handle_dispatch(
                                    payload.t.as_deref(),
                                    &payload.d,
                                    &mut state.session_id,
                                    &mut state.resume_url,
                                    tx,
                                    client,
                                )
                                .await
                                {
                                    tracing::warn!(?e, "discord gateway: dispatch error");
                                }
                                if tx.is_closed() {
                                    return Ok(false);
                                }
                            }
                            OP_HEARTBEAT => {
                                // Server explicitly requested a heartbeat
                                let d = state
                                    .last_seq
                                    .map(|s| serde_json::json!(s))
                                    .unwrap_or(Value::Null);
                                write
                                    .send(Message::Text(
                                        serde_json::json!({"op": OP_HEARTBEAT, "d": d}).to_string(),
                                    ))
                                    .await
                                    .map_err(|e| anyhow::anyhow!("heartbeat send failed: {e}"))?;
                            }
                            OP_HEARTBEAT_ACK => {
                                ack_received = true;
                            }
                            OP_RECONNECT => {
                                tracing::info!("discord gateway: server requested reconnect");
                                return Ok(true);
                            }
                            OP_INVALID_SESSION => {
                                let resumable = payload.d.as_bool().unwrap_or(false);
                                tracing::info!(resumable, "discord gateway: invalid session");
                                if !resumable {
                                    state.clear();
                                    // Brief delay before re-identifying to avoid rate limiting
                                    tokio::time::sleep(Duration::from_secs(5)).await;
                                }
                                return Ok(resumable);
                            }
                            op => {
                                tracing::debug!(op, "discord gateway: unhandled opcode");
                            }
                        }
                    }
                    Message::Close(frame) => {
                        tracing::info!(?frame, "discord gateway: close frame received");
                        return Ok(true);
                    }
                    Message::Ping(data) => {
                        write
                            .send(Message::Pong(data))
                            .await
                            .map_err(|e| anyhow::anyhow!("pong send failed: {e}"))?;
                    }
                    _ => {}
                }
            }

            _ = hb_ticker.tick() => {
                if !ack_received {
                    tracing::warn!(
                        "discord gateway: heartbeat not acknowledged (zombie connection), reconnecting"
                    );
                    return Ok(true);
                }
                ack_received = false;
                let d = state
                    .last_seq
                    .map(|s| serde_json::json!(s))
                    .unwrap_or(Value::Null);
                write
                    .send(Message::Text(
                        serde_json::json!({"op": OP_HEARTBEAT, "d": d}).to_string(),
                    ))
                    .await
                    .map_err(|e| anyhow::anyhow!("heartbeat send failed: {e}"))?;
            }
        }
    }
}

/// Read the next text frame from the stream, skipping non-text control frames.
async fn next_text_message<S>(read: &mut S) -> anyhow::Result<String>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match read.next().await {
            Some(Ok(Message::Text(t))) => return Ok(t),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
            None => anyhow::bail!("stream closed before receiving expected message"),
        }
    }
}

/// Handle a DISPATCH event (op=0).
async fn handle_dispatch(
    event_type: Option<&str>,
    data: &Value,
    session_id: &mut Option<String>,
    resume_url: &mut Option<String>,
    tx: &mpsc::Sender<IncomingMessage>,
    client: &Client,
) -> anyhow::Result<()> {
    match event_type {
        Some("READY") => {
            if let Some(sid) = data["session_id"].as_str() {
                *session_id = Some(sid.to_string());
            }
            if let Some(url) = data["resume_gateway_url"].as_str() {
                *resume_url = Some(url.to_string());
            }
            let username = data["user"]["username"].as_str().unwrap_or("unknown");
            tracing::info!(username, "discord gateway: ready");
        }
        Some("RESUMED") => {
            tracing::info!("discord gateway: session resumed");
        }
        Some("MESSAGE_CREATE") => {
            let msg_channel_id = data["channel_id"].as_str().unwrap_or("");

            // Skip messages from bots (avoid reacting to ourselves)
            if data["author"]["bot"].as_bool().unwrap_or(false) {
                return Ok(());
            }

            let id = data["id"].as_str().unwrap_or("").to_string();
            let author = data["author"]["username"]
                .as_str()
                .unwrap_or("unknown")
                .to_string();
            let body = data["content"].as_str().unwrap_or("").to_string();
            let timestamp = data["timestamp"]
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(Utc::now);

            let incoming = IncomingMessage {
                channel: "discord".to_string(),
                id,
                thread_id: msg_channel_id.to_string(),
                author,
                body,
                timestamp,
                metadata: serde_json::json!({}),
                topic_id: Some(msg_channel_id.to_string()),
            };

            tx.send(incoming)
                .await
                .map_err(|_| anyhow::anyhow!("receiver closed"))?;
        }
        Some("INTERACTION_CREATE") => {
            // Handle message component interactions (e.g. button clicks)
            let interaction_type = data["type"].as_u64().unwrap_or(0);
            if interaction_type == 3 {
                // MESSAGE_COMPONENT
                let interaction_id = data["id"].as_str().unwrap_or("").to_string();
                let interaction_token = data["token"].as_str().unwrap_or("").to_string();
                let custom_id = data["data"]["custom_id"].as_str().unwrap_or("").to_string();
                let inter_channel_id = data["channel_id"].as_str().unwrap_or("").to_string();
                let author = data["member"]["user"]["username"]
                    .as_str()
                    .or_else(|| data["user"]["username"].as_str())
                    .unwrap_or("unknown")
                    .to_string();

                // Acknowledge the interaction (type 6 = DEFERRED_UPDATE_MESSAGE)
                let callback_url = format!(
                    "https://discord.com/api/v10/interactions/{}/{}/callback",
                    interaction_id, interaction_token
                );
                let http_client = client.clone();
                let _ = http_client
                    .post(&callback_url)
                    .header("Content-Type", "application/json")
                    .json(&serde_json::json!({ "type": 6 }))
                    .send()
                    .await;

                let incoming = IncomingMessage {
                    channel: "discord".to_string(),
                    id: interaction_id.clone(),
                    thread_id: inter_channel_id.clone(),
                    author,
                    body: custom_id.clone(),
                    timestamp: Utc::now(),
                    metadata: serde_json::json!({
                        "interaction_id": interaction_id,
                        "interaction_token": interaction_token,
                        "custom_id": custom_id,
                    }),
                    topic_id: Some(inter_channel_id),
                };

                tx.send(incoming)
                    .await
                    .map_err(|_| anyhow::anyhow!("receiver closed"))?;
            }
        }
        Some(t) => {
            tracing::debug!(event_type = %t, "discord gateway: unhandled event");
        }
        None => {}
    }
    Ok(())
}

// ── Heartbeat interval parsing ───────────────────────────────────────────────

/// Parse heartbeat interval from Discord gateway Hello payload.
/// Rejects zero or invalid values and falls back to Discord's default (41.25 seconds).
fn parse_heartbeat_interval(value: &Value) -> u64 {
    value.as_u64().filter(|&v| v > 0).unwrap_or(41_250)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn handle_dispatch_message_create_sends_to_channel() {
        let (tx, mut rx) = mpsc::channel(10);
        let data = serde_json::json!({
            "id": "123456789",
            "channel_id": "987654321",
            "author": {"username": "testuser", "bot": false},
            "content": "hello world",
            "timestamp": "2024-01-01T00:00:00+00:00",
        });

        let mut session_id = None;
        let mut resume_url = None;
        let client = Client::new();

        handle_dispatch(
            Some("MESSAGE_CREATE"),
            &data,
            &mut session_id,
            &mut resume_url,
            &tx,
            &client,
        )
        .await
        .unwrap();

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.id, "123456789");
        assert_eq!(msg.thread_id, "987654321");
        assert_eq!(msg.topic_id, Some("987654321".to_string()));
        assert_eq!(msg.author, "testuser");
        assert_eq!(msg.body, "hello world");
        assert_eq!(msg.channel, "discord");
    }

    #[tokio::test]
    async fn handle_dispatch_skips_bot_messages() {
        let (tx, mut rx) = mpsc::channel(10);
        let data = serde_json::json!({
            "id": "111",
            "channel_id": "222",
            "author": {"username": "botuser", "bot": true},
            "content": "I am a bot",
            "timestamp": "2024-01-01T00:00:00+00:00",
        });

        let mut session_id = None;
        let mut resume_url = None;
        let client = Client::new();

        handle_dispatch(
            Some("MESSAGE_CREATE"),
            &data,
            &mut session_id,
            &mut resume_url,
            &tx,
            &client,
        )
        .await
        .unwrap();

        // Nothing should arrive
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_dispatch_accepts_any_channel() {
        let (tx, mut rx) = mpsc::channel(10);
        let data = serde_json::json!({
            "id": "999",
            "channel_id": "any-channel-id",
            "author": {"username": "user", "bot": false},
            "content": "hello from any channel",
            "timestamp": "2024-01-01T00:00:00+00:00",
        });

        let mut session_id = None;
        let mut resume_url = None;
        let client = Client::new();

        handle_dispatch(
            Some("MESSAGE_CREATE"),
            &data,
            &mut session_id,
            &mut resume_url,
            &tx,
            &client,
        )
        .await
        .unwrap();

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.body, "hello from any channel");
        assert_eq!(msg.thread_id, "any-channel-id");
        assert_eq!(msg.topic_id, Some("any-channel-id".to_string()));
    }

    #[tokio::test]
    async fn handle_dispatch_ready_sets_session_and_resume_url() {
        let (tx, _rx) = mpsc::channel(10);
        let data = serde_json::json!({
            "session_id": "abc123",
            "resume_gateway_url": "wss://us-east1.discord.gg",
            "user": {"username": "mybot"},
        });

        let mut session_id = None;
        let mut resume_url = None;
        let client = Client::new();

        handle_dispatch(
            Some("READY"),
            &data,
            &mut session_id,
            &mut resume_url,
            &tx,
            &client,
        )
        .await
        .unwrap();

        assert_eq!(session_id.as_deref(), Some("abc123"));
        assert_eq!(resume_url.as_deref(), Some("wss://us-east1.discord.gg"));
    }

    #[tokio::test]
    async fn handle_dispatch_accepts_messages_from_any_channel() {
        let (tx, mut rx) = mpsc::channel(10);
        let data = serde_json::json!({
            "id": "42",
            "channel_id": "random-channel",
            "author": {"username": "user", "bot": false},
            "content": "open message",
            "timestamp": "2024-01-01T00:00:00+00:00",
        });

        let mut session_id = None;
        let mut resume_url = None;
        let client = Client::new();

        handle_dispatch(
            Some("MESSAGE_CREATE"),
            &data,
            &mut session_id,
            &mut resume_url,
            &tx,
            &client,
        )
        .await
        .unwrap();

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.id, "42");
        assert_eq!(msg.thread_id, "random-channel");
        assert_eq!(msg.topic_id, Some("random-channel".to_string()));
    }

    #[tokio::test]
    async fn handle_dispatch_receiver_closed_returns_error() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx); // close receiver

        let data = serde_json::json!({
            "id": "1",
            "channel_id": "ch",
            "author": {"username": "u", "bot": false},
            "content": "msg",
            "timestamp": "2024-01-01T00:00:00+00:00",
        });

        let mut session_id = None;
        let mut resume_url = None;
        let client = Client::new();

        let result = handle_dispatch(
            Some("MESSAGE_CREATE"),
            &data,
            &mut session_id,
            &mut resume_url,
            &tx,
            &client,
        )
        .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("receiver closed"));
    }

    #[tokio::test]
    async fn handle_dispatch_interaction_create_button_click() {
        let (tx, mut rx) = mpsc::channel(10);
        let data = serde_json::json!({
            "type": 3, // MESSAGE_COMPONENT
            "id": "interaction-123",
            "token": "interaction-token-abc",
            "channel_id": "chan-456",
            "data": {
                "custom_id": "select_project:owner/repo"
            },
            "member": {
                "user": {"username": "clicker"}
            }
        });

        let mut session_id = None;
        let mut resume_url = None;
        let client = Client::new();

        handle_dispatch(
            Some("INTERACTION_CREATE"),
            &data,
            &mut session_id,
            &mut resume_url,
            &tx,
            &client,
        )
        .await
        .unwrap();

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.channel, "discord");
        assert_eq!(msg.id, "interaction-123");
        assert_eq!(msg.thread_id, "chan-456");
        assert_eq!(msg.topic_id, Some("chan-456".to_string()));
        assert_eq!(msg.body, "select_project:owner/repo");
        assert_eq!(msg.author, "clicker");
        assert_eq!(
            msg.metadata["custom_id"].as_str(),
            Some("select_project:owner/repo")
        );
        assert_eq!(
            msg.metadata["interaction_id"].as_str(),
            Some("interaction-123")
        );
        assert_eq!(
            msg.metadata["interaction_token"].as_str(),
            Some("interaction-token-abc")
        );
    }

    #[tokio::test]
    async fn handle_dispatch_interaction_create_ignores_non_component() {
        let (tx, mut rx) = mpsc::channel(10);
        let data = serde_json::json!({
            "type": 1, // PING, not MESSAGE_COMPONENT
            "id": "interaction-999",
            "token": "tok",
            "channel_id": "ch",
        });

        let mut session_id = None;
        let mut resume_url = None;
        let client = Client::new();

        handle_dispatch(
            Some("INTERACTION_CREATE"),
            &data,
            &mut session_id,
            &mut resume_url,
            &tx,
            &client,
        )
        .await
        .unwrap();

        // Should not produce a message for non-component interactions
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn parse_heartbeat_interval_rejects_zero() {
        let zero = serde_json::json!(0);
        assert_eq!(parse_heartbeat_interval(&zero), 41_250);
    }

    #[test]
    fn parse_heartbeat_interval_accepts_valid() {
        let valid = serde_json::json!(50_000);
        assert_eq!(parse_heartbeat_interval(&valid), 50_000);
    }

    #[test]
    fn parse_heartbeat_interval_defaults_on_null() {
        let null = serde_json::json!(null);
        assert_eq!(parse_heartbeat_interval(&null), 41_250);
    }

    #[test]
    fn parse_heartbeat_interval_defaults_on_missing() {
        let object = serde_json::json!({});
        assert_eq!(
            parse_heartbeat_interval(&object["heartbeat_interval"]),
            41_250
        );
    }

    #[test]
    fn parse_heartbeat_interval_can_create_valid_duration() {
        // Verify that the parsed interval can be used to create a tokio interval
        // without panicking. This is the real-world requirement: the value must
        // be valid for Duration::from_millis().
        let zero = serde_json::json!(0);
        let interval_ms = parse_heartbeat_interval(&zero);
        let duration = std::time::Duration::from_millis(interval_ms);
        assert!(duration.as_millis() > 0, "duration must be non-zero");
    }
}
