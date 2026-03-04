//! Discord channel — REST helpers for sending messages.
//!
//! Incoming events are handled by `discord_ws::DiscordGateway` which
//! connects to the Discord Gateway websocket (wss://gateway.discord.gg) and
//! delivers MESSAGE_CREATE events in real-time.
//!
//! REST helpers (message send, reactions) live in `discord_ws.rs` so that
//! `DiscordGateway` can send messages without a separate REST client.
//! This module is kept as a registration point for the `discord` sub-module.
