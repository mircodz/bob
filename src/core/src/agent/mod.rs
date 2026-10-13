#[allow(clippy::module_inception)]
pub mod agent;
pub mod assembly;
pub mod compaction;
pub(crate) mod context;
pub mod env;
pub mod hooks;
pub(crate) mod lifecycle;
pub(crate) mod output;
pub(crate) mod profiles;
pub mod prompt;
#[cfg(test)]
mod prompt_eval;
pub mod team;
