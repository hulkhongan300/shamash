use crate::transcriber::Transcriber;
use anyhow::Context;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// Whisper-backed [`Transcriber`] running whisper.cpp locally.
///
/// whisper.cpp is not thread-safe across concurrent transcriptions, so all
/// calls are serialized with a mutex.
pub struct WhisperTranscriber {
    context: WhisperContext,
    threads: i32,
    lock: std::sync::Mutex<()>,
}

impl WhisperTranscriber {
    /// Loads a GGML Whisper model from disk.
    pub fn new(model_path: &str, threads: i32) -> anyhow::Result<Self> {
        let context =
            WhisperContext::new_with_params(model_path, WhisperContextParameters::default())
                .map_err(|e| anyhow::anyhow!("failed to load Whisper model '{model_path}': {e}"))?;
        Ok(Self {
            context,
            threads,
            lock: std::sync::Mutex::new(()),
        })
    }
}

impl Transcriber for WhisperTranscriber {
    fn sample_rate(&self) -> u32 {
        16_000
    }

    fn transcribe(&self, samples: &[f32]) -> anyhow::Result<String> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self
            .context
            .create_state()
            .context("failed to create Whisper state")?;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(self.threads);
        params.set_print_special(false);
        params.set_print_progress(false);
        state
            .full(params, samples)
            .context("failed to transcribe audio")?;

        let mut transcript = String::new();
        for i in 0..state.full_n_segments() {
            if let Some(segment) = state.get_segment(i) {
                transcript.push_str(&segment.to_string());
                transcript.push(' ');
            }
        }
        Ok(transcript.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_model_is_a_clear_error() {
        let err = match WhisperTranscriber::new("/nonexistent/whisper-model.bin", 1) {
            Ok(_) => panic!("expected loading a missing model to fail"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("failed to load Whisper model"));
    }
}
