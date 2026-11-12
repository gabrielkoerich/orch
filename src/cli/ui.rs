//! `orch ui`: open the daemon's web UI in the browser

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use futures::StreamExt;
use tokio::sync::broadcast;
use tokio::{signal, time};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::cli;
use crate::cli::service;
use crate::engine::events::{TaskEvent, read_ws_credentials};
use crate::ui;

const RELAY_RETRY: Duration = Duration::from_secs(3);
const SERVICE_WAIT: Duration = Duration::from_secs(30);

pub async fn open(print: bool, dev: bool) -> anyhow::Result<()> {
    if dev {
        return serve_dev(print).await;
    }

    if let Some(endpoint) = ui::endpoint().await {
        return launch(&endpoint, print).await;
    }

    // A first run has no service yet, so start it and wait for it to write its address
    println!("Starting the orch service");
    service::start()?;
    let deadline = Instant::now() + SERVICE_WAIT;
    while Instant::now() < deadline {
        if let Some(endpoint) = ui::endpoint().await {
            return launch(&endpoint, print).await;
        }

        time::sleep(Duration::from_millis(500)).await;
    }

    anyhow::bail!("the orch service did not start its web UI. Check `orch log`")
}

/// A sign-in address with a one-time code, so the browser's history never holds the token
async fn sign_in_url(endpoint: &ui::Endpoint) -> anyhow::Result<String> {
    let url = format!("http://127.0.0.1:{}/", endpoint.port);
    let code: serde_json::Value = reqwest::Client::new()
        .post(format!("{url}login-code"))
        .bearer_auth(&endpoint.token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let code = code
        .get("code")
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow::anyhow!("the orch service sent no sign-in code"))?;
    Ok(format!("{url}?code={code}"))
}

async fn launch(endpoint: &ui::Endpoint, print: bool) -> anyhow::Result<()> {
    let url = format!("http://127.0.0.1:{}/", endpoint.port);
    let login = sign_in_url(endpoint).await?;
    if print {
        println!("{login}");
        return Ok(());
    }

    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let status = Command::new(opener).arg(&login).status()?;
    if !status.success() {
        anyhow::bail!("{opener} failed, run `orch ui --print` and open the address yourself");
    }

    println!("Opened {url}");
    Ok(())
}

/// The UI from this binary, on the running service's database, with its task events relayed in
async fn serve_dev(print: bool) -> anyhow::Result<()> {
    let store = Arc::new(cli::init_store().await?);
    let (tx, _) = broadcast::channel(256);
    tokio::spawn(relay_events(tx.clone()));
    let endpoint = ui::serve_on(store, tx).await?;
    launch(&endpoint, print).await?;
    println!("Serving the web UI from this build. Ctrl+C to stop");
    signal::ctrl_c().await?;
    Ok(())
}

async fn relay_events(tx: broadcast::Sender<TaskEvent>) {
    loop {
        if let Ok((port, token)) = read_ws_credentials() {
            let url = format!(
                "ws://127.0.0.1:{port}/events?token={}",
                urlencoding::encode(&token)
            );
            if let Ok((ws, _)) = connect_async(&url).await {
                let (_write, mut read) = ws.split();
                while let Some(Ok(msg)) = read.next().await {
                    if let Message::Text(text) = msg
                        && let Ok(event) = serde_json::from_str::<TaskEvent>(&text)
                    {
                        let _ = tx.send(event);
                    }
                }
            }
        }

        time::sleep(RELAY_RETRY).await;
    }
}
