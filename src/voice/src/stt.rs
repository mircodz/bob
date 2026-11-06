//! Speech-to-text abstraction.
//!
//! The rest of the app depends only on the [`Transcriber`] trait. The scaffold
//! ships a [`MockTranscriber`] so the full pipeline runs with no model files.
//! A real implementation (`SherpaTranscriber`, Parakeet/Whisper via sherpa-onnx)
//! lives behind the `sherpa` feature flag.

use anyhow::Result;

/// One transcription result: interim or final.
#[derive(Debug, Clone)]
pub struct Transcription {
    pub text: String,
    pub is_final: bool,
}

/// Streaming speech-to-text engine.
///
/// Feed mono 16 kHz f32 samples; drain interim/final results. Implementations
/// are expected to be `Send` so audio and STT can live on separate threads.
pub trait Transcriber: Send {
    /// Push a chunk of audio samples (mono, 16 kHz, f32 in [-1, 1]).
    fn accept_audio(&mut self, samples: &[f32]) -> Result<()>;

    /// Pull any results ready since the last call.
    fn poll(&mut self) -> Result<Vec<Transcription>>;

    /// Signal end-of-utterance (hotkey released / VAD endpoint). Flushes a final.
    fn finish(&mut self) -> Result<Vec<Transcription>>;

    /// Reset internal state for a new utterance.
    fn reset(&mut self);

    /// Warm up the engine so the first real utterance isn't slow. The default
    /// runs a short silent decode — this compiles GPU (Metal) kernels and pages
    /// in the model. Engines with no warmup benefit can leave the default no-op
    /// by overriding to `Ok(())`. Discards any output and resets afterward.
    fn warmup(&mut self) -> Result<()> {
        // ~0.5s of silence at 16 kHz.
        let silence = vec![0.0f32; 8000];
        self.reset();
        self.accept_audio(&silence)?;
        let _ = self.finish()?;
        self.reset();
        Ok(())
    }
}

/// A fake transcriber that emits deterministic partials then a final, driven by
/// how much audio it has "heard". Lets the whole pipeline + UI run model-free.
pub struct MockTranscriber {
    samples_seen: usize,
    last_emitted: usize,
    words: Vec<&'static str>,
    idx: usize,
}

impl Default for MockTranscriber {
    fn default() -> Self {
        Self::new()
    }
}

impl MockTranscriber {
    pub fn new() -> Self {
        Self {
            samples_seen: 0,
            last_emitted: 0,
            words: vec!["hello", "this", "is", "a", "local", "dictation", "test"],
            idx: 0,
        }
    }

    fn current_text(&self) -> String {
        self.words[..self.idx.min(self.words.len())].join(" ")
    }
}

impl Transcriber for MockTranscriber {
    fn accept_audio(&mut self, samples: &[f32]) -> Result<()> {
        self.samples_seen += samples.len();
        Ok(())
    }

    fn poll(&mut self) -> Result<Vec<Transcription>> {
        // Emit one more word roughly every ~0.4s of 16 kHz audio.
        const STEP: usize = 6400;
        let mut out = Vec::new();
        while self.samples_seen - self.last_emitted >= STEP && self.idx < self.words.len() {
            self.last_emitted += STEP;
            self.idx += 1;
            out.push(Transcription {
                text: self.current_text(),
                is_final: false,
            });
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<Transcription>> {
        self.idx = self.words.len();
        let text = self.current_text();
        Ok(vec![Transcription {
            text,
            is_final: true,
        }])
    }

    fn reset(&mut self) {
        self.samples_seen = 0;
        self.last_emitted = 0;
        self.idx = 0;
    }

    // The mock has nothing to warm up; skip the dummy decode.
    fn warmup(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Real STT via whisper.cpp (`whisper-rs`), Metal-accelerated on macOS.
///
/// Whisper is not streaming, so this buffers audio for the utterance and decodes
/// on [`finish`]. Great for push-to-talk (buffer while held, decode on release).
/// Enable with `--features whisper` and pass a ggml/gguf model path.
#[cfg(feature = "whisper")]
pub struct WhisperTranscriber {
    ctx: whisper_rs::WhisperContext,
    buffer: Vec<f32>,
    language: Option<String>,
}

#[cfg(feature = "whisper")]
impl WhisperTranscriber {
    /// `model_path` points at a ggml `.bin` (e.g. `ggml-base.en.bin`).
    /// `language` = `None` for auto-detect, or e.g. `Some("en")`.
    pub fn new(model_path: &std::path::Path, language: Option<String>) -> Result<Self> {
        use whisper_rs::{WhisperContext, WhisperContextParameters};
        let ctx = WhisperContext::new_with_params(
            &model_path.to_string_lossy(),
            WhisperContextParameters::default(),
        )
        .map_err(|e| anyhow::anyhow!("load whisper model: {e:?}"))?;
        Ok(Self {
            ctx,
            buffer: Vec::new(),
            language,
        })
    }
}

#[cfg(feature = "whisper")]
impl Transcriber for WhisperTranscriber {
    fn accept_audio(&mut self, samples: &[f32]) -> Result<()> {
        self.buffer.extend_from_slice(samples);
        Ok(())
    }

    // Whisper decodes the whole utterance at once; no interim results.
    fn poll(&mut self) -> Result<Vec<Transcription>> {
        Ok(Vec::new())
    }

    fn finish(&mut self) -> Result<Vec<Transcription>> {
        use whisper_rs::{FullParams, SamplingStrategy};
        if self.buffer.len() < 1600 {
            // < ~0.1s of audio; nothing worth decoding.
            self.buffer.clear();
            return Ok(Vec::new());
        }
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        if let Some(lang) = &self.language {
            params.set_language(Some(lang));
        }

        let mut st = self
            .ctx
            .create_state()
            .map_err(|e| anyhow::anyhow!("whisper state: {e:?}"))?;
        st.full(params, &self.buffer)
            .map_err(|e| anyhow::anyhow!("whisper decode: {e:?}"))?;

        let n = st
            .full_n_segments()
            .map_err(|e| anyhow::anyhow!("segments: {e:?}"))?;
        let mut text = String::new();
        for i in 0..n {
            if let Ok(seg) = st.full_get_segment_text(i) {
                text.push_str(&seg);
            }
        }
        self.buffer.clear();
        let text = text.trim().to_string();
        if text.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(vec![Transcription {
                text,
                is_final: true,
            }])
        }
    }

    fn reset(&mut self) {
        self.buffer.clear();
    }
}
