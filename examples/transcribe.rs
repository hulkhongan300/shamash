//! Local test harness for the wake-word → play-request pipeline.
//!
//! Transcribes a WAV file (16-bit PCM, mono or stereo, any sample rate) using
//! the same path as live voice: resample to 16 kHz, VAD-split into
//! utterances, transcribe, then parse. Prints transcripts and any resulting
//! play request so you can test the bot's ears without Discord.
//!
//!     cargo run --release --example transcribe -- path/to/recording.wav
//!
//! Requires a Whisper model, because the harness drives the bundled engine
//! directly. Fetch one with `scripts/setup.sh` (default `data/model.bin`) or
//! point at another with `WHISPER_MODEL`. The bot itself defaults to the Handy
//! app and Parakeet instead; set `ASR_ENGINE=whisper` to use this engine there
//! too.

use anyhow::Context;
use shamash::audio::{Resampler, VadBuffer, rms};
use shamash::config::{AsrEngine, Config, DEFAULT_WAKE_WORDS};
use shamash::listener::{VAD_GAP_FRAMES, VAD_MAX_FRAMES, VAD_RMS_THRESHOLD};
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
            .unwrap_or_else(|_| DEFAULT_WAKE_WORDS.iter().map(|w| w.to_string()).collect()),
        // The rest of the config is unused by this harness.
        discord_token: String::new(),
        voice_channel_id: 0,
        // The harness always drives Whisper directly; the bot picks its engine
        // from the environment instead.
        asr_engine: AsrEngine::Whisper,
        whisper_model: model_path.clone(),
        parakeet_model: shamash::parakeet::DEFAULT_MODEL.to_string(),
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

    let mut vad = VadBuffer::new(320, VAD_RMS_THRESHOLD, VAD_GAP_FRAMES, VAD_MAX_FRAMES);
    let mut stored = String::new();
    let mut peak = 0.0f32;
    let mut frames_heard = 0usize;
    let mut frames_total = 0usize;
    for frame in resampled.as_chunks::<320>().0 {
        frames_total += 1;
        let level = rms(frame);
        peak = peak.max(level);
        if level >= VAD_RMS_THRESHOLD {
            frames_heard += 1;
        }
        let utterance = vad.push(frame);
        if utterance.is_empty() {
            continue;
        }
        let transcript = transcriber
            .transcribe(&utterance)
            .with_context(|| format!("transcription failed for {wav_path}"))?;
        println!("heard: {transcript:?}");
        stored.push_str(&transcript);
        stored.push(' ');
        match parser.parse_with_reason(&transcript) {
            Ok(request) => println!("play: {}", request.query),
            Err(miss) => println!("not a play command: {miss}"),
        }
    }

    // Level report, so a threshold that is wrong for this voice is visible
    // rather than just silent.
    println!(
        "level: peak frame rms {:.4} ({:.1} dBFS), gate {:.4} ({:.1} dBFS)",
        peak,
        to_dbfs(peak),
        VAD_RMS_THRESHOLD,
        to_dbfs(VAD_RMS_THRESHOLD),
    );
    println!(
        "gate: {frames_heard} of {frames_total} frames counted as speech ({:.0}%)",
        100.0 * frames_heard as f32 / frames_total.max(1) as f32
    );
    if frames_heard == 0 {
        println!(
            "nothing crossed the gate; speak louder or closer, or lower VAD_RMS_THRESHOLD in src/listener.rs"
        );
    }
    if stored.trim().is_empty() {
        println!("no speech detected; try a louder or longer recording");
    }
    println!("done.");
    Ok(())
}

/// Full-scale decibel level of a linear amplitude.
fn to_dbfs(amplitude: f32) -> f32 {
    20.0 * amplitude.max(1e-9).log10()
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
