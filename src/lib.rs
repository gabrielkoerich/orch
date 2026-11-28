// Library target — exposes production modules to integration tests.
//
#![expect(
    clippy::new_without_default,
    clippy::should_implement_trait,
    reason = "these types are public only so integration tests can reach them"
)]
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
pub mod cli;
pub mod config;
pub mod cooldown;
pub mod engine;
pub mod github;
pub mod plan;
pub mod security;
pub mod store;
pub mod sys;
pub mod ui;
pub mod webhook_status;
