//! The high-level, embeddable API: [`Voice`] and its [`builder`](Voice::builder).
//!
//! This is the "one line to get text" surface. It owns a transcriber and a
//! cleanup pass, and exposes simple `transcribe*` calls plus a [`Session`] for
//! live dictation.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};

use crate::cleanup::{Cleanup, RuleBasedCleanup};
use crate::session::Session;
use crate::stt::Transcriber;

/// Which cleanup pass to apply to raw transcripts.
pub enum CleanupChoice {
    /// Fast rule-based tidy (default): fillers, casing, terminal punctuation.
    Rule,
    /// In-process LLM formatter (Qwen-style GGUF). Needs a model path.
    Llm(PathBuf),
    /// No cleanup — raw model output.
    None,
    /// A custom cleanup implementation.
    Custom(Box<dyn Cleanup>),
}

/// Builder for [`Voice`]. Configure the model, language, and cleanup, then
/// [`build`](VoiceBuilder::build).
pub struct VoiceBuilder {
    model: Option<PathBuf>,
    language: Option<String>,
    cleanup: CleanupChoice,
    warmup: bool,
}

impl Default for VoiceBuilder {
    fn default() -> Self {
        Self {
            model: None,
            language: None,
            cleanup: CleanupChoice::Rule,
            warmup: true,
        }
    }
}

impl VoiceBuilder {
    /// Path to the Whisper ggml model (e.g. `models/ggml-large-v3-turbo.bin`).
    pub fn model(mut self, path: impl Into<PathBuf>) -> Self {
        self.model = Some(path.into());
        self
    }

    /// Transcription language (e.g. `"en"`). Omit for auto-detect.
    pub fn language(mut self, lang: impl Into<String>) -> Self {
        self.language = Some(lang.into());
        self
    }

    /// Choose the cleanup pass (default: [`CleanupChoice::Rule`]).
    pub fn cleanup(mut self, choice: CleanupChoice) -> Self {
        self.cleanup = choice;
        self
    }

    /// Whether to warm up the model on build (default: true). Warming compiles
    /// GPU kernels so the first real transcription isn't slow.
    pub fn warmup(mut self, on: bool) -> Self {
        self.warmup = on;
        self
    }

    /// Load the model + cleanup and produce a ready [`Voice`].
    pub fn build(self) -> Result<Voice> {
        let transcriber = build_transcriber(self.model.as_deref(), self.language.clone())?;
        let cleanup = build_cleanup(self.cleanup)?;

        let mut voice = Voice {
            transcriber: Mutex::new(transcriber),
            cleanup,
            language: self.language,
        };
        if self.warmup {
            voice.warmup()?;
        }
        Ok(voice)
    }
}

/// A loaded, ready speech-to-text engine. Cheap to share behind an `Arc`.
pub struct Voice {
    // Behind a Mutex so `&self` methods can drive the mutable transcriber and
    // the type stays `Sync` for easy embedding (Arc<Voice>).
    transcriber: Mutex<Box<dyn Transcriber>>,
    cleanup: Box<dyn Cleanup>,
    language: Option<String>,
}

impl Voice {
    /// Start configuring a new instance.
    pub fn builder() -> VoiceBuilder {
        VoiceBuilder::default()
    }

    /// Warm up the underlying model (compile kernels / page in weights).
    pub fn warmup(&mut self) -> Result<()> {
        self.transcriber.get_mut().unwrap().warmup()
    }

    /// Transcribe raw mono 16 kHz f32 samples, returning cleaned text.
    pub fn transcribe(&self, samples: &[f32]) -> Result<String> {
        let mut t = self.transcriber.lock().unwrap();
        t.reset();
        t.accept_audio(samples)?;
        let finals = t.finish()?;
        drop(t);
        let raw = finals
            .into_iter()
            .filter(|x| x.is_final)
            .map(|x| x.text)
            .collect::<Vec<_>>()
            .join(" ");
        Ok(self.cleanup.clean(&raw))
    }

    /// Transcribe a WAV file (16-bit PCM), returning cleaned text.
    pub fn transcribe_wav(&self, path: impl AsRef<Path>) -> Result<String> {
        let samples =
            crate::audio::read_wav_16k_mono(path.as_ref()).with_context(|| "reading WAV file")?;
        self.transcribe(&samples)
    }

    /// The configured language, if any.
    pub fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }

    /// Begin a live dictation [`Session`] driven by the microphone. Consumes
    /// the `Voice` (the session owns it); use [`shared`](Voice::shared) +
    /// [`session_shared`](Voice::session_shared) if you need to keep a handle.
    pub fn into_session(self) -> Result<Session> {
        Session::new(std::sync::Arc::new(self))
    }

    /// Wrap in an `Arc` for sharing across threads / multiple sessions.
    pub fn shared(self) -> std::sync::Arc<Self> {
        std::sync::Arc::new(self)
    }

    /// Begin a live dictation [`Session`] from a shared handle.
    pub fn session_shared(self: &std::sync::Arc<Self>) -> Result<Session> {
        Session::new(self.clone())
    }

    /// Internal: run one utterance for a session (used by [`Session`]).
    pub(crate) fn transcribe_locked(&self, samples: &[f32]) -> Result<String> {
        self.transcribe(samples)
    }
}

fn build_transcriber(
    model: Option<&Path>,
    language: Option<String>,
) -> Result<Box<dyn Transcriber>> {
    match model {
        #[cfg(feature = "whisper")]
        Some(path) => {
            let t = crate::stt::WhisperTranscriber::new(path, language)?;
            Ok(Box::new(t))
        }
        #[cfg(not(feature = "whisper"))]
        Some(_) => {
            let _ = language;
            anyhow::bail!("bob-voice built without the `whisper` feature")
        }
        None => Ok(Box::new(crate::stt::MockTranscriber::new())),
    }
}

fn build_cleanup(choice: CleanupChoice) -> Result<Box<dyn Cleanup>> {
    match choice {
        CleanupChoice::Rule => Ok(Box::new(RuleBasedCleanup::default())),
        CleanupChoice::None => Ok(Box::new(crate::cleanup::NoCleanup)),
        CleanupChoice::Custom(c) => Ok(c),
        CleanupChoice::Llm(path) => {
            let c = crate::cleanup::LlmCleanup::new(&path)?;
            Ok(Box::new(c))
        }
    }
}
