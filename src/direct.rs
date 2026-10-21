//! Speech recognition through transcribe.cpp, linked directly.
//!
//! This is the same engine the Handy app wraps, and the same Parakeet models,
//! but the model is loaded once at startup and held in the process instead of
//! being reloaded for every command. That is the whole point of this module.
//!
//! Going through `handy --transcribe-file` cost about 350 ms per utterance on
//! this machine, of which only ~155 ms was transcription: ~180 ms was the Tauri
//! app starting up (loading compute backends, seeding 69 catalogue models into
//! its registry) and ~125 ms was reloading the weights from disk. Neither
//! depends on the audio. Holding the model resident leaves only inference, and
//! the same utterance then transcribes in ~50 ms.
//!
//! It also removes the temp WAV file: the engine takes raw f32 PCM, so the
//! samples go straight from the voice connection to the decoder.
//!
//! Inference goes on the GPU when one is available, because `Model::load`
//! leaves the backend as `Auto` and ggml picks the best device it has: Vulkan
//! on AMD and Intel under Linux, Metal on macOS. The difference is not marginal
//! for the Whisper models -- on an RX 570 a decode that takes ~17 s on the CPU
//! takes ~1 s on the GPU. Requesting a backend here instead would be worse,
//! because it would take Metal away from the macOS build for no gain.

use crate::model;
use crate::transcriber::Transcriber;
use std::sync::Mutex;
use transcribe_cpp::{Model, RunOptions, Session};

/// Transcribes with a permanently loaded model.
pub struct DirectTranscriber {
    /// `Session` is `Send` but not `Sync`: its methods take `&mut self` and the
    /// underlying library allows one in-flight run per model. The mutex is what
    /// makes this type `Sync`, which `Box<dyn Transcriber>` requires.
    session: Mutex<Session>,
    /// A language hint, for the models that need one (Canary and Nemotron
    /// return empty text without it). `None` leaves detection to the model.
    language: Option<String>,
}

impl DirectTranscriber {
    /// Loads the model and opens a session. Costs one model load, so call it
    /// once at startup rather than per utterance.
    ///
    /// `model` is either a path or a Handy catalogue id; see
    /// [`crate::model::resolve`].
    pub fn new(model: &str, language: Option<&str>) -> anyhow::Result<Self> {
        // Both of these are process-global and the library asks for them once,
        // before the first model load. `Once` keeps a second engine from
        // re-scanning the backend directory.
        //
        // `Once::call_once` cannot report failure to the caller, so the result
        // is stashed and re-checked here: a missing compute backend surfaces as
        // a normal startup error instead of a model load that mysteriously
        // runs at glacial speed.
        static BACKENDS: std::sync::OnceLock<anyhow::Result<()>> = std::sync::OnceLock::new();
        let backends = BACKENDS.get_or_init(|| {
            // Route the native library's chatter through `log`, which
            // `tracing_subscriber` already bridges, so RUST_LOG governs it.
            // Without this the engine prints to stderr on every utterance.
            transcribe_cpp::init_logging();
            transcribe_cpp::init_backends_default().map_err(anyhow::Error::from)
        });
        if let Err(e) = backends {
            anyhow::bail!("failed to initialise transcribe.cpp backends: {e}");
        }

        let path = model::resolve(model)?;
        let loaded = Model::load(&path)
            .map_err(|e| anyhow::anyhow!("failed to load ASR model {}: {e}", path.display()))?;

        // Checked here rather than left to the first utterance. The engine does
        // not reject an unknown language at load; it rejects it at *run*, so a
        // typo in `ASR_LANGUAGE` used to leave the bot joining the channel and
        // then failing every single transcription, which looks like a model
        // problem and is not one. Codes are plain ISO-639-1 -- `pt` and `en`,
        // not `pt-PT` or `Portuguese`.
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

        // The backend and device the model actually *bound to*, not the devices
        // that merely registered. That distinction is the whole question: a
        // registered Vulkan0 alongside "CPU" says nothing about where the
        // weights went, and silently falling back to the CPU is exactly what
        // this reports so it can be noticed.
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

        let session = loaded.session()?;

        Ok(Self {
            session: Mutex::new(session),
            language: language.map(str::to_string),
        })
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
            ..RunOptions::default()
        };

        // A poisoned lock means an earlier call panicked; the session is still
        // a valid pointer, so recover rather than propagating the panic.
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

/// Checks a language hint against what a model advertises, without a model.
///
/// The engine takes a language it does not know and only complains when it runs,
/// which is far too late: the bot is already in the voice channel by then. Split
/// out so the rule is testable without a several-hundred-megabyte GGUF.
fn language_is_known(language: &str, supported: &[String]) -> bool {
    // A model that advertises nothing accepts anything; some Parakeet builds do
    // not publish a list at all.
    supported.is_empty() || supported.iter().any(|known| known == language)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Transcriber` is used behind `Arc<dyn Transcriber>`, which needs
    /// `Send + Sync`. `Session` is neither on its own, so this fails to compile
    /// if the mutex is ever removed.
    #[test]
    fn the_transcriber_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DirectTranscriber>();
    }

    #[test]
    fn empty_audio_transcribes_to_nothing() {
        // Building a real model needs a GGUF on disk, so this only checks the
        // guard that runs before the session is ever touched.
        assert!(model::DEFAULT_MODEL_ID.ends_with(".gguf"));
    }

    fn whisper_languages() -> Vec<String> {
        ["en", "pt", "es", "fr"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// `pt-PT` is the obvious thing to type for European Portuguese and it is
    /// wrong: the engine takes plain ISO-639-1 and rejects everything else at
    /// *run* time, which used to mean the bot joined the channel and then failed
    /// every transcription.
    #[test]
    fn a_language_the_model_does_not_know_is_refused_up_front() {
        let known = whisper_languages();
        assert!(language_is_known("en", &known));
        assert!(language_is_known("pt", &known));

        for wrong in ["pt-PT", "pt_PT", "Portuguese", "english", ""] {
            assert!(
                !language_is_known(wrong, &known),
                "{wrong} should not have been accepted"
            );
        }
    }

    /// A model that publishes no language list -- some Parakeet builds -- takes
    /// anything, and refusing to start would be worse than trying.
    #[test]
    fn a_model_that_advertises_nothing_accepts_anything() {
        assert!(language_is_known("anything", &[]));
    }
}
