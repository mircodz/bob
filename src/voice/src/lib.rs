//! bob-voice — local speech-to-text.
//!
//! Turns speech into text, fully on-device: microphone capture → Whisper
//! transcription → text cleanup. Platform concerns (typing into apps, global
//! hotkeys, UI) live in the consuming application, not here — bob-voice is just
//! "voice → text" so it stays portable and easy to embed.
//!
//! # Quick start (one call)
//!
//! ```no_run
//! use bob_voice::Voice;
//!
//! let voice = Voice::builder()
//!     .model("models/ggml-large-v3-turbo.bin")
//!     .language("en")
//!     .build()?;
//!
//! let text = voice.transcribe_wav("clip.wav")?;
//! println!("{text}");
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! # Live dictation (event session)
//!
//! Utterance boundaries are driven explicitly (push-to-talk): `start()` on
//! hotkey down, `stop()` on hotkey up. Results arrive as `Event::Final`.
//!
//! ```no_run
//! # use bob_voice::{Voice, Event};
//! # let voice = Voice::builder().build()?;
//! let session = voice.into_session()?;
//! session.start();                        // begin capturing (mic opens)
//! // … user speaks …
//! session.stop();                         // transcribe the utterance
//! for event in session.events() {
//!     if let Event::Final { text } = event {
//!         println!("{text}");
//!         break;
//!     }
//! }
//! # Ok::<(), anyhow::Error>(())
//! ```

pub mod api;
pub mod audio;
pub mod cleanup;
pub mod session;
pub mod state;
pub mod stt;

pub use api::{Voice, VoiceBuilder};
pub use cleanup::{Cleanup, NoCleanup, RuleBasedCleanup};
pub use session::Session;
pub use state::{DictationState, Event};
pub use stt::{Transcriber, Transcription};
