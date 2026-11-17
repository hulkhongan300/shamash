use crate::transcriber::Transcriber;
use anyhow::Context as _;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// The Handy model used when `PARAKEET_MODEL` is unset.
///
/// Hands each utterance to the Handy app's headless batch mode and returns the
/// text it prints.
///
/// Handy owns the model, the compute backend and the decoding parameters, so
/// Shamash does not carry its own ASR runtime. The model is Parakeet
/// (transcribe.cpp), which on this machine's GPU is roughly five times quicker
/// than the bundled Whisper for a spoken command, and it returns punctuated,
/// capitalized text instead of lowercased fragments.
pub struct HandyTranscriber {
    program: String,
    model: String,
    /// Serializes calls: one Handy process handles one utterance at a time.
    lock: std::sync::Mutex<()>,
}

impl HandyTranscriber {
    pub fn new(model: &str) -> Self {
        Self {
            program: "handy".to_string(),
            model: model.to_string(),
            lock: std::sync::Mutex::new(()),
        }
    }
}

/// The fields Shamash reads out of Handy's `--json` output.
#[derive(Debug, Deserialize)]
struct HandyOutput {
    text: String,
    #[serde(default)]
    load_ms: u64,
    #[serde(default)]
    transcribe_ms: Vec<u64>,
}

impl HandyTranscriber {
    /// Runs Handy over `wav_path` and returns its transcript.
    fn run(&self, wav_path: &Path) -> anyhow::Result<HandyOutput> {
        let output = Command::new(&self.program)
            .arg("--transcribe-file")
            .arg(wav_path)
            .arg("--json")
            .arg("--model")
            .arg(&self.model)
            .output()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    anyhow::anyhow!(
                        "could not find '{}' on path; install the Handy app, or drop \
                         ASR_ENGINE=handy to use the in-process engine, which does not need it",
                        self.program
                    )
                } else {
                    anyhow::anyhow!("failed to run '{}': {e}", self.program)
                }
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let tail: Vec<&str> = stderr.lines().rev().take(3).collect();
            anyhow::bail!(
                "{} failed with {}: {}",
                self.program,
                output.status,
                tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
            );
        }

        // Handy logs to stderr and prints one JSON object to stdout, but a
        // crash report can precede it, so start from the last line that parses.
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines().rev() {
            if let Ok(parsed) = serde_json::from_str::<HandyOutput>(line) {
                return Ok(parsed);
            }
        }

        anyhow::bail!(
            "{} printed no usable JSON; stdout was: {}",
            self.program,
            stdout.trim()
        )
    }
}

impl Transcriber for HandyTranscriber {
    fn sample_rate(&self) -> u32 {
        16_000
    }

    fn transcribe(&self, samples: &[f32]) -> anyhow::Result<String> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());

        let wav = wav::write(samples, self.sample_rate());
        let path = temp_wav_path();
        std::fs::write(&path, &wav)
            .with_context(|| format!("failed to write {}", path.display()))?;

        let result = self.run(&path);
        // The WAV only exists to be handed to the subprocess, so a failure to
        // delete it is worth a warning: it accumulates in the working directory
        // otherwise.
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::warn!("could not remove temp wav {}: {e}", path.display());
        }
        let output = result?;

        tracing::debug!(
            transcribe_ms = output.transcribe_ms.first().copied().unwrap_or(0),
            load_ms = output.load_ms,
            "transcribed via handy"
        );
        Ok(output.text)
    }
}

/// A unique path for one utterance's WAV file.
fn temp_wav_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("shamash-asr-{}-{n}.wav", std::process::id()))
}

/// Minimal 16-bit PCM WAV writing, the one format Handy's batch mode reads.
mod wav {

    pub fn write(samples: &[f32], sample_rate: u32) -> Vec<u8> {
        let data: Vec<u8> = samples
            .iter()
            .map(|s| (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16)
            .flat_map(i16::to_le_bytes)
            .collect();

        let byte_rate = sample_rate * 2;
        let mut out = Vec::with_capacity(44 + data.len());
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes()); // block align
        out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    /// Reads a mono 16-bit PCM WAV back into samples, for tests.
    #[cfg(test)]
    pub fn read(bytes: &[u8]) -> anyhow::Result<(u32, Vec<f32>)> {
        anyhow::ensure!(
            bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE",
            "not a RIFF/WAVE file"
        );
        let mut sample_rate = 0;
        let mut pos = 12;
        while pos + 8 <= bytes.len() {
            let id = &bytes[pos..pos + 4];
            let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
            let start = pos + 8;
            let end = start + size;
            if end > bytes.len() {
                break;
            }
            match id {
                b"fmt " => {
                    sample_rate =
                        u32::from_le_bytes(bytes[start + 4..start + 8].try_into().unwrap());
                }
                b"data" => {
                    let samples = bytes[start..end]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / f32::from(i16::MAX))
                        .collect();
                    return Ok((sample_rate, samples));
                }
                _ => {}
            }
            // Chunks are word aligned, so an odd size is followed by a pad byte.
            pos = end + (size % 2);
        }
        anyhow::bail!("WAV has no data chunk")
    }
}

#[cfg(test)]
#[path = "tests/parakeet.rs"]
mod tests;
