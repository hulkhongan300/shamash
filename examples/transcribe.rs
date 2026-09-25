//! Local test harness for the wake-word → play-request pipeline.
//!
//! Transcribes a WAV file (16-bit PCM, mono or stereo, any sample rate) using
//! the same path as live voice: resample to 16 kHz, VAD-split into
//! utterances, transcribe, then parse. Prints transcripts and any resulting
//! play request so you can test the bot's ears without Discord.
//!
//!     cargo run --release --example transcribe -- path/to/recording.wav
//!
//! Requires a Whisper model. Fetch one with `scripts/setup.sh` (default
//! `data/model.bin`) or point at another with `WHISPER_MODEL`.

use anyhow::Context;
use shamash::audio::{Resampler, VadBuffer};
use shamash::config::Config;
use shamash::parser::CommandParser;
use shamash::transcriber::Transcriber;
use shamash::whisper::WhisperTranscriber;
use std::path::Path;

fn main() -> anyhow::Result<()> {
    let wav_path = std::env::args()
        .nth(1)
        .context("usage: transcribe <recording.wav>")?;
    let model_path =
        std::env::var("WHISPER_MODEL").unwrap_or_else(|_| "data/model.bin".to_string());
    let transcriber = WhisperTranscriber::new(&model_path, 4)
        .with_context(|| format!("failed to load Whisper model at '{model_path}'"))?;

    let config = Config {
        wake_words: std::env::var("WAKE_WORDS")
            .map(|s| {
                s.split(',')
                    .map(|w| w.trim().to_ascii_lowercase())
                    .collect()
            })
            .unwrap_or_else(|_| ["shamash", "bot"].into_iter().map(str::to_string).collect()),
        // The rest of the config is unused by this harness.
        discord_token: String::new(),
        voice_channel_id: 0,
        whisper_model: model_path.clone(),
        alert_channel_id: None,
    };

    let (samples, sample_rate) = load_wav(Path::new(&wav_path))?;
    let resampled = Resampler::new(sample_rate, 16_000).resample(&samples);
    let duration = resampled.len() / 16_000;
    println!(
        "wav: {sample_rate} Hz -> {} samples at 16 kHz ({duration} s)",
        resampled.len()
    );
    let parser = CommandParser::new(config.wake_words.clone());

    let mut vad = VadBuffer::new(320, 0.02, 15, 600);
    let mut stored = String::new();
    for frame in resampled.as_chunks::<320>().0 {
        let utterance = vad.push(frame);
        if utterance.is_empty() {
            continue;
        }
        let transcript = transcriber
            .transcribe(&utterance)
            .with_context(|| format!("transcription failed for {wav_path}"))?;
        println!("hear: {transcript:?}");
        stored.push_str(&transcript);
        stored.push(' ');
        match parser.parse(&transcript) {
            Some(request) => println!("would play: {}", request.query),
            None => println!("(no play request)"),
        }
    }
    if stored.trim().is_empty() {
        println!("no speech detected; try a louder or longer recording");
    }
    println!("done.");
    Ok(())
}

/// Reads a RIFF/WAVE file and returns f32 samples plus the file's sample rate.
///
/// Supports PCM (format 1) at 16-bit, mono or stereo.
fn load_wav(path: &Path) -> anyhow::Result<(Vec<f32>, u32)> {
    let bytes = std::fs::read(path).context("failed to read wav file")?;
    let bytes = &bytes[..];
    anyhow::ensure!(
        bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE",
        "not a RIFF/WAVE file"
    );

    let mut offset = 12usize;
    let (mut sample_rate, mut channels, mut data) = (0u32, 0u16, &bytes[..0]);
    while offset + 8 <= bytes.len() {
        let chunk_id = &bytes[offset..offset + 4];
        let chunk_size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into()?) as usize;
        let chunk =
            &bytes[offset + 8..offset + 8 + chunk_size.min(bytes.len().saturating_sub(offset + 8))];
        match chunk_id {
            b"fmt " if chunk.len() >= 16 => {
                let format = u16::from_le_bytes(chunk[0..2].try_into()?);
                anyhow::ensure!(format == 1, "only raw PCM (format 1) is supported");
                channels = u16::from_le_bytes(chunk[2..4].try_into()?);
                sample_rate = u32::from_le_bytes(chunk[4..8].try_into()?);
                let bits = u16::from_le_bytes(chunk[14..16].try_into()?);
                anyhow::ensure!(bits == 16, "only 16-bit PCM is supported");
            }
            b"data" => {
                data = chunk;
            }
            _ => {}
        }
        offset += 8 + chunk_size + (chunk_size % 2);
    }

    anyhow::ensure!(sample_rate != 0 && channels != 0, "missing fmt chunk");
    anyhow::ensure!(!data.is_empty(), "missing data chunk");

    let mut samples = Vec::with_capacity(data.len() / 2);
    let frame_bytes = 2 * channels as usize;
    for frame in data.chunks_exact(frame_bytes) {
        let mut frame_sum = 0i64;
        for channel in frame.as_chunks::<2>().0 {
            let value = i16::from_le_bytes([channel[0], channel[1]]) as i64;
            frame_sum += value;
        }
        samples.push((frame_sum as f32 / channels as f32) / 32768.0);
    }
    Ok((samples, sample_rate))
}
