//! Agent execution, tools, and persistence for the CLI.

pub mod agent;
pub mod auth;
pub mod core;
pub mod lsp;
pub mod mcp;
pub mod providers;
pub mod tools;
pub mod workflow;

#[cfg(test)]
pub(crate) mod test_support;
