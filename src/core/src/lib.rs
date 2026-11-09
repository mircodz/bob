//! Agent execution, tools, and persistence for the CLI.

pub mod agent;
pub mod auth;
pub mod core;
pub mod lsp;
pub mod mcp;
pub mod providers;
pub mod tools;
pub mod workflow;

/// Identifies bob to every provider, auth and MCP endpoint.
pub const USER_AGENT: &str = concat!("mircodz/bob/", env!("CARGO_PKG_VERSION"));

#[cfg(test)]
pub(crate) mod test_support;
