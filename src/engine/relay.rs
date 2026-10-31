//! Client for orch-relay, which receives the GitHub App's webhooks on Cloudflare. orch connects
//! out, so it needs no endpoint. The contract is PROTOCOL.md in the orch-relay repository

use crate::store::TaskStore;
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

const CURSOR_KEY: &str = "relay_cursor";
const PING_EVERY: Duration = Duration::from_secs(300);
const MAX_BACKOFF_SECS: u64 = 300;
// A shorter session keeps the backoff growing, so a relay that closes each socket at once is not
// hit every second, every request counts toward its daily limit
const STABLE_SESSION: Duration = Duration::from_secs(60);

#[derive(serde::Deserialize)]
struct Incoming {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    seq: i64,
    #[serde(default)]
    event: String,
    #[serde(default)]
    action: String,
    #[serde(default)]
    repo: String,
    #[serde(default)]
    payload: Option<serde_json::Value>,
}

/// Keeps one stream open for the life of the daemon, reconnecting with backoff
pub async fn run(url: String, token: String, store: Arc<TaskStore>, notify: Arc<Notify>) {
    let mut backoff = 1;

    loop {
        let started = Instant::now();
        match session(&url, &token, &store, &notify).await {
            Ok(()) => tracing::warn!("relay stream closed, reconnecting"),
            Err(e) => tracing::warn!(err = %e, "relay stream failed, reconnecting"),
        }

        let wait = retry_after(backoff, started.elapsed());
        tokio::time::sleep(Duration::from_secs(wait)).await;
        backoff = (wait * 2).min(MAX_BACKOFF_SECS);
    }
}

/// Seconds to wait before the next connect
fn retry_after(backoff: u64, lasted: Duration) -> u64 {
    if lasted >= STABLE_SESSION {
        1
    } else {
        backoff
    }
}

async fn session(url: &str, token: &str, store: &TaskStore, notify: &Notify) -> anyhow::Result<()> {
    let ws_url = format!("{}/ws", url.trim_end_matches('/'))
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    let mut request = ws_url.into_client_request()?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse()?);
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await?;

    let after = cursor(store).await;
    let hello = serde_json::json!({ "v": 1, "type": "hello", "after": after });
    ws.send(Message::Text(hello.to_string())).await?;
    tracing::info!(after, "relay stream connected");

    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;

    loop {
        tokio::select! {
            _ = ping.tick() => ws.send(Message::Text("ping".to_string())).await?,
            msg = ws.next() => match msg {
                None => return Ok(()),
                Some(msg) => match msg? {
                    Message::Text(text) => handle(&text, store, notify).await,
                    Message::Close(_) => return Ok(()),
                    _ => {}
                },
            },
        }
    }
}

async fn cursor(store: &TaskStore) -> i64 {
    store
        .kv_get(CURSOR_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// An event is a hint to re-read GitHub, never a command, so orch acts on what the sync reads
pub(crate) async fn handle(text: &str, store: &TaskStore, notify: &Notify) {
    if text == "pong" {
        return;
    }

    let msg: Incoming = match serde_json::from_str(text) {
        Ok(msg) => msg,
        Err(e) => {
            tracing::warn!(err = %e, "unreadable relay message");
            return;
        }
    };

    match msg.kind.as_str() {
        "gap" => {
            tracing::warn!("relay deleted events this client never read, syncing every project");
            notify.notify_one();
        }
        "event" => {
            // An orch PR closes its issue, so `issues` `closed` also covers a merged PR
            if msg.event == "issues" && msg.action == "closed" {
                let number = msg
                    .payload
                    .as_ref()
                    .and_then(|p| p.pointer("/issue/number"))
                    .and_then(serde_json::Value::as_i64);

                if let Some(number) = number {
                    let key = format!("dep_blocker_read_at:{}#{number}", msg.repo);

                    if let Err(e) = store.kv_delete(&key).await {
                        tracing::warn!(err = %e, key, "failed to release a closed blocker");
                    }
                }
            }

            if let Err(e) = store.kv_set(CURSOR_KEY, &msg.seq.to_string()).await {
                tracing::warn!(err = %e, seq = msg.seq, "failed to store the relay cursor");
            }

            notify.notify_one();
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn woke(notify: &Notify) -> bool {
        tokio::time::timeout(Duration::from_millis(200), notify.notified())
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn an_event_stores_the_cursor_and_wakes_the_sync() {
        let store = TaskStore::open_memory().await.unwrap();
        let notify = Notify::new();
        handle(
            r#"{"type":"event","seq":7,"event":"pull_request","action":"opened","repo":"o/r","payload":{}}"#,
            &store,
            &notify,
        )
        .await;

        assert_eq!(cursor(&store).await, 7);
        assert!(woke(&notify).await);
    }

    #[tokio::test]
    async fn a_closed_issue_releases_its_blocker_cache_entry() {
        let store = TaskStore::open_memory().await.unwrap();
        store
            .kv_set("dep_blocker_read_at:o/r#12", "1")
            .await
            .unwrap();
        store
            .kv_set("dep_blocker_read_at:o/r#13", "1")
            .await
            .unwrap();
        handle(
            r#"{"type":"event","seq":1,"event":"issues","action":"closed","repo":"o/r","payload":{"issue":{"number":12}}}"#,
            &store,
            &Notify::new(),
        )
        .await;

        assert_eq!(
            store.kv_get("dep_blocker_read_at:o/r#12").await.unwrap(),
            None
        );
        assert!(store
            .kv_get("dep_blocker_read_at:o/r#13")
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn a_gap_wakes_the_sync_and_keeps_the_cursor() {
        let store = TaskStore::open_memory().await.unwrap();
        store.kv_set(CURSOR_KEY, "3").await.unwrap();
        let notify = Notify::new();
        handle(r#"{"type":"gap","oldest":50}"#, &store, &notify).await;

        assert_eq!(cursor(&store).await, 3);
        assert!(woke(&notify).await);
    }

    #[tokio::test]
    async fn pong_and_unknown_messages_change_nothing() {
        let store = TaskStore::open_memory().await.unwrap();
        let notify = Notify::new();
        handle("pong", &store, &notify).await;
        handle("not json", &store, &notify).await;

        assert_eq!(cursor(&store).await, 0);
        assert!(!woke(&notify).await);
    }

    // The handshake callback's error type is tungstenite's, too large for the lint
    #[allow(clippy::result_large_err)]
    #[tokio::test]
    async fn a_session_says_hello_with_the_cursor_and_applies_events() {
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut auth = String::new();
            let mut ws =
                tokio_tungstenite::accept_hdr_async(tcp, |req: &Request, res: Response| {
                    auth = req
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    Ok(res)
                })
                .await
                .unwrap();
            let hello = ws.next().await.unwrap().unwrap().into_text().unwrap();
            let event = r#"{"type":"event","seq":9,"event":"issues","action":"opened","repo":"o/r","payload":{}}"#;
            ws.send(Message::Text(event.to_string())).await.unwrap();
            ws.close(None).await.unwrap();
            (auth, hello)
        });

        let store = TaskStore::open_memory().await.unwrap();
        store.kv_set(CURSOR_KEY, "4").await.unwrap();
        let notify = Notify::new();
        session(&format!("http://{addr}"), "tok", &store, &notify)
            .await
            .unwrap();

        let (auth, hello) = server.await.unwrap();
        assert_eq!(auth, "Bearer tok");
        let hello: serde_json::Value = serde_json::from_str(&hello).unwrap();
        assert_eq!(
            hello,
            serde_json::json!({ "v": 1, "type": "hello", "after": 4 })
        );
        assert_eq!(cursor(&store).await, 9);
        assert!(woke(&notify).await);
    }

    #[test]
    fn a_socket_closed_at_once_keeps_the_backoff_growing() {
        let quick = Duration::from_secs(1);
        let mut backoff = 1;
        let mut waits = Vec::new();

        for _ in 0..12 {
            let wait = retry_after(backoff, quick);
            waits.push(wait);
            backoff = (wait * 2).min(MAX_BACKOFF_SECS);
        }

        assert_eq!(waits[..9], [1, 2, 4, 8, 16, 32, 64, 128, 256]);
        assert_eq!(waits[9..], [300, 300, 300]);
        assert_eq!(retry_after(300, STABLE_SESSION), 1);
    }
}
