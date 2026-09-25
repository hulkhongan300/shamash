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
            .map(|_| {
                println!("voice diag: handler installed, expecting voice ticks");
                TickDiag::new(Instant::now())
            });
        Self {
            tx,
            diag: Mutex::new(diag),
        }
    }

    /// Records one voice tick and prints a periodic report when `VOICE_DIAG` is
    /// set.
    fn record(&self, mixed: &[f32]) {
        let Ok(mut diag) = self.diag.lock() else {
            return;
        };
        let Some(diag) = diag.as_mut() else {
            return;
        };
        if diag.ticks == 0 {
            println!("voice diag: first voice tick received");
        }
        let now = Instant::now();
        diag.observe(mixed);
        if let Some(report) = diag.report(now, DIAG_INTERVAL) {
            println!("{report}");
        }
    }
}

/// How often [`TickDiag::report`] emits a line.
const DIAG_INTERVAL: Duration = Duration::from_secs(5);

impl TickDiag {
    fn new(since: Instant) -> Self {
        Self {
            since,
            ticks: 0,
            frames: 0,
            peak: 0.0,
            frame_len: 0,
        }
    }

    /// Folds one tick into the counters. An empty frame is a tick that carried
    /// no audio.
    fn observe(&mut self, frame: &[f32]) {
        self.ticks += 1;
        if !frame.is_empty() {
            self.frames += 1;
            self.frame_len = frame.len();
            self.peak = self.peak.max(rms(frame));
        }
    }

    /// Returns a report once `interval` has passed, and resets the counters.
    ///
    /// The report is driven by ticks, not by audio, and that is the whole
    /// point: a report that only appears when audio arrives cannot distinguish
    /// a bot that receives nothing from one that receives silence, and those
    /// are exactly the two cases worth telling apart.
    fn report(&mut self, now: Instant, interval: Duration) -> Option<String> {
        let elapsed = now.saturating_duration_since(self.since);
        if elapsed < interval {
            return None;
        }
        let seconds = elapsed.as_secs_f32().max(f32::EPSILON);
        let mut report = format!(
            "voice diag: {} ticks and {} audio frames in {seconds:.0}s ({:.0} frames/s), \
             {}/frame, peak rms {:.4} ({:.1} dBFS) vs gate {VAD_RMS_THRESHOLD:.4}",
            self.ticks,
            self.frames,
            self.frames as f32 / seconds,
            self.frame_len,
            self.peak,
            to_dbfs(self.peak),
        );
        if self.ticks == 0 {
            report.push_str(
                "\n  no voice ticks at all: the call is not connected to Discord's voice server",
            );
        } else if self.frames == 0 {
            report.push_str(
                "\n  ticks are arriving but carry no audio. Either nobody is speaking, or \
                 Discord's end-to-end voice encryption (DAVE) has not finished negotiating, \
                 in which case nothing can be decoded. Speaking once the bot has been in the \
                 channel a while, or rejoining the channel, usually settles it.",
            );
        } else if self.peak < VAD_RMS_THRESHOLD {
            report.push_str(
                "\n  audio is arriving below the gate: raise the microphone gain or lower \
                 VAD_RMS_THRESHOLD",
            );
        }
        *self = Self::new(now);
        Some(report)
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

    /// A 20 ms frame at 16 kHz, quiet enough to sit below the gate.
    fn quiet_frame() -> Vec<f32> {
        vec![0.01; FRAME_SAMPLES]
    }

    /// A 20 ms frame at 16 kHz at a normal speaking level.
    fn speech_frame() -> Vec<f32> {
        vec![0.2; FRAME_SAMPLES]
    }

    #[test]
    fn diag_counts_a_tick_that_carries_no_audio() {
        let mut diag = TickDiag::new(Instant::now());
        for _ in 0..50 {
            diag.observe(&[]);
        }
        assert_eq!(diag.ticks, 50);
        assert_eq!(diag.frames, 0, "an empty frame is a tick, not audio");
    }

    #[test]
    fn diag_records_frame_length_and_peak() {
        let mut diag = TickDiag::new(Instant::now());
        diag.observe(&speech_frame());
        assert_eq!(diag.frames, 1);
        assert_eq!(diag.frame_len, FRAME_SAMPLES);
        assert!((diag.peak - 0.2).abs() < 1e-3);
    }

    #[test]
    fn diag_stays_quiet_until_the_interval_passes() {
        let start = Instant::now();
        let mut diag = TickDiag::new(start);
        diag.observe(&speech_frame());
        assert!(
            diag.report(start + Duration::from_secs(4), DIAG_INTERVAL)
                .is_none(),
            "must not report before the interval"
        );
        let report = diag
            .report(start + DIAG_INTERVAL, DIAG_INTERVAL)
            .expect("reports once the interval passes");
        assert!(report.contains("1 ticks and 1 audio frames"), "{report}");
    }

    #[test]
    fn diag_names_the_three_ways_audio_can_be_missing() {
        let start = Instant::now();

        // No ticks at all: nothing is driving the handler.
        let mut diag = TickDiag::new(start);
        let report = diag.report(start + DIAG_INTERVAL, DIAG_INTERVAL).unwrap();
        assert!(report.contains("no voice ticks at all"), "{report}");

        // Ticks but no frames: the decoder is producing nothing.
        let mut diag = TickDiag::new(start);
        for _ in 0..50 {
            diag.observe(&[]);
        }
        let report = diag.report(start + DIAG_INTERVAL, DIAG_INTERVAL).unwrap();
        assert!(
            report.contains("ticks are arriving but carry no audio"),
            "{report}"
        );

        // Frames below the gate: audio is there, the gate is too high.
        let mut diag = TickDiag::new(start);
        for _ in 0..50 {
            diag.observe(&quiet_frame());
        }
        let report = diag.report(start + DIAG_INTERVAL, DIAG_INTERVAL).unwrap();
        assert!(report.contains("below the gate"), "{report}");

        // Frames above the gate: working.
        let mut diag = TickDiag::new(start);
        for _ in 0..50 {
            diag.observe(&speech_frame());
        }
        let report = diag.report(start + DIAG_INTERVAL, DIAG_INTERVAL).unwrap();
        assert!(!report.contains("below the gate"), "{report}");
        assert!(!report.contains("no voice ticks"), "{report}");
        assert!(!report.contains("carry no audio"), "{report}");
    }

    #[test]
    fn diag_resets_counters_after_reporting() {
        let start = Instant::now();
        let mut diag = TickDiag::new(start);
        diag.observe(&speech_frame());
        diag.report(start + DIAG_INTERVAL, DIAG_INTERVAL).unwrap();
        assert_eq!(diag.ticks, 0);
        assert_eq!(diag.frames, 0);
        assert_eq!(diag.peak, 0.0);
    }

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
