// Library target — exposes production modules to integration tests.
//
// These lints are suppressed because they fire on public types that are only
// public to satisfy the lib target requirement for integration tests —
// the binary doesn't expose these as a public API.
#![allow(clippy::new_without_default, clippy::should_implement_trait)]
//
//
// Integration tests in tests/ cannot access modules declared in main.rs.
// This lib target re-declares the same modules so that `use orch::...`
// works from tests/.
//
// The module source files are shared between the lib and bin targets;
// each target compiles them as part of its own crate root.

pub mod backends;
pub mod channels;
pub mod chat_session;
pub mod cli;
pub mod cmd;
pub mod cmd_cache;
pub mod config;
pub mod control;
pub mod cron;
pub mod engine;
pub mod github;
pub mod home;
pub mod parser;
pub mod repo_context;
pub mod security;
pub mod store;
pub mod template;
pub mod tmux;
pub mod webhook_status;
