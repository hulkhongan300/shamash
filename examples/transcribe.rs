//! Offline rehearsal of the listening pipeline.
//!
//!     cargo run --release --example transcribe -- path/to/recording.wav
//!
//! Feeds a WAV (16-bit PCM, mono or stereo, any sample rate) through the real
//! [`WakeGate`] rather than a copy of it: resampled to 16 kHz and fed in 20 ms
//! ticks as Discord delivers it, with the real voice detector cutting it into
//! utterances and the small model checking each for the wake word. Every
//! utterance the gate hands over is transcribed by the larger model and run
//! through the same grammar the bot uses, including the case where the wake
//! word was said on its own and the request follows in the next utterance.
//!
//! Models come from the same settings as the bot: `WAKE_MODEL` for the small
//! one, `REQUEST_MODEL` for the large, `WAKE_WORDS` for what counts as a wake
//! word, `ASR_LANGUAGE` and `ASR_PROMPT`.

use anyhow::Context;
use shamash::audio::{Resampler, rms};
use shamash::config::Config;
use shamash::listener::{FRAME_SAMPLES, QUIET_SPEECH_RMS, to_dbfs};
use shamash::parser::CommandParser;
use shamash::pipeline::{Understood, understand};
use shamash::transcriber::Transcriber;
use shamash::wake::{Armed, Heard, Step, WakeGate};
use std::path::Path;
use std::time::Instant;

/// The one speaker a recording has.
const SSRC: u32 = 1;

fn main() -> anyhow::Result<()> {
    let wav_path = std::env::args()
        .nth(1)
        .context("usage: transcribe <recording.wav>")?;
    let _ = dotenvy::dotenv();

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
    let mut sorted: Vec<&str> = config.wake_words.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    println!("wake model:    {}", config.wake_model);
    println!("request model: {}", config.request_model);
    println!("wake words:    {}", sorted.join(", "));

    let (raw, rate) = load_wav(Path::new(&wav_path))?;
    let samples = Resampler::new(rate, 16_000).resample(&raw);
    println!(
        "audio: {:.2} s at {rate} Hz",
        raw.len() as f32 / rate as f32
    );

    let peak = samples
        .chunks(FRAME_SAMPLES)
        .map(rms)
        .fold(0.0f32, f32::max);
    println!(
        "loudest 20 ms frame: {:.1} dBFS{}",
        to_dbfs(peak),
        if peak < QUIET_SPEECH_RMS {
            " -- very quiet; raise the microphone gain"
        } else {
            ""
        }
    );

    let language = config.asr_language.as_deref();
    let wake_model = build(&config.wake_model, language, None)?;
    let parser = CommandParser::new(config.wake_words.clone());
    let armed = Armed::default();
    let mut gate = WakeGate::new(wake_model.clone(), parser.clone(), armed.clone());
    let mut request_model: Option<std::sync::Arc<dyn Transcriber>> = None;

    // Two seconds of silence after the recording, so its last utterance ends.
    let ticks = samples
        .chunks(FRAME_SAMPLES)
        .map(<[f32]>::to_vec)
        .chain(std::iter::repeat_n(vec![0.0f32; FRAME_SAMPLES], 100));

    let (mut checks, mut utterances) = (0usize, 0usize);
    for (at, tick) in ticks.enumerate() {
        let seconds = at as f32 * FRAME_SAMPLES as f32 / 16_000.0;
        let mut heard = Vec::new();
        for step in gate.push(SSRC, &tick) {
            match step {
                Step::Heard(h) => heard.push(h),
                Step::Check {
                    window, closing, ..
                } => {
                    checks += 1;
                    let text = wake_model.transcribe(&window)?;
                    println!("  [{seconds:6.2}s] wake model heard: {text:?}");
                    heard.extend(gate.transcribed(SSRC, &text, closing));
                }
            }
        }
        for h in heard {
            match h {
                Heard::Wake { .. } => println!(">>> [{seconds:6.2}s] wake word heard (tone 1)"),
                Heard::Utterance {
                    audio,
                    armed: was_armed,
                    ..
                } => {
                    utterances += 1;
                    let model = match &request_model {
                        Some(model) => model.clone(),
                        None => {
                            let loading = Instant::now();
                            let model = build(
                                &config.request_model,
                                language,
                                config.asr_prompt.as_deref(),
                            )?;
                            println!("larger model ready in {:?}", loading.elapsed());
                            request_model = Some(model.clone());
                            model
                        }
                    };
                    let started = Instant::now();
                    let transcript = model.transcribe(&audio)?;
                    println!(
                        ">>> utterance of {:.2} s transcribed in {:?}: {transcript:?}",
                        audio.len() as f32 / 16_000.0,
                        started.elapsed()
                    );
                    match understand(&parser, &transcript, was_armed) {
                        Understood::Request(request) => println!("play: {}", request.query),
                        Understood::WakeOnly => {
                            armed.arm(SSRC);
                            println!("wake word on its own: waiting for the request (tone 2)");
                        }
                        Understood::Nothing(miss) => println!("not a play command: {miss:?}"),
                    }
                }
            }
        }
    }

    if utterances == 0 {
        println!(
            "no utterance was handed to the larger model after {checks} wake check(s); \
             zero checks means the voice detector heard no speech, and a non-zero count \
             means the wake word is not being recognised (see WAKE_WORDS)"
        );
    }
    println!("done.");
    Ok(())
}

/// Builds one speech-to-text engine.
fn build(
    model: &str,
    language: Option<&str>,
    prompt: Option<&str>,
) -> anyhow::Result<std::sync::Arc<dyn Transcriber>> {
    Ok(std::sync::Arc::new(
        shamash::direct::DirectTranscriber::new(model, language, prompt)?,
    ))
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
