//! Times the in-process transcribe.cpp engine on a WAV file.
//!
//!     cargo run --release --example bench_direct -- recording.wav [model]
//!
//! The point is the "load" line versus the "run" lines: the load happens once
//! at startup, and every run after it is inference only. That split is the
//! whole reason this engine is faster than the Handy subprocess.

use anyhow::Context;
use shamash::direct::DirectTranscriber;
use shamash::transcriber::Transcriber;

fn main() -> anyhow::Result<()> {
    let wav_path = std::env::args()
        .nth(1)
        .context("usage: bench_direct <recording.wav> [model]")?;
    let model = std::env::args()
        .nth(2)
        .unwrap_or_else(|| shamash::model::DEFAULT_MODEL_ID.to_string());
    let language = std::env::var("ASR_LANGUAGE").ok().filter(|s| !s.is_empty());

    let bytes = std::fs::read(&wav_path).with_context(|| format!("reading {wav_path}"))?;
    let samples = read_pcm_s16le(&bytes).context("decoding WAV")?;
    println!(
        "audio: {:.1}s of 16 kHz mono",
        samples.len() as f32 / 16_000.0
    );

    let started = std::time::Instant::now();
    let transcriber = DirectTranscriber::new(&model, language.as_deref())?;
    println!("load:  {:?}", started.elapsed());

    for run in 1..=3 {
        let started = std::time::Instant::now();
        let text = transcriber.transcribe(&samples)?;
        println!("run {run}: {:?}  {text:?}", started.elapsed());
    }
    Ok(())
}

/// Reads 16-bit little-endian samples, skipping the header.
///
/// Only handles what `ffmpeg` and a recorder produce, which is the whole
/// purpose here: a benchmark tool that rejects its own test files is worse
/// than one that assumes 16 kHz mono.
fn read_pcm_s16le(bytes: &[u8]) -> anyhow::Result<Vec<f32>> {
    let data_start = find_data_chunk(bytes).context("no data chunk in WAV")?;
    let (samples, _) = bytes[data_start..].as_chunks::<2>();
    Ok(samples
        .iter()
        .map(|pair| i16::from_le_bytes(*pair) as f32 / 32_768.0)
        .collect())
}

/// The offset of the first `data` chunk's payload, per the RIFF layout.
fn find_data_chunk(bytes: &[u8]) -> Option<usize> {
    let id = |i: usize| &bytes[i..i + 4];
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().ok()?) as usize;
        if id(at) == b"data" {
            return Some(at + 8);
        }
        at += 8 + size + (size & 1);
    }
    None
}
