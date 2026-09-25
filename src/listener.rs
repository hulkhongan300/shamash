use crate::audio::{VadBuffer, rms};
use serenity::async_trait;
use songbird::EventHandler;
use songbird::events::{CoreEvent, Event, EventContext};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Sample rate (Hz) of the mono audio stream the listener consumes.
pub const LISTEN_SAMPLE_RATE: u32 = 16_000;
/// Samples per 20 ms frame at [`LISTEN_SAMPLE_RATE`].
pub const FRAME_SAMPLES: usize = 320;

/// Frame [`rms`](crate::audio::rms) below which audio counts as silence.
///
/// 0.02 is -34 dBFS. Ordinary conversation sits near -20 dBFS and a quiet
/// voice or low microphone gain near -30 dBFS, so this keeps roughly 4 to 14 dB
/// of headroom above speech while staying well clear of room tone, which is
/// typically -45 dBFS or lower. Measure your own voice with
/// `cargo run --release --example transcribe -- recording.wav`, which prints
/// the peak frame level against this gate.
pub const VAD_RMS_THRESHOLD: f32 = 0.02;
/// Silence frames that end an utterance: 15 x 20 ms = 300 ms.
pub const VAD_GAP_FRAMES: usize = 15;
/// Hard cap on one utterance: 600 x 20 ms = 12 s.
pub const VAD_MAX_FRAMES: usize = 600;

/// Accumulates received frames into complete speech utterances.
#[derive(Debug)]
pub struct Listener {
    vad: VadBuffer,
}

impl Listener {
    pub fn new() -> Self {
        Self {
            vad: VadBuffer::new(
                FRAME_SAMPLES,
                VAD_RMS_THRESHOLD,
                VAD_GAP_FRAMES,
                VAD_MAX_FRAMES,
            ),
        }
    }

    /// Feeds one 20 ms mono frame; returns a completed utterance, if any.
    pub fn push_frame(&mut self, samples: &[f32]) -> Option<Vec<f32>> {
        let utterance = self.vad.push(samples);
        (!utterance.is_empty()).then_some(utterance)
    }
}

impl Default for Listener {
    fn default() -> Self {
        Self::new()
    }
}

/// Mixes per-speaker i16 frames into a single normalized f32 frame.
///
/// All frames must have the same length (they are always 20 ms at the
/// configured decode rate).
pub fn mix_frames(frames: &[&[i16]]) -> Vec<f32> {
    let len = frames.first().map_or(0, |f| f.len());
    let mut out = vec![0.0f32; len];
    for frame in frames {
        debug_assert_eq!(frame.len(), len, "speaker frame sizes must match");
        for (i, sample) in frame.iter().enumerate() {
            out[i] += *sample as f32 / 32768.0;
        }
    }
    out
}

/// Songbird event handler forwarding decoded voice frames to a listener task.
#[derive(Debug)]
pub struct VoiceTickHandler {
    tx: UnboundedSender<Vec<f32>>,
    diag: Mutex<Option<TickDiag>>,
}

/// Rolling counters behind the `VOICE_DIAG` report.
#[derive(Debug)]
struct TickDiag {
    since: Instant,
    ticks: u64,
    frames: u64,
    peak: f32,
    frame_len: usize,
}

impl VoiceTickHandler {
    pub fn new(tx: UnboundedSender<Vec<f32>>) -> Self {
        let diag = std::env::var("VOICE_DIAG")
            .ok()
            .filter(|v| !v.is_empty() && v != "0")
            .map(|_| TickDiag {
                since: Instant::now(),
                ticks: 0,
                frames: 0,
                peak: 0.0,
                frame_len: 0,
            });
        Self {
            tx,
            diag: Mutex::new(diag),
        }
    }

    /// Records one voice tick and prints a periodic report when `VOICE_DIAG` is
    /// set.
    ///
    /// The report is driven by ticks, not by audio, and that is the whole
    /// point: reporting only when frames arrive cannot distinguish a bot that
    /// receives nothing from one that receives silence, and those are exactly
    /// the two cases worth telling apart. A tick count of zero means the voice
    /// connection is not up; ticks with no frames means nothing is speaking or
    /// nothing is being decoded.
    fn record(&self, mixed: &[f32]) {
        let Ok(mut diag) = self.diag.lock() else {
            return;
        };
        let Some(diag) = diag.as_mut() else {
            return;
        };
        diag.ticks += 1;
        if !mixed.is_empty() {
            diag.frames += 1;
            diag.frame_len = mixed.len();
            diag.peak = diag.peak.max(rms(mixed));
        }
        let elapsed = diag.since.elapsed();
        if elapsed < Duration::from_secs(5) {
            return;
        }
        let seconds = elapsed.as_secs_f32();
        println!(
            "voice diag: {ticks} ticks and {frames} audio frames in {seconds:.0}s \
             ({frame_rate:.0} frames/s), {frame_len}/frame, peak rms {peak:.4} \
             ({peak_db:.1} dBFS) vs gate {VAD_RMS_THRESHOLD:.4}",
            ticks = diag.ticks,
            frames = diag.frames,
            frame_rate = diag.frames as f32 / seconds,
            frame_len = diag.frame_len,
            peak = diag.peak,
            peak_db = to_dbfs(diag.peak),
        );
        if diag.ticks == 0 {
            println!(
                "  no voice ticks at all: the call is not connected to Discord's voice server"
            );
        } else if diag.frames == 0 {
            println!(
                "  ticks are arriving but carry no audio: nobody is speaking, or the decoder is idle"
            );
        } else if diag.peak < VAD_RMS_THRESHOLD {
            println!(
                "  audio is arriving below the gate: raise the microphone gain or lower VAD_RMS_THRESHOLD"
            );
        }
        diag.since = Instant::now();
        diag.ticks = 0;
        diag.frames = 0;
        diag.peak = 0.0;
    }
}

/// Full-scale decibel level of a linear amplitude.
fn to_dbfs(amplitude: f32) -> f32 {
    20.0 * amplitude.max(1e-9).log10()
}

#[async_trait]
impl EventHandler for VoiceTickHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::VoiceTick(tick) = ctx {
            let frames: Vec<&[i16]> = tick
                .speaking
                .values()
                .filter_map(|data| data.decoded_voice.as_deref())
                .collect();
            let mixed = mix_frames(&frames);
            self.record(&mixed);
            if !mixed.is_empty() {
                let _ = self.tx.send(mixed);
            }
        }
        Some(CoreEvent::VoiceTick.into())
    }
}

/// Consumes frames until the channel closes, invoking `on_utterance` for each
/// completed utterance. The task exits once the voice call (and its sender)
/// is torn down.
pub async fn run_listener(mut rx: UnboundedReceiver<Vec<f32>>, on_utterance: impl Fn(Vec<f32>)) {
    let mut listener = Listener::new();
    while let Some(frame) = rx.recv().await {
        if let Some(utterance) = listener.push_frame(&frame) {
            on_utterance(utterance);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn mixes_multiple_speakers() {
        let left = vec![1000i16; 320];
        let right = vec![3000i16; 320];
        let mixed = mix_frames(&[&left, &right]);
        assert_eq!(mixed.len(), 320);
        let expected = (1000.0 + 3000.0) / 32768.0;
        assert!((mixed[0] - expected).abs() < 1e-6);
    }

    #[test]
    fn mixes_empty_frame_set() {
        assert!(mix_frames(&[]).is_empty());
    }

    #[test]
    fn isolates_utterances() {
        let mut listener = Listener::new();
        let loud = vec![0.5f32; 320];
        let silent = vec![0.0f32; 320];
        for _ in 0..5 {
            assert!(listener.push_frame(&loud).is_none());
        }
        for _ in 0..14 {
            assert!(listener.push_frame(&silent).is_none());
        }
        let utterance = listener.push_frame(&silent);
        assert_eq!(utterance.as_ref().map(Vec::len), Some(5 * 320));
    }

    #[tokio::test]
    async fn run_listener_yields_utterances() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<f32>>();
        let utterances: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = utterances.clone();
        let handle = tokio::spawn(async move {
            run_listener(rx, move |utterance| {
                seen.lock().unwrap().push(utterance.len());
            })
            .await;
        });

        let loud = vec![0.5f32; 320];
        let silent = vec![0.0f32; 320];
        for _ in 0..5 {
            let _ = tx.send(loud.clone());
        }
        for _ in 0..16 {
            let _ = tx.send(silent.clone());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(tx);
        let _ = handle.await;

        let got = utterances.lock().unwrap().clone();
        assert_eq!(got, vec![5 * 320]);
    }
}
