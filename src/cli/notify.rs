//! `orch notify` — send a Telegram message from the command line.
//!
//! Allows agents (and humans) to send Telegram messages directly by running:
//!
//! ```bash
//! orch notify "Job completed successfully"
//! orch notify --target 602365815 "Job failed: backup.sh exited with code 1"
//! ```
//!
//! The `--target` flag overrides the `channels.telegram.chat_id` from config.
//! If both are absent, the command returns an error.

use crate::channels::telegram::TelegramChannel;
use crate::channels::{Channel, OutgoingMessage};

/// Send a Telegram notification message.
///
/// # Arguments
///
/// * `message` — the text to send
/// * `target`  — optional chat_id override; falls back to `channels.telegram.chat_id`
pub async fn send(message: &str, target: Option<&str>) -> anyhow::Result<()> {
    // Resolve bot token
    let token = crate::config::get("channels.telegram.bot_token").map_err(|_| {
        anyhow::anyhow!(
            "Telegram bot token not configured.\n\
             Set `channels.telegram.bot_token` in ~/.orch/config.yml"
        )
    })?;

    // Resolve chat_id: --target arg → config default
    let chat_id: Option<String> = target
        .map(|s| s.to_string())
        .or_else(|| crate::config::get("channels.telegram.chat_id").ok());

    if chat_id.is_none() {
        anyhow::bail!(
            "No Telegram chat_id configured.\n\
             Use --target <chat_id> or set `channels.telegram.chat_id` in ~/.orch/config.yml"
        );
    }

    // Build the channel with the resolved chat_id
    let channel = TelegramChannel::new(token, chat_id.clone())?;

    // Build the outgoing message
    let msg = OutgoingMessage {
        thread_id: chat_id.unwrap_or_default(),
        body: message.to_string(),
        reply_to: None,
        metadata: serde_json::Value::Null,
        topic_id: None,
    };

    channel.send(&msg).await?;

    println!("Telegram message sent.");
    Ok(())
}
