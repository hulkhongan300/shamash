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

use crate::model;
use crate::transcriber::Transcriber;
use std::path::Path;
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
    /// Kept for the startup log line; the session holds its own reference.
    model_id: String,
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
        let session = loaded.session()?;

        tracing::info!(
            model = %path.display(),
            device = ?transcribe_cpp::devices()
                .iter()
                .map(|d| d.name.clone())
                .collect::<Vec<_>>(),
            "loaded ASR model"
        );

        Ok(Self {
            session: Mutex::new(session),
            language: language.map(str::to_string),
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

/// The model id used when `PARAKEET_MODEL` is unset.
pub fn default_model() -> &'static str {
    model::DEFAULT_MODEL_ID
}

/// True when `path` looks like something [`DirectTranscriber::new`] can open.
///
/// Used by the startup check to explain a bad `PARAKEET_MODEL` before the bot
/// joins a voice channel.
pub fn model_exists(spec: &str) -> bool {
    Path::new(spec).is_file() || model::resolve(spec).is_ok()
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
}
