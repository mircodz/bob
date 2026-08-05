//! bob-core: the provider-agnostic, UI-less coding-agent core. Any frontend
//! (CLI, TUI, web bridge) depends on this crate and drives it via the event bus.

pub mod agent;
pub mod auth;
pub mod core;
pub mod lsp;
pub mod mcp;
pub mod providers;
pub mod tools;
pub mod workflow;
