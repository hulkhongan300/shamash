/// Converts PCM audio into transcribed text.
pub trait Transcriber: Send + Sync {
    /// The sample rate (Hz) of PCM audio this transcriber expects.
    fn sample_rate(&self) -> u32;

    /// Transcribes a complete utterance of mono, f32 samples in
    /// [`Self::sample_rate`] Hz.
    fn transcribe(&self, samples: &[f32]) -> anyhow::Result<String>;
}
