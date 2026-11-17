//! Speech recognition through transcribe.cpp, linked directly.
//!
//! The model stays loaded for the process: shelling out to `handy` cost ~350 ms
//! per utterance versus ~50 ms resident. Backend is left `Auto` so ggml picks
//! Vulkan/Metal itself; forcing one would break the macOS build.

use crate::model;
use crate::transcriber::Transcriber;
use std::path::Path;
use std::sync::Mutex;
use transcribe_cpp::{Feature, Model, RunExtension, RunOptions, Session, WhisperRunOptions};

/// Transcribes with a permanently loaded model.
pub struct DirectTranscriber {
    /// Mutex makes this `Sync` (required by `Box<dyn Transcriber>`); one run per model.
    session: Mutex<Session>,
    /// Language hint; Canary and Nemotron return empty text without one.
    language: Option<String>,
    /// Whisper decoding prompt; biases it to spell song and artist names as names.
    prompt: Option<String>,
    /// Kept for the startup log line.
    model_id: String,
}

impl DirectTranscriber {
    /// Loads the model once; `model` is a path or catalogue id (see [`crate::model::resolve`]).
    pub fn new(model: &str, language: Option<&str>, prompt: Option<&str>) -> anyhow::Result<Self> {
        // Process-global init; result stashed so a missing backend is a startup error, not a slow load.
        static BACKENDS: std::sync::OnceLock<anyhow::Result<()>> = std::sync::OnceLock::new();
        let backends = BACKENDS.get_or_init(|| {
            // Route native logs through `log` so RUST_LOG governs them.
            transcribe_cpp::init_logging();
            transcribe_cpp::init_backends_default().map_err(anyhow::Error::from)
        });
        if let Err(e) = backends {
            anyhow::bail!("failed to initialise transcribe.cpp backends: {e}");
        }

        let path = model::resolve(model)?;
        let loaded = Model::load(&path)
            .map_err(|e| anyhow::anyhow!("failed to load ASR model {}: {e}", path.display()))?;

        // The engine only rejects a bad language at run time; catch it at startup. ISO-639-1 codes.
        if let Some(language) = language {
            let supported = loaded.capabilities().languages;
            if !language_is_known(language, &supported) {
                anyhow::bail!(
                    "ASR_LANGUAGE={language} is not a language this model knows; \
                     expected an ISO-639-1 code such as {}",
                    supported.first().map(String::as_str).unwrap_or("en")
                );
            }
        }

        // The device the model actually bound to, so a silent CPU fallback shows up.
        let device = loaded
            .device()
            .map(|d| d.name)
            .unwrap_or_else(|e| format!("unresolved ({e})"));
        tracing::info!(
            model = %path.display(),
            backend = %loaded.backend(),
            device = %device,
            "loaded ASR model"
        );

        let prompt = prompt
            .filter(|_| loaded.supports(Feature::InitialPrompt))
            .map(str::to_string);
        if let Some(prompt) = &prompt {
            tracing::info!(prompt = %prompt, "decoding with a prompt");
        }

        let session = loaded.session()?;

        Ok(Self {
            session: Mutex::new(session),
            language: language.map(str::to_string),
            prompt,
            model_id: model.to_string(),
        })
    }

    /// The model this transcriber was built from, for logging.
    pub fn model_id(&self) -> &str {
        &self.model_id
    }
}

impl Transcriber for DirectTranscriber {
    fn sample_rate(&self) -> u32 {
        16_000
    }

    fn transcribe(&self, samples: &[f32]) -> anyhow::Result<String> {
        if samples.is_empty() {
            return Ok(String::new());
        }

        let options = RunOptions {
            language: self.language.clone(),
            family: self.prompt.as_ref().map(|prompt| {
                RunExtension::Whisper(WhisperRunOptions {
                    initial_prompt: Some(prompt.clone()),
                    ..WhisperRunOptions::default()
                })
            }),
            ..RunOptions::default()
        };

        // Poisoned by an earlier panic; the session is still valid.
        let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
        let transcript = session.run(samples, &options)?;

        tracing::debug!(
            text = %transcript.text,
            total_ms = transcript.timings.load_ms
                + transcript.timings.mel_ms
                + transcript.timings.encode_ms
                + transcript.timings.decode_ms,
            "transcribed"
        );

        Ok(transcript.text)
    }
}

/// True when `path` looks like something [`DirectTranscriber::new`] can open.
pub fn model_exists(spec: &str) -> bool {
    Path::new(spec).is_file() || model::resolve(spec).is_ok()
}

/// Checks a language hint against what a model advertises; split out to test without a GGUF.
fn language_is_known(language: &str, supported: &[String]) -> bool {
    // Some Parakeet builds advertise no list; accept anything.
    supported.is_empty() || supported.iter().any(|known| known == language)
}

#[cfg(test)]
#[path = "tests/direct.rs"]
mod tests;
