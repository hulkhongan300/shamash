use crate::audio::VadBuffer;
use serenity::async_trait;
use songbird::EventHandler;
use songbird::events::{CoreEvent, Event, EventContext};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Sample rate (Hz) of the mono audio stream the listener consumes.
pub const LISTEN_SAMPLE_RATE: u32 = 16_000;
/// Samples per 20 ms frame at [`LISTEN_SAMPLE_RATE`].
pub const FRAME_SAMPLES: usize = 320;

/// Accumulates received frames into complete speech utterances.
#[derive(Debug)]
pub struct Listener {
    vad: VadBuffer,
}

impl Listener {
    pub fn new() -> Self {
        Self {
            // RMS threshold ~0.02, 300 ms of silence ends an utterance,
            // and a 12 s cap bounds utterances that never hit a pause.
            vad: VadBuffer::new(FRAME_SAMPLES, 0.02, 15, 600),
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
}

impl VoiceTickHandler {
    pub fn new(tx: UnboundedSender<Vec<f32>>) -> Self {
        Self { tx }
    }
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
            if !frames.is_empty() {
                let _ = self.tx.send(mix_frames(&frames));
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
