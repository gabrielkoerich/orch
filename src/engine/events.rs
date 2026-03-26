//! Task event bus — broadcast channel for status transitions.

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::handshake::server::Request;

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
            let decoded = urlencoding::decode(value).unwrap_or(std::borrow::Cow::Borrowed(value));
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
        if let Some(ref task_id) = self.task_id {
            if event.task_id != *task_id {
                return false;
            }
        }
        if let Some(ref repo) = self.repo {
            if event.repo != *repo {
                return false;
            }
        }
        true
    }
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
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn publish(&self, event: TaskEvent) -> usize {
        self.tx.send(event).unwrap_or(0)
    }

    /// Start the websocket server. Returns the bound port.
    /// Spawns a background task — does not block.
    pub async fn start_ws_server(&self) -> anyhow::Result<u16> {
        // Select a deterministic port in the ephemeral range (49152-65535),
        // starting from a hostname-based offset to be stable across restarts.
        let port = select_available_port()?;
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;

        // Write port file
        let state_dir = crate::home::state_dir()?;
        std::fs::write(state_dir.join("ws.port"), port.to_string())?;

        let tx = self.tx.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _addr)) => {
                        let mut rx = tx.subscribe();
                        tokio::spawn(async move {
                            let mut filter = EventFilter::default();
                            #[allow(clippy::result_large_err)]
                            let callback = |req: &Request, resp| {
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
                                                let msg = tokio_tungstenite::tungstenite::Message::Text(json);
                                                if write.send(msg).await.is_err() {
                                                    break;
                                                }
                                            }
                                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
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
        Ok(port)
    }
}

/// Find an available port in the ephemeral range (49152-65535).
/// Starts from a deterministic offset based on hostname hash, then increments.
/// This gives stable first-attempt ports across restarts on the same machine.
pub fn select_available_port() -> anyhow::Result<u16> {
    let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| "orch".to_string());
    let hash = hostname
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_add(b as u32));
    let range = 65535u32 - 49152;
    let start = 49152u16 + (hash % range) as u16;

    for offset in 0..1000u16 {
        let port = start.wrapping_add(offset);
        if port < 49152 {
            continue;
        }
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    anyhow::bail!("no available port found in range 49152-65535")
}

/// Remove the ws.port file on shutdown.
pub fn cleanup_port_file() {
    if let Ok(state_dir) = crate::home::state_dir() {
        let _ = std::fs::remove_file(state_dir.join("ws.port"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        bus.publish(event.clone());
        let received = rx.try_recv().unwrap();
        assert_eq!(received.task_id, "1");
        assert_eq!(received.new_status, "routed");
    }

    #[test]
    fn select_port_finds_available_port() {
        let port = select_available_port().unwrap();
        assert!(port >= 49152);
        // Verify the port is actually available by binding to it
        let listener = std::net::TcpListener::bind(("127.0.0.1", port));
        assert!(listener.is_ok());
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
        let port = bus.start_ws_server().await.unwrap();

        // Connect a websocket client
        let url = format!("ws://127.0.0.1:{}", port);
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (_write, mut read) = ws.split();

        // Give the server a moment to register the client
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Publish an event
        let mut event_with_agent = make_event("test-1", "owner/repo");
        event_with_agent.agent = Some("claude".to_string());
        bus.publish(event_with_agent);

        // Read from websocket
        let msg = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            StreamExt::next(&mut read),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();

        if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
            let received: TaskEvent = serde_json::from_str(&text).unwrap();
            assert_eq!(received.task_id, "test-1");
            assert_eq!(received.new_status, "routed");
            assert_eq!(received.agent, Some("claude".to_string()));
        } else {
            panic!("expected text message");
        }

        // Cleanup
        cleanup_port_file();
    }

    #[tokio::test]
    async fn ws_filter_by_task_id() {
        let bus = EventBus::new(256);
        let port = bus.start_ws_server().await.unwrap();

        // Connect filtered client — only task "42"
        let url = format!("ws://127.0.0.1:{port}/events?task_id=42");
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (_write, mut read) = ws.split();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Publish event for task 42 (should arrive)
        bus.publish(make_event("42", "owner/repo"));
        // Publish event for task 99 (should be filtered out)
        bus.publish(make_event("99", "owner/repo"));

        let msg = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            StreamExt::next(&mut read),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();

        if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
            let received: TaskEvent = serde_json::from_str(&text).unwrap();
            assert_eq!(received.task_id, "42");
        } else {
            panic!("expected text message");
        }

        // No more messages should arrive — timeout confirms the filter works
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            StreamExt::next(&mut read),
        )
        .await;
        assert!(result.is_err(), "expected no more messages (filtered)");

        cleanup_port_file();
    }

    #[tokio::test]
    async fn ws_filter_by_repo() {
        let bus = EventBus::new(256);
        let port = bus.start_ws_server().await.unwrap();

        // Connect filtered client — only repo containing "gabrielkoerich/orch"
        let url = format!(
            "ws://127.0.0.1:{port}/events?repo={}",
            urlencoding::encode("gabrielkoerich/orch")
        );
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (_write, mut read) = ws.split();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Matching event
        bus.publish(make_event("1", "gabrielkoerich/orch"));
        // Non-matching event
        bus.publish(make_event("2", "other/repo"));

        let msg = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            StreamExt::next(&mut read),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();

        if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
            let received: TaskEvent = serde_json::from_str(&text).unwrap();
            assert_eq!(received.task_id, "1");
            assert_eq!(received.repo, "gabrielkoerich/orch");
        } else {
            panic!("expected text message");
        }

        // No more messages — non-matching event was filtered server-side
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            StreamExt::next(&mut read),
        )
        .await;
        assert!(result.is_err(), "expected no more messages (filtered)");

        cleanup_port_file();
    }

    #[tokio::test]
    async fn ws_unfiltered_client_receives_all() {
        let bus = EventBus::new(256);
        let port = bus.start_ws_server().await.unwrap();

        // Unfiltered client
        let url = format!("ws://127.0.0.1:{port}/events");
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (_write, mut read) = ws.split();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        bus.publish(make_event("1", "a/b"));
        bus.publish(make_event("2", "c/d"));
        bus.publish(make_event("3", "e/f"));

        let mut received_ids = Vec::new();
        for _ in 0..3 {
            let msg = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                StreamExt::next(&mut read),
            )
            .await
            .unwrap()
            .unwrap()
            .unwrap();

            if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
                let event: TaskEvent = serde_json::from_str(&text).unwrap();
                received_ids.push(event.task_id);
            }
        }

        assert_eq!(received_ids, vec!["1", "2", "3"]);

        cleanup_port_file();
    }
}
