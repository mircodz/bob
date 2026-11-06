//! Application state machine and the JSON event protocol shared with the UI.

use serde::{Deserialize, Serialize};

/// High-level dictation states, rendered by the notch pill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DictationState {
    /// Loading and warming up the model; not ready to dictate yet.
    Loading,
    /// Not recording; waiting for the hotkey or VAD trigger.
    Idle,
    /// Mic is open and capturing audio.
    Listening,
    /// Audio captured; model is producing text.
    Transcribing,
    /// Final text is being typed into the focused app.
    Injecting,
    /// A recoverable error occurred (message carried in the event).
    Error,
}

/// Events emitted by the core, serialized as one JSON object per line on stdout.
/// The Swift notch overlay consumes these to drive its animation and text.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// State machine transitioned.
    State { state: DictationState },
    /// Interim hypothesis while transcribing (may change).
    Partial { text: String },
    /// Final transcription for an utterance.
    Final { text: String },
    /// Human-readable error.
    Error { message: String },
}

impl Event {
    /// Serialize to a single JSON line (no trailing newline).
    pub fn to_line(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|e| format!(r#"{{"type":"error","message":"serialize failed: {e}"}}"#))
    }
}
