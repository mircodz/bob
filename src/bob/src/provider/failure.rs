use bob_core::providers::provider::{is_context_overflow, is_transient_error};
use bob_kernel::ModelFailureKind;
use std::fmt;

#[derive(Debug)]
pub(super) struct AdapterFailure {
    pub kind: ModelFailureKind,
    pub message: String,
}

impl fmt::Display for AdapterFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AdapterFailure {}

pub(super) fn classify(error: &anyhow::Error) -> ModelFailureKind {
    if let Some(failure) = error.downcast_ref::<AdapterFailure>() {
        failure.kind
    } else if is_context_overflow(error) {
        ModelFailureKind::ContextOverflow
    } else if is_transient_error(&format!("{error:#}")) {
        ModelFailureKind::Transient
    } else {
        ModelFailureKind::Fatal
    }
}

pub(super) fn timeout(phase: &str) -> anyhow::Error {
    AdapterFailure {
        kind: ModelFailureKind::Transient,
        message: format!("provider {phase} deadline exceeded; no retry attempted"),
    }
    .into()
}
