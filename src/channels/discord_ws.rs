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
        let channel_id = self.channel_id.clone();
        let shard_id = self.shard_id;
        let shard_count = self.shard_count;

        tracing::info!(shard_id, shard_count, "discord gateway starting");

        tokio::spawn(async move {
            run_gateway(token, channel_id, shard_id, shard_count, tx).await;
        });

        Ok(rx)
    }

    async fn send(&self, msg: &OutgoingMessage) -> anyhow::Result<()> {
        let channel_id = self
            .channel_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("discord channel_id not configured"))?;
        self.send_message(channel_id, &msg.body).await
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
    channel_id: Option<String>,
    shard_id: u64,
    shard_count: u64,
    tx: mpsc::Sender<IncomingMessage>,
) {
    let mut state = GatewayState::new();
    let mut backoff = Duration::from_secs(1);

    loop {
        let ws_url = state.ws_url();
        tracing::debug!(url = %ws_url, "connecting to discord gateway");

        match connect_async(&ws_url).await {
            Ok((ws, _)) => {
                backoff = Duration::from_secs(1); // reset on success

                let result = handle_connection(
                    ws,
                    &token,
                    channel_id.as_deref(),
                    shard_id,
                    shard_count,
                    &mut state,
                    &tx,
                )
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
    channel_id: Option<&str>,
    shard_id: u64,
    shard_count: u64,
    state: &mut GatewayState,
    tx: &mpsc::Sender<IncomingMessage>,
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

    let hb_interval_ms = hello.d["heartbeat_interval"].as_u64().unwrap_or(41_250);
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
                                    channel_id,
                                    &mut state.session_id,
                                    &mut state.resume_url,
                                    tx,
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
    channel_id: Option<&str>,
    session_id: &mut Option<String>,
    resume_url: &mut Option<String>,
    tx: &mpsc::Sender<IncomingMessage>,
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

            // Filter by configured channel_id when set
            if let Some(configured) = channel_id {
                if msg_channel_id != configured {
                    return Ok(());
                }
            }

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
            };

            tx.send(incoming)
                .await
                .map_err(|_| anyhow::anyhow!("receiver closed"))?;
        }
        Some(t) => {
            tracing::debug!(event_type = %t, "discord gateway: unhandled event");
        }
        None => {}
    }
    Ok(())
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

        handle_dispatch(
            Some("MESSAGE_CREATE"),
            &data,
            None,
            &mut session_id,
            &mut resume_url,
            &tx,
        )
        .await
        .unwrap();

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.id, "123456789");
        assert_eq!(msg.thread_id, "987654321");
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

        handle_dispatch(
            Some("MESSAGE_CREATE"),
            &data,
            None,
            &mut session_id,
            &mut resume_url,
            &tx,
        )
        .await
        .unwrap();

        // Nothing should arrive
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_dispatch_filters_by_channel_id() {
        let (tx, mut rx) = mpsc::channel(10);
        let data = serde_json::json!({
            "id": "111",
            "channel_id": "other-channel",
            "author": {"username": "user", "bot": false},
            "content": "wrong channel",
            "timestamp": "2024-01-01T00:00:00+00:00",
        });

        let mut session_id = None;
        let mut resume_url = None;

        handle_dispatch(
            Some("MESSAGE_CREATE"),
            &data,
            Some("configured-channel"), // only accept from this channel
            &mut session_id,
            &mut resume_url,
            &tx,
        )
        .await
        .unwrap();

        // Should be filtered out
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_dispatch_passes_matching_channel_id() {
        let (tx, mut rx) = mpsc::channel(10);
        let data = serde_json::json!({
            "id": "999",
            "channel_id": "configured-channel",
            "author": {"username": "user", "bot": false},
            "content": "hello",
            "timestamp": "2024-01-01T00:00:00+00:00",
        });

        let mut session_id = None;
        let mut resume_url = None;

        handle_dispatch(
            Some("MESSAGE_CREATE"),
            &data,
            Some("configured-channel"),
            &mut session_id,
            &mut resume_url,
            &tx,
        )
        .await
        .unwrap();

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.body, "hello");
        assert_eq!(msg.thread_id, "configured-channel");
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

        handle_dispatch(
            Some("READY"),
            &data,
            None,
            &mut session_id,
            &mut resume_url,
            &tx,
        )
        .await
        .unwrap();

        assert_eq!(session_id.as_deref(), Some("abc123"));
        assert_eq!(resume_url.as_deref(), Some("wss://us-east1.discord.gg"));
    }

    #[tokio::test]
    async fn handle_dispatch_no_channel_filter_accepts_any() {
        let (tx, mut rx) = mpsc::channel(10);
        let data = serde_json::json!({
            "id": "42",
            "channel_id": "any-channel",
            "author": {"username": "user", "bot": false},
            "content": "open message",
            "timestamp": "2024-01-01T00:00:00+00:00",
        });

        let mut session_id = None;
        let mut resume_url = None;

        handle_dispatch(
            Some("MESSAGE_CREATE"),
            &data,
            None,
            &mut session_id,
            &mut resume_url,
            &tx,
        )
        .await
        .unwrap();

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.id, "42");
        assert_eq!(msg.thread_id, "any-channel");
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

        let result = handle_dispatch(
            Some("MESSAGE_CREATE"),
            &data,
            None,
            &mut session_id,
            &mut resume_url,
            &tx,
        )
        .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("receiver closed"));
    }
}
