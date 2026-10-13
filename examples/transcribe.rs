//! Offline rehearsal of the two-stage listening pipeline.
//!
//!     cargo run --release --example transcribe -- path/to/recording.wav
//!
//! Feeds a WAV (16-bit PCM, mono or stereo, any sample rate) through the real
//! [`WakeGate`] rather than a copy of it: resample to 16 kHz, offer each window
//! to the small wake model, report what it heard, and when it hears the wake
//! word mark the model "ready" exactly as the bot does and capture the rest.
//! The captured request is then transcribed by the larger model and parsed.
//!
//! That makes the two things worth measuring on a real recording directly
//! visible: whether the small model can hear the wake word at all, and whether
//! the larger model turns the words after it into a playable request.
//!
//! Models come from the same settings as the bot: `WAKE_MODEL` for the small
//! one, `PARAKEET_MODEL` for the large, `WAKE_WORDS` for what counts as a wake
//! word, and `ASR_LANGUAGE=en` for models that need a language hint.
//!
//! Record a wake word and a request with a clear pause between them — the pause
//! is what the bot's two tones sit in, and a recording with no gap cannot show
//! the second stage working.

use anyhow::Context;
use shamash::audio::{Resampler, rms};
use shamash::config::{Config, DEFAULT_WAKE_WORDS};
use shamash::listener::{FRAME_SAMPLES, VAD_RMS_THRESHOLD};
use shamash::parser::CommandParser;
use shamash::transcriber::Transcriber;
use shamash::wake::{COMMAND_GAP_FRAMES, Heard, Step, WakeGate};
use std::path::Path;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let wav_path = std::env::args()
        .nth(1)
        .context("usage: transcribe <recording.wav>")?;

    // Config::from_env insists on the Discord settings so a typo in one is still
    // reported here. The harness never connects, so placeholders are enough.
    // SAFETY: single-threaded at this point; nothing else reads the environment.
    unsafe {
        std::env::set_var(
            "DISCORD_TOKEN",
            std::env::var("DISCORD_TOKEN").unwrap_or_else(|_| "harness".into()),
        );
        std::env::set_var(
            "VOICE_CHANNEL_ID",
            std::env::var("VOICE_CHANNEL_ID").unwrap_or_else(|_| "0".into()),
        );
    }
    let config = Config::from_env()?;
    let wake_words = if config.wake_words.is_empty() {
        DEFAULT_WAKE_WORDS.iter().map(|w| w.to_string()).collect()
    } else {
        config.wake_words.clone()
    };

    println!("wake model:   {}", config.wake_model);
    println!("request model: {}", config.parakeet_model);
    let mut sorted: Vec<&str> = wake_words.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    println!("wake words:   {}", sorted.join(", "));

    let wake_model = build(&config.wake_model, config.asr_language.as_deref())?;
    let parser = CommandParser::new(wake_words);
    let mut gate = WakeGate::new(wake_model, parser.clone());

    let (samples, sample_rate) = load_wav(Path::new(&wav_path))?;
    let resampled = Resampler::new(sample_rate, 16_000).resample(&samples);
    let duration = resampled.len() / 16_000;
    println!(
        "wav: {sample_rate} Hz -> {} samples at 16 kHz ({duration} s)",
        resampled.len()
    );

    let mut peak = 0.0f32;
    let mut frames_heard = 0usize;
    let mut frames_total = 0usize;
    let mut checks = 0usize;
    let mut command: Option<Vec<f32>> = None;
    // Loaded when the wake word is heard, exactly as the bot does, so the time
    // it takes is the time the speaker would really have waited.
    let mut request_model: Option<std::sync::Arc<dyn Transcriber>> = None;

    // A synthetic tail of silence stands in for the speaker having stopped, so a
    // recording that ends mid-request still closes the capture.
    let frames = resampled
        .as_chunks::<FRAME_SAMPLES>()
        .0
        .iter()
        .map(|f| f.to_vec())
        .chain(std::iter::repeat_with(|| vec![0.0f32; FRAME_SAMPLES]).take(COMMAND_GAP_FRAMES * 3));

    for frame in frames {
        frames_total += 1;
        let level = rms(&frame);
        peak = peak.max(level);
        if level >= VAD_RMS_THRESHOLD {
            frames_heard += 1;
        }
        match gate.push(&frame) {
            Step::Idle => {}
            Step::Check(window) => {
                checks += 1;
                let transcript = gate.transcriber().transcribe(&window)?;
                println!("wake model heard: {transcript:?}");
                if let Some(heard) = gate.transcribed(&transcript) {
                    let Heard::Wake(hit) = heard else {
                        unreachable!("a check only ever reports a wake word");
                    };
                    println!(
                        ">>> wake word heard after {checks} check(s), in {:.2} s of audio",
                        hit.audio.len() as f32 / 16_000.0
                    );
                    // What the bot does between its two tones: the larger model
                    // loads, then the gate is told it is ready.
                    let started = Instant::now();
                    request_model = Some(build(
                        &config.parakeet_model,
                        config.asr_language.as_deref(),
                    )?);
                    println!("larger model ready in {:?}", started.elapsed());
                    hit.ready.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            Step::Heard(heard) => {
                let Heard::Command(audio) = heard else {
                    unreachable!("only a check reports a wake word");
                };
                println!(
                    ">>> captured {:.2} s of request",
                    audio.len() as f32 / 16_000.0
                );
                command = Some(audio);
            }
        }
        if command.is_some() {
            break;
        }
    }

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

    let Some(audio) = command else {
        println!("no wake word was heard, so there was no request to transcribe.");
        println!(
            "the small model made {checks} check(s); if that is zero it never saw enough \
             speech, and if it is non-zero the wake word is in there but not being recognised"
        );
        println!("done.");
        return Ok(());
    };

    let transcript = request_model
        .as_deref()
        .context("a captured request without a loaded model")?
        .transcribe(&audio)?;
    println!("request heard: {transcript:?}");
    match parser.parse_with_reason(&transcript) {
        Ok(request) => println!("play: {}", request.query),
        Err(miss) => println!("not a play command: {miss}"),
    }
    println!("done.");
    Ok(())
}

/// Builds one speech-to-text engine.
fn build(model: &str, language: Option<&str>) -> anyhow::Result<std::sync::Arc<dyn Transcriber>> {
    Ok(std::sync::Arc::new(
        shamash::direct::DirectTranscriber::new(model, language)?,
    ))
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
