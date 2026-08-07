//! Core provider-agnostic vocabulary. Moved to the `bob-types` leaf crate so
//! every workspace crate can share it without depending on the engine. This
//! module re-exports it so existing `crate::core::types::…` paths keep working.

pub use bob_types::*;
