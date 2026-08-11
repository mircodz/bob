// The core agent loop lives in `agent::agent`; the repeated name is intentional
// (the module groups agent-related submodules, `agent.rs` is the loop itself).
#[allow(clippy::module_inception)]
pub mod agent;
pub mod assembly;
pub mod compaction;
pub mod env;
pub mod hooks;
pub mod prompt;
pub mod team;
