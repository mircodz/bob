//! Post-transcription text cleanup.
//!
//! Raw ASR output is usually lowercase, unpunctuated, and full of disfluencies
//! ("um", "uh"). A cleanup pass is what makes dictation feel polished.
//!
//! Everything sits behind the [`Cleanup`] trait so we can start with cheap
//! rule-based tidying now and later drop in an in-process tiny LLM
//! (llama.cpp / Candle) without touching the pipeline.

/// Transforms a raw transcript into final text to inject.
///
/// `Send + Sync` so a `Voice` (which owns a boxed `Cleanup`) can be shared
/// across threads via `Arc` — needed to drive a background dictation session.
pub trait Cleanup: Send + Sync {
    fn clean(&self, raw: &str) -> String;
}

/// No-op passthrough (useful for debugging raw model output).
pub struct NoCleanup;
impl Cleanup for NoCleanup {
    fn clean(&self, raw: &str) -> String {
        raw.trim().to_string()
    }
}

/// Fast, dependency-free tidy: strip fillers, collapse whitespace, capitalize
/// sentence starts, ensure terminal punctuation.
pub struct RuleBasedCleanup {
    fillers: Vec<&'static str>,
}

impl Default for RuleBasedCleanup {
    fn default() -> Self {
        Self {
            fillers: vec!["um", "uh", "erm", "uhh", "umm", "hmm"],
        }
    }
}

impl Cleanup for RuleBasedCleanup {
    fn clean(&self, raw: &str) -> String {
        // Drop filler tokens (whole-word, case-insensitive).
        let kept: Vec<&str> = raw
            .split_whitespace()
            .filter(|w| {
                let bare = w
                    .trim_matches(|c: char| !c.is_alphanumeric())
                    .to_lowercase();
                !self.fillers.contains(&bare.as_str())
            })
            .collect();
        let mut text = kept.join(" ");

        // Capitalize the first alphabetic char.
        if let Some(pos) = text.find(|c: char| c.is_alphabetic()) {
            let mut chars: Vec<char> = text.chars().collect();
            let idx = text[..pos].chars().count();
            chars[idx] = chars[idx].to_ascii_uppercase();
            text = chars.into_iter().collect();
        }

        // Ensure terminal punctuation.
        if !text.is_empty() && !text.ends_with(['.', '!', '?', ',', ':']) {
            text.push('.');
        }
        text
    }
}

/// In-process LLM cleanup: a tiny instruct model (Qwen2.5-0.5B via llama.cpp)
/// reformats the transcript — punctuation, casing, filler removal — WITHOUT
/// changing the words. Metal-accelerated; loads once and stays resident.
///
/// The model runs greedily with a tight instruction and a low token budget, so
/// output stays deterministic and fast (~a few hundred ms for a sentence).
pub struct LlmCleanup {
    backend: llama_cpp_2::llama_backend::LlamaBackend,
    model: llama_cpp_2::model::LlamaModel,
    // llama contexts need &mut; Cleanup::clean takes &self, so guard with a lock.
    ctx_params: llama_cpp_2::context::params::LlamaContextParams,
    lock: std::sync::Mutex<()>,
}

impl LlmCleanup {
    /// Load a GGUF instruct model (e.g. Qwen2.5-0.5B-Instruct Q4_K_M).
    pub fn new(model_path: &std::path::Path) -> anyhow::Result<Self> {
        use llama_cpp_2::context::params::LlamaContextParams;
        use llama_cpp_2::llama_backend::LlamaBackend;
        use llama_cpp_2::model::params::LlamaModelParams;
        use llama_cpp_2::model::LlamaModel;
        use std::num::NonZeroU32;

        let backend = LlamaBackend::init()?;
        let model_params = LlamaModelParams::default().with_n_gpu_layers(1000);
        let model = LlamaModel::load_from_file(&backend, model_path, &model_params)?;
        let ctx_params =
            LlamaContextParams::default().with_n_ctx(Some(NonZeroU32::new(1024).unwrap()));
        Ok(Self {
            backend,
            model,
            ctx_params,
            lock: std::sync::Mutex::new(()),
        })
    }

    /// Build the Qwen chat-format prompt for a formatting-only rewrite.
    fn prompt(&self, raw: &str) -> String {
        let system = "You are a transcription formatter. Fix capitalization, \
            punctuation, and remove filler words (um, uh). Do NOT add, remove, \
            or change any other words. Output ONLY the corrected text, nothing else.";
        format!(
            "<|im_start|>system\n{system}<|im_end|>\n\
             <|im_start|>user\n{raw}<|im_end|>\n\
             <|im_start|>assistant\n"
        )
    }

    fn run(&self, raw: &str) -> anyhow::Result<String> {
        use llama_cpp_2::llama_batch::LlamaBatch;
        use llama_cpp_2::model::AddBos;
        use llama_cpp_2::sampling::LlamaSampler;

        let _guard = self.lock.lock().unwrap();
        let mut ctx = self
            .model
            .new_context(&self.backend, self.ctx_params.clone())?;

        let prompt = self.prompt(raw);
        let tokens = self.model.str_to_token(&prompt, AddBos::Always)?;

        // Cap generation relative to input; formatting never needs much more.
        let max_new = (tokens.len() as i32) + 64;

        let mut batch = LlamaBatch::new(512, 1);
        let last = tokens.len() as i32 - 1;
        for (i, tok) in (0i32..).zip(tokens.into_iter()) {
            batch.add(tok, i, &[0], i == last)?;
        }
        ctx.decode(&mut batch)?;

        let mut sampler = LlamaSampler::chain_simple([LlamaSampler::greedy()]);
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let mut out = String::new();
        let mut n_cur = batch.n_tokens();

        while n_cur <= max_new {
            let token = sampler.sample(&ctx, batch.n_tokens() - 1);
            sampler.accept(token);
            if self.model.is_eog_token(token) {
                break;
            }
            out.push_str(&self.model.token_to_piece(token, &mut decoder, true, None)?);
            batch.clear();
            batch.add(token, n_cur, &[0], true)?;
            n_cur += 1;
            ctx.decode(&mut batch)?;
        }
        Ok(out.trim().to_string())
    }
}

impl Cleanup for LlmCleanup {
    fn clean(&self, raw: &str) -> String {
        let raw = raw.trim();
        if raw.is_empty() {
            return String::new();
        }
        // Fall back to the raw text on any model error — never lose a transcript.
        match self.run(raw) {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => raw.to_string(),
            Err(e) => {
                eprintln!("llm cleanup failed, using raw text: {e}");
                raw.to_string()
            }
        }
    }
}
