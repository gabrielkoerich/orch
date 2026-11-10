//! Task event bus — broadcast channel for status transitions.

use std::borrow::Cow;
use std::env;
use std::fs as std_fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::net;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process;

use anyhow::Context;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tokio::fs;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::ErrorResponse;
use tokio_tungstenite::tungstenite::handshake::server::Request;
use tokio_tungstenite::tungstenite::handshake::server::Response;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::http::header;

use crate::home;

/// Parsed query-parameter filters for a websocket connection.
#[derive(Clone, Debug, Default)]
pub struct EventFilter {
    pub task_id: Option<String>,
    pub repo: Option<String>,
}

impl EventFilter {
    /// Parse `?task_id=123&repo=owner/repo` from a request URI.
    fn from_request(request: &Request) -> Self {
        let uri = request.uri();
        let Some(query) = uri.query() else {
            return Self::default();
        };

        let mut task_id = None;
        let mut repo = None;

        for pair in query.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            let decoded = urlencoding::decode(value).unwrap_or(Cow::Borrowed(value));
            match key {
                "task_id" => task_id = Some(decoded.into_owned()),
                "repo" => repo = Some(decoded.into_owned()),
                _ => {}
            }
        }

        Self { task_id, repo }
    }

    /// Returns `true` if the event matches this filter (or no filter is set).
    fn matches(&self, event: &TaskEvent) -> bool {
        if let Some(ref task_id) = self.task_id
            && event.task_id != *task_id
        {
            return false;
        }
        if let Some(ref repo) = self.repo
            && event.repo != *repo
        {
            return false;
        }
        true
    }
}

fn query_value<'a>(request: &'a Request, key: &str) -> Option<Cow<'a, str>> {
    let query = request.uri().query()?;
    for pair in query.split('&') {
        let Some((k, value)) = pair.split_once('=') else {
            continue;
        };
        if k == key {
            return Some(urlencoding::decode(value).unwrap_or(Cow::Borrowed(value)));
        }
    }
    None
}

fn handshake_token(request: &Request) -> Cow<'_, str> {
    if let Some(token) = query_value(request, "token") {
        return token;
    }
    request
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
        .map_or(Cow::Borrowed(""), |v| Cow::Borrowed(v.trim()))
}

fn authorize_handshake(request: &Request, token: &str, host: &str) -> Result<(), StatusCode> {
    // Local clients send no Origin, a page in the browser always does
    if request.headers().get(header::ORIGIN).is_some() {
        return Err(StatusCode::FORBIDDEN);
    }
    // A rebound DNS name sends its own Host, so only the literal address passes
    let host_ok = request
        .headers()
        .get(header::HOST)
        .is_some_and(|h| h.as_bytes() == host.as_bytes());
    if !host_ok {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let given = handshake_token(request);
    let token_ok: bool = given.as_bytes().ct_eq(token.as_bytes()).into();
    if token_ok {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn handshake_error(status: StatusCode) -> ErrorResponse {
    let mut resp = ErrorResponse::new(None);
    *resp.status_mut() = status;
    resp
}

/// A control message sent by the server to the websocket client for out-of-band
/// notifications such as lag warnings.
///
/// The client should treat these as advisory: print a warning to the operator
/// and, when `kind == "lagged"`, resync task state from SQLite.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ControlMessage {
    /// Discriminator — currently only `"lagged"`.
    pub kind: String,
    /// Human-readable description.
    pub message: String,
    /// Number of events skipped (for `"lagged"`).
    pub missed: u64,
}

/// A task status transition event.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskEvent {
    pub task_id: String,
    pub repo: String,
    pub old_status: String,
    pub new_status: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub pr_number: Option<String>,
    pub branch: Option<String>,
    pub review_context: Option<String>,
    pub error: Option<String>,
    pub timestamp: String,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub duration_seconds: Option<f64>,
}

/// The event bus — wraps a tokio broadcast channel.
#[derive(Debug)]
pub struct EventBus {
    tx: broadcast::Sender<TaskEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Get a sender clone (for TaskManager, subscribers, etc.).
    pub fn sender(&self) -> broadcast::Sender<TaskEvent> {
        self.tx.clone()
    }

    /// Subscribe to events.
    pub fn subscribe(&self) -> broadcast::Receiver<TaskEvent> {
        self.tx.subscribe()
    }

    /// Publish an event. Returns number of receivers that got it.
    #[cfg(test)]
    pub fn publish(&self, event: TaskEvent) -> usize {
        self.tx.send(event).unwrap_or(0)
    }

    /// Start the websocket server. Returns the bound port and handshake token.
    /// Spawns a background task, does not block.
    pub async fn start_ws_server(&self) -> anyhow::Result<(u16, String)> {
        // Select a deterministic port in the ephemeral range (49152-65535),
        // starting from a hostname-based offset to be stable across restarts.
        let (listener, port) = select_available_listener()?;
        listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(listener)?;

        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let host = format!("127.0.0.1:{port}");
        write_private(&ws_port_path()?, &format!("{port} {token}"))?;

        // Write service version file so service status can detect CLI/service drift.
        // Format: "{pid}\t{version}" so readers can verify the owning process is alive.
        if let Ok(path) = home::service_version_path() {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent).await;
            }
            let pid = process::id();
            let content = format!("{}\t{}", pid, env!("ORCH_VERSION"));
            let _ = fs::write(path, content).await;
        }

        let tx = self.tx.clone();
        let serve_token = token.clone();
        let serve_host = host;
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _addr)) => {
                        let mut rx = tx.subscribe();
                        let token = serve_token.clone();
                        let host = serve_host.clone();
                        tokio::spawn(async move {
                            let mut filter = EventFilter::default();
                            #[expect(
                                clippy::result_large_err,
                                reason = "the tungstenite callback type fixes the error type"
                            )]
                            let callback = |req: &Request,
                                            mut resp: Response|
                             -> Result<Response, ErrorResponse> {
                                if let Err(status) = authorize_handshake(req, &token, &host) {
                                    return Err(handshake_error(status));
                                }
                                if let Some(proto) =
                                    req.headers().get(header::SEC_WEBSOCKET_PROTOCOL)
                                {
                                    let proto_ok: bool =
                                        proto.as_bytes().ct_eq(token.as_bytes()).into();
                                    if proto_ok {
                                        resp.headers_mut()
                                            .insert(header::SEC_WEBSOCKET_PROTOCOL, proto.clone());
                                    }
                                }
                                filter = EventFilter::from_request(req);
                                tracing::debug!(?filter, "ws client connected with filter");
                                Ok(resp)
                            };
                            let Ok(ws) =
                                tokio_tungstenite::accept_hdr_async(stream, callback).await
                            else {
                                return;
                            };
                            let (mut write, mut read) = ws.split();
                            loop {
                                tokio::select! {
                                    event = rx.recv() => {
                                        match event {
                                            Ok(e) => {
                                                if !filter.matches(&e) {
                                                    continue;
                                                }
                                                let json = serde_json::to_string(&e)
                                                    .unwrap_or_default();
                                                let msg = Message::Text(json);
                                                if write.send(msg).await.is_err() {
                                                    break;
                                                }
                                            }
                                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                                tracing::warn!(
                                                    missed = n,
                                                    "event bus ws client lagged — {n} events dropped"
                                                );
                                                let ctrl = ControlMessage {
                                                    kind: "lagged".to_string(),
                                                    message: format!(
                                                        "event bus lagged: {n} events were dropped — resync from store"
                                                    ),
                                                    missed: n,
                                                };
                                                if let Ok(json) = serde_json::to_string(&ctrl) {
                                                    let msg = Message::Text(json);
                                                    if write.send(msg).await.is_err() {
                                                        break;
                                                    }
                                                }
                                            }
                                            Err(_) => break,
                                        }
                                    }
                                    msg = read.next() => {
                                        // Client disconnected or sent close
                                        if msg.is_none() {
                                            break;
                                        }
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(?e, "ws accept failed");
                    }
                }
            }
        });

        tracing::info!(port, "event bus websocket server started on 127.0.0.1");
        Ok((port, token))
    }
}

/// Find an available port in the ephemeral range (49152-65535).
/// Starts from a deterministic offset based on hostname hash, then increments.
/// This gives stable first-attempt ports across restarts on the same machine.
pub fn select_available_listener() -> anyhow::Result<(net::TcpListener, u16)> {
    let hostname = env::var("HOSTNAME").unwrap_or_else(|_| "orch".to_string());
    let hash = hostname
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_add(u32::from(b)));
    let range = 65535u32 - 49152;
    let start = 49152u16 + u16::try_from(hash % range).unwrap_or(0);

    for offset in 0..1000u16 {
        let port = start.wrapping_add(offset);
        if port < 49152 {
            continue;
        }
        if let Ok(listener) = net::TcpListener::bind(("127.0.0.1", port)) {
            return Ok((listener, port));
        }
    }
    anyhow::bail!("no available port found in range 49152-65535")
}

/// Remove the ws.port file on shutdown.
pub async fn cleanup_port_file() {
    if let Ok(path) = ws_port_path() {
        let _ = fs::remove_file(path).await;
    }
}

/// Remove the service.version file on shutdown — only if this process owns it.
pub fn cleanup_version_file() {
    if let Ok(path) = home::service_version_path() {
        let owned = std_fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.split('\t').next()?.parse::<u32>().ok())
            .is_none_or(|pid| pid == process::id()); // legacy plain-version files: assume ownership
        if owned {
            let _ = std_fs::remove_file(path);
        }
    }
}

fn ws_port_path() -> anyhow::Result<PathBuf> {
    if cfg!(test) {
        Ok(env::temp_dir().join("orch-test-state").join("ws.port"))
    } else {
        Ok(home::state_dir()?.join("ws.port"))
    }
}

/// Owner-only file, written to a temp name and renamed so a reader never sees half of it
fn write_private(path: &Path, body: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std_fs::create_dir_all(parent)?;
    }

    let tmp = path.with_extension(format!("tmp.{}", process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(body.as_bytes())?;
    std_fs::rename(&tmp, path)?;
    Ok(())
}

fn parse_ws_credentials(body: &str) -> anyhow::Result<(u16, String)> {
    let mut parts = body.split_whitespace();
    let port = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty ws.port"))?
        .parse()
        .context("invalid port number in ws.port")?;
    let token = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("ws.port missing token"))?
        .to_string();
    Ok((port, token))
}

/// Port and token written at listen time, so the CLI can prove it can read `~/.orch`
pub fn read_ws_credentials() -> anyhow::Result<(u16, String)> {
    let body = std_fs::read_to_string(ws_port_path()?)
        .context("event bus not running — is the service started? (ws.port not found)")?;
    parse_ws_credentials(&body)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use tokio::time;
    use tokio_tungstenite::tungstenite::Error;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    use super::*;

    #[test]
    fn control_message_serializes_to_json() {
        let ctrl = ControlMessage {
            kind: "lagged".to_string(),
            message: "event bus lagged: 5 events were dropped".to_string(),
            missed: 5,
        };
        let json = serde_json::to_string(&ctrl).unwrap();
        assert!(json.contains("\"kind\":\"lagged\""));
        assert!(json.contains("\"missed\":5"));
    }

    #[test]
    fn control_message_round_trips() {
        let ctrl = ControlMessage {
            kind: "lagged".to_string(),
            message: "test".to_string(),
            missed: 42,
        };
        let json = serde_json::to_string(&ctrl).unwrap();
        let decoded: ControlMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.kind, "lagged");
        assert_eq!(decoded.missed, 42);
    }

    #[test]
    fn task_event_serializes_to_json() {
        let event = TaskEvent {
            task_id: "123".to_string(),
            repo: "owner/repo".to_string(),
            old_status: "new".to_string(),
            new_status: "routed".to_string(),
            agent: Some("claude".to_string()),
            model: None,
            pr_number: None,
            branch: None,
            review_context: None,
            error: None,
            timestamp: "2026-03-23T12:00:00Z".to_string(),
            title: None,
            summary: None,
            duration_seconds: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"task_id\":\"123\""));
        assert!(json.contains("\"new_status\":\"routed\""));
    }

    #[test]
    fn event_bus_send_receive() {
        let bus = EventBus::new(256);
        let mut rx = bus.subscribe();
        let event = TaskEvent {
            task_id: "1".to_string(),
            repo: "r".to_string(),
            old_status: "new".to_string(),
            new_status: "routed".to_string(),
            agent: None,
            model: None,
            pr_number: None,
            branch: None,
            review_context: None,
            error: None,
            timestamp: "2026-03-23T12:00:00Z".to_string(),
            title: None,
            summary: None,
            duration_seconds: None,
        };
        bus.publish(event);
        let received = rx.try_recv().unwrap();
        assert_eq!(received.task_id, "1");
        assert_eq!(received.new_status, "routed");
    }

    #[test]
    fn select_port_finds_available_port() {
        let (listener, port) = select_available_listener().unwrap();
        assert!(port >= 49152);
        // Verify the port is reserved while the listener is alive.
        let second_bind = net::TcpListener::bind(("127.0.0.1", port));
        assert!(second_bind.is_err());
        drop(listener);
    }

    fn make_event(task_id: &str, repo: &str) -> TaskEvent {
        TaskEvent {
            task_id: task_id.to_string(),
            repo: repo.to_string(),
            old_status: "new".to_string(),
            new_status: "routed".to_string(),
            agent: None,
            model: None,
            pr_number: None,
            branch: None,
            review_context: None,
            error: None,
            timestamp: "2026-03-23T12:00:00Z".to_string(),
            title: None,
            summary: None,
            duration_seconds: None,
        }
    }

    fn authed_url(port: u16, token: &str, extra: &str) -> String {
        let mut url = format!("ws://127.0.0.1:{port}/events?token={token}");
        if !extra.is_empty() {
            url.push('&');
            url.push_str(extra);
        }
        url
    }

    async fn handshake_status(req: impl IntoClientRequest + Unpin) -> u16 {
        match tokio_tungstenite::connect_async(req).await {
            Ok(_) => 101,
            Err(Error::Http(resp)) => resp.status().as_u16(),
            Err(e) => panic!("expected HTTP handshake status, got {e}"),
        }
    }

    #[test]
    fn event_filter_empty_matches_all() {
        let filter = EventFilter::default();
        assert!(filter.matches(&make_event("1", "a/b")));
        assert!(filter.matches(&make_event("999", "x/y")));
    }

    #[test]
    fn event_filter_task_id_filters() {
        let filter = EventFilter {
            task_id: Some("42".to_string()),
            repo: None,
        };
        assert!(filter.matches(&make_event("42", "a/b")));
        assert!(!filter.matches(&make_event("43", "a/b")));
    }

    #[test]
    fn event_filter_repo_filters() {
        let filter = EventFilter {
            task_id: None,
            repo: Some("owner/repo".to_string()),
        };
        assert!(filter.matches(&make_event("1", "owner/repo")));
        assert!(!filter.matches(&make_event("1", "other/repo")));
    }

    #[test]
    fn event_filter_repo_exact_match() {
        let filter = EventFilter {
            task_id: None,
            repo: Some("owner".to_string()),
        };
        assert!(filter.matches(&make_event("1", "owner")));
        assert!(!filter.matches(&make_event("1", "owner/repo")));
        assert!(!filter.matches(&make_event("1", "someone/else")));
    }

    #[test]
    fn event_filter_combined() {
        let filter = EventFilter {
            task_id: Some("5".to_string()),
            repo: Some("a/b".to_string()),
        };
        assert!(filter.matches(&make_event("5", "a/b")));
        assert!(!filter.matches(&make_event("6", "a/b")));
        assert!(!filter.matches(&make_event("5", "c/d")));
    }

    #[tokio::test]
    async fn ws_server_broadcasts_events() {
        let bus = EventBus::new(256);
        let (port, token) = bus.start_ws_server().await.unwrap();

        // Connect a websocket client
        let url = authed_url(port, &token, "");
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (_write, mut read) = ws.split();

        // Give the server a moment to register the client
        time::sleep(Duration::from_millis(50)).await;

        // Publish an event
        let mut event_with_agent = make_event("test-1", "owner/repo");
        event_with_agent.agent = Some("claude".to_string());
        bus.publish(event_with_agent);

        // Read from websocket
        let msg = time::timeout(Duration::from_secs(2), StreamExt::next(&mut read))
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        if let Message::Text(text) = msg {
            let received: TaskEvent = serde_json::from_str(&text).unwrap();
            assert_eq!(received.task_id, "test-1");
            assert_eq!(received.new_status, "routed");
            assert_eq!(received.agent, Some("claude".to_string()));
        } else {
            panic!("expected text message");
        }

        // Cleanup
        cleanup_port_file().await;
    }

    #[tokio::test]
    async fn ws_filter_by_task_id() {
        let bus = EventBus::new(256);
        let (port, token) = bus.start_ws_server().await.unwrap();

        // Connect filtered client, only task "42"
        let url = authed_url(port, &token, "task_id=42");
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (_write, mut read) = ws.split();

        time::sleep(Duration::from_millis(50)).await;

        // Publish event for task 42 (should arrive)
        bus.publish(make_event("42", "owner/repo"));
        // Publish event for task 99 (should be filtered out)
        bus.publish(make_event("99", "owner/repo"));

        let msg = time::timeout(Duration::from_secs(2), StreamExt::next(&mut read))
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        if let Message::Text(text) = msg {
            let received: TaskEvent = serde_json::from_str(&text).unwrap();
            assert_eq!(received.task_id, "42");
        } else {
            panic!("expected text message");
        }

        // No more messages should arrive — timeout confirms the filter works
        let result = time::timeout(Duration::from_millis(300), StreamExt::next(&mut read)).await;
        assert!(result.is_err(), "expected no more messages (filtered)");

        cleanup_port_file().await;
    }

    #[tokio::test]
    async fn ws_filter_by_repo() {
        let bus = EventBus::new(256);
        let (port, token) = bus.start_ws_server().await.unwrap();

        let url = authed_url(
            port,
            &token,
            &format!("repo={}", urlencoding::encode("gabrielkoerich/orch")),
        );
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (_write, mut read) = ws.split();

        time::sleep(Duration::from_millis(50)).await;

        // Matching event
        bus.publish(make_event("1", "gabrielkoerich/orch"));
        // Non-matching event
        bus.publish(make_event("2", "other/repo"));

        let msg = time::timeout(Duration::from_secs(2), StreamExt::next(&mut read))
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        if let Message::Text(text) = msg {
            let received: TaskEvent = serde_json::from_str(&text).unwrap();
            assert_eq!(received.task_id, "1");
            assert_eq!(received.repo, "gabrielkoerich/orch");
        } else {
            panic!("expected text message");
        }

        // No more messages — non-matching event was filtered server-side
        let result = time::timeout(Duration::from_millis(300), StreamExt::next(&mut read)).await;
        assert!(result.is_err(), "expected no more messages (filtered)");

        cleanup_port_file().await;
    }

    #[tokio::test]
    async fn ws_unfiltered_client_receives_all() {
        let bus = EventBus::new(256);
        let (port, token) = bus.start_ws_server().await.unwrap();

        let url = authed_url(port, &token, "");
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (_write, mut read) = ws.split();

        time::sleep(Duration::from_millis(50)).await;

        bus.publish(make_event("1", "a/b"));
        bus.publish(make_event("2", "c/d"));
        bus.publish(make_event("3", "e/f"));

        let mut received_ids = Vec::new();
        for _ in 0..3 {
            let msg = time::timeout(Duration::from_secs(2), StreamExt::next(&mut read))
                .await
                .unwrap()
                .unwrap()
                .unwrap();

            if let Message::Text(text) = msg {
                let event: TaskEvent = serde_json::from_str(&text).unwrap();
                received_ids.push(event.task_id);
            }
        }

        assert_eq!(received_ids, vec!["1", "2", "3"]);

        cleanup_port_file().await;
    }

    /// When the broadcast receiver lags (missed events), the server must send a
    /// ControlMessage{kind:"lagged"} to the ws client instead of silently skipping.
    #[tokio::test]
    async fn ws_server_sends_lag_control_message() {
        // Tiny capacity (1) so we can force lag by publishing faster than the
        // client consumes.
        let bus = EventBus::new(1);
        let Ok((port, token)) = bus.start_ws_server().await else {
            // Port unavailable (race with parallel tests), skip gracefully
            return;
        };

        // Connect client, but don't read yet, let the buffer overflow
        let url = authed_url(port, &token, "");
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (_write, mut read) = ws.split();

        // Give the server a moment to register the client.
        time::sleep(Duration::from_millis(50)).await;

        // Flood the bus — many more events than the channel capacity (1).
        // This guarantees the server-side rx will lag before the client reads.
        for i in 0..20 {
            bus.publish(make_event(&i.to_string(), "owner/repo"));
        }

        // Drain messages until we find a ControlMessage with kind=="lagged".
        let mut got_lag = false;
        for _ in 0..30 {
            let result = time::timeout(Duration::from_secs(2), StreamExt::next(&mut read)).await;

            let Ok(Some(Ok(msg))) = result else { break };

            if let Message::Text(text) = msg
                && let Ok(ctrl) = serde_json::from_str::<ControlMessage>(&text)
                && ctrl.kind == "lagged"
            {
                got_lag = true;
                assert!(ctrl.missed > 0, "missed count must be > 0");
                break;
            }
        }

        assert!(
            got_lag,
            "expected a lagged ControlMessage when bus capacity is exceeded"
        );

        cleanup_port_file().await;
    }

    #[test]
    fn parse_ws_credentials_reads_port_and_token() {
        let (port, token) = parse_ws_credentials("51234 abcdef\n").unwrap();
        assert_eq!(port, 51234);
        assert_eq!(token, "abcdef");
    }

    #[test]
    fn parse_ws_credentials_rejects_port_only() {
        assert!(
            parse_ws_credentials("51234\n").is_err(),
            "a port file without a token is refused"
        );
    }

    #[test]
    fn write_private_creates_owner_only_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ws.port");
        write_private(&path, "9 tok").unwrap();
        let mode = std_fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "ws.port must be owner-only");
    }

    #[tokio::test]
    async fn ws_port_file_is_owner_only() {
        let bus = EventBus::new(16);
        let (port, token) = bus.start_ws_server().await.unwrap();
        let path = ws_port_path().unwrap();
        let body = std_fs::read_to_string(&path).unwrap_or_default();
        if body.contains(&token) {
            let mode = std_fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "ws.port must be owner-only");
            assert!(
                body.starts_with(&format!("{port} ")),
                "port is first in the file, got {body:?}"
            );
        }
        cleanup_port_file().await;
    }

    #[tokio::test]
    async fn ws_handshake_rejects_a_missing_token() {
        let bus = EventBus::new(16);
        let (port, _token) = bus.start_ws_server().await.unwrap();
        let url = format!("ws://127.0.0.1:{port}/events");
        assert_eq!(
            handshake_status(&url).await,
            401,
            "a handshake with no token is unauthorized"
        );
        cleanup_port_file().await;
    }

    #[tokio::test]
    async fn ws_handshake_rejects_a_wrong_token() {
        let bus = EventBus::new(16);
        let (port, _token) = bus.start_ws_server().await.unwrap();
        let url = format!("ws://127.0.0.1:{port}/events?token=nope");
        assert_eq!(
            handshake_status(&url).await,
            401,
            "a handshake with a wrong token is unauthorized"
        );
        cleanup_port_file().await;
    }

    #[tokio::test]
    async fn ws_handshake_rejects_another_host() {
        let bus = EventBus::new(16);
        let (port, token) = bus.start_ws_server().await.unwrap();
        let url = format!("ws://localhost:{port}/events?token={token}");
        assert_eq!(
            handshake_status(&url).await,
            401,
            "a Host other than 127.0.0.1 is refused"
        );
        cleanup_port_file().await;
    }

    #[tokio::test]
    async fn ws_handshake_rejects_any_origin() {
        let bus = EventBus::new(16);
        let (port, token) = bus.start_ws_server().await.unwrap();
        let mut req = authed_url(port, &token, "").into_client_request().unwrap();
        req.headers_mut()
            .insert("origin", "http://evil.test".parse().unwrap());
        assert_eq!(
            handshake_status(req).await,
            403,
            "a handshake with an Origin header is forbidden"
        );
        cleanup_port_file().await;
    }

    #[tokio::test]
    async fn ws_handshake_accepts_sec_websocket_protocol() {
        let bus = EventBus::new(16);
        let (port, token) = bus.start_ws_server().await.unwrap();
        let mut req = format!("ws://127.0.0.1:{port}/events")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("sec-websocket-protocol", token.parse().unwrap());
        assert_eq!(
            handshake_status(req).await,
            101,
            "the token may be sent as Sec-WebSocket-Protocol"
        );
        cleanup_port_file().await;
    }
}
