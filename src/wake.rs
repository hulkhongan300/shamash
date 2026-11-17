//! Listening: one wake gate per speaker, and the larger model it wakes.
//!
//! A small model checks a rolling window for the wake word mid-utterance; the
//! **whole** utterance, quiet frames included, then goes to the larger model so
//! words said in the same breath aren't lost. "groovy play" alone arms the
//! speaker, so their next utterance is the request; see [`Armed`].

use crate::config::AsrEngine;
use crate::direct::DirectTranscriber;
use crate::parakeet::HandyTranscriber;
use crate::parser::CommandParser;
use crate::segment::{SAMPLE_RATE, Segment, Segmenter, VoiceDetector};
use crate::transcriber::Transcriber;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;

/// A voice stream's id within the call (an RTP SSRC). One per speaker.
pub type Ssrc = u32;

/// One tick of every speaker's samples; silent speakers get silence so utterances can end.
pub type Tick = Vec<(Ssrc, Vec<f32>)>;

/// How much audio the small model looks at for the wake word.
pub const WAKE_WINDOW: Duration = Duration::from_millis(2000);

/// How often the small model runs while somebody speaks; the always-on cost.
pub const WAKE_HOP: Duration = Duration::from_millis(250);

/// How long a speaker who said only the wake word stays armed for the request.
pub const ARM_TTL: Duration = Duration::from_secs(8);

fn samples(duration: Duration) -> usize {
    (duration.as_secs_f32() * SAMPLE_RATE as f32) as usize
}

/// Something the pipeline has to act on.
#[derive(Debug)]
pub enum Heard {
    /// Wake word heard mid-utterance; acknowledge now, the utterance follows.
    Wake { ssrc: Ssrc },
    /// A finished utterance worth transcribing with the larger model.
    Utterance {
        ssrc: Ssrc,
        audio: Vec<f32>,
        woke: bool,
        /// Follows a bare wake word, so it is the request without one.
        armed: bool,
    },
}

/// What one push asked the caller to do.
#[derive(Debug)]
pub enum Step {
    /// Run the small model on `window`, then call [`WakeGate::transcribed`] with `closing`.
    Check {
        ssrc: Ssrc,
        window: Vec<f32>,
        /// Set on the final check of an utterance no rolling check woke on.
        closing: Option<Vec<f32>>,
    },
    Heard(Heard),
}

/// Speakers who said a bare wake word; their next utterance is the request.
#[derive(Debug, Clone, Default)]
pub struct Armed(Arc<Mutex<HashMap<Ssrc, Instant>>>);

impl Armed {
    /// Arms `ssrc` until `ARM_TTL` from now, returning that deadline.
    pub fn arm(&self, ssrc: Ssrc) -> Instant {
        let deadline = Instant::now() + ARM_TTL;
        self.lock().insert(ssrc, deadline);
        deadline
    }

    /// Whether `ssrc` is armed, without consuming it.
    pub fn is_armed(&self, ssrc: Ssrc) -> bool {
        self.lock()
            .get(&ssrc)
            .is_some_and(|deadline| Instant::now() < *deadline)
    }

    /// Consumes the arming, returning whether it was still live.
    pub fn take(&self, ssrc: Ssrc) -> bool {
        self.lock()
            .remove(&ssrc)
            .is_some_and(|deadline| Instant::now() < deadline)
    }

    /// Disarms only if still armed with `deadline`, so an old timer can't cancel a newer arming.
    pub fn expire(&self, ssrc: Ssrc, deadline: Instant) -> bool {
        let mut all = self.lock();
        if all.get(&ssrc) == Some(&deadline) {
            all.remove(&ssrc);
            true
        } else {
            false
        }
    }

    pub fn clear(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Ssrc, Instant>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct Speaker {
    segmenter: Segmenter,
    since_check: usize,
    woke: bool,
}

/// Watches every speaker for the wake word and hands over their utterances.
pub struct WakeGate {
    transcriber: Arc<dyn Transcriber>,
    parser: CommandParser,
    armed: Armed,
    speakers: HashMap<Ssrc, Speaker>,
    detector: fn() -> Box<dyn VoiceDetector>,
}

impl WakeGate {
    pub fn new(transcriber: Arc<dyn Transcriber>, parser: CommandParser, armed: Armed) -> Self {
        Self::with_detector(transcriber, parser, armed, || {
            Box::new(crate::segment::Earshot::default())
        })
    }

    /// As [`Self::new`], with a chosen voice detector. For tests.
    pub fn with_detector(
        transcriber: Arc<dyn Transcriber>,
        parser: CommandParser,
        armed: Armed,
        detector: fn() -> Box<dyn VoiceDetector>,
    ) -> Self {
        Self {
            transcriber,
            parser,
            armed,
            speakers: HashMap::new(),
            detector,
        }
    }

    /// The small model, for a caller that runs it off the blocking pool.
    pub fn transcriber(&self) -> Arc<dyn Transcriber> {
        Arc::clone(&self.transcriber)
    }

    /// Forgets every speaker's audio, as when listening is locked.
    pub fn reset(&mut self) {
        self.speakers.clear();
        self.armed.clear();
    }

    /// Feeds one speaker's samples; returns what to do about them, in order.
    pub fn push(&mut self, ssrc: Ssrc, audio: &[f32]) -> Vec<Step> {
        let detector = self.detector;
        let speaker = self.speakers.entry(ssrc).or_insert_with(|| Speaker {
            segmenter: Segmenter::new(detector()),
            since_check: 0,
            woke: false,
        });

        let mut steps = Vec::new();
        for event in speaker.segmenter.push(audio) {
            match event {
                Segment::Started => {
                    speaker.since_check = 0;
                    speaker.woke = false;
                }
                Segment::Ended(utterance) => {
                    let woke = std::mem::take(&mut speaker.woke);
                    if self.armed.take(ssrc) || woke {
                        let armed = !woke;
                        steps.push(Step::Heard(Heard::Utterance {
                            ssrc,
                            audio: utterance,
                            woke,
                            armed,
                        }));
                    } else {
                        // Rolling checks may have missed a wake word at the very end.
                        steps.push(Step::Check {
                            ssrc,
                            window: tail(&utterance, samples(WAKE_WINDOW)),
                            closing: Some(utterance),
                        });
                    }
                }
            }
        }

        let checking = speaker.segmenter.in_speech() && !speaker.woke;
        if checking && !self.armed.is_armed(ssrc) {
            speaker.since_check += audio.len();
            let current = speaker.segmenter.current();
            if speaker.since_check >= samples(WAKE_HOP) && current.len() >= samples(WAKE_HOP) {
                speaker.since_check = 0;
                steps.push(Step::Check {
                    ssrc,
                    window: tail(current, samples(WAKE_WINDOW)),
                    closing: None,
                });
            }
        }
        steps
    }

    /// The small model's verdict on a window offered by [`Step::Check`].
    pub fn transcribed(
        &mut self,
        ssrc: Ssrc,
        transcript: &str,
        closing: Option<Vec<f32>>,
    ) -> Vec<Heard> {
        tracing::debug!(ssrc, transcript = %transcript, "wake model heard");
        if !self.parser.wakes_on(transcript) {
            return Vec::new();
        }
        match closing {
            Some(audio) => vec![
                Heard::Wake { ssrc },
                Heard::Utterance {
                    ssrc,
                    audio,
                    woke: true,
                    armed: false,
                },
            ],
            None => match self.speakers.get_mut(&ssrc) {
                Some(speaker) if speaker.segmenter.in_speech() && !speaker.woke => {
                    speaker.woke = true;
                    vec![Heard::Wake { ssrc }]
                }
                _ => Vec::new(),
            },
        }
    }
}

fn tail(audio: &[f32], len: usize) -> Vec<f32> {
    audio[audio.len().saturating_sub(len)..].to_vec()
}

/// Consumes ticks until the channel closes, calling `on_heard` for each event.
///
/// While `locked()`, audio and speaker state are dropped. `on_heard` must not block.
pub async fn run_wake_gate(
    mut gate: WakeGate,
    mut rx: UnboundedReceiver<Tick>,
    locked: impl Fn() -> bool,
    mut on_heard: impl FnMut(Heard),
) {
    let mut was_locked = false;
    while let Some(tick) = rx.recv().await {
        if locked() {
            if !was_locked {
                gate.reset();
                was_locked = true;
            }
            continue;
        }
        was_locked = false;

        for (ssrc, audio) in tick {
            for step in gate.push(ssrc, &audio) {
                let (window, closing) = match step {
                    Step::Heard(heard) => {
                        on_heard(heard);
                        continue;
                    }
                    Step::Check {
                        window, closing, ..
                    } => (window, closing),
                };

                let transcriber = gate.transcriber();
                let inferred =
                    tokio::task::spawn_blocking(move || transcriber.transcribe(&window)).await;
                let transcript = match inferred {
                    Ok(Ok(text)) => text,
                    Ok(Err(e)) => {
                        tracing::warn!(error = %format!("{e:#}"), "wake word transcription failed");
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "wake word transcription task failed");
                        continue;
                    }
                };
                for heard in gate.transcribed(ssrc, &transcript, closing) {
                    on_heard(heard);
                }
            }
        }
    }
}

/// The larger model, loaded on first wake word then kept; the load blocks.
pub struct BigModels {
    engine: AsrEngine,
    model: String,
    language: Option<String>,
    prompt: Option<String>,
    loaded: Mutex<Option<Arc<dyn Transcriber>>>,
}

impl BigModels {
    pub fn new(
        engine: AsrEngine,
        model: &str,
        language: Option<&str>,
        prompt: Option<&str>,
    ) -> Self {
        Self {
            engine,
            model: model.to_string(),
            language: language.map(str::to_string),
            prompt: prompt.map(str::to_string),
            loaded: Mutex::new(None),
        }
    }

    /// The loaded model; the lock spans the load so preload and request don't both load.
    pub fn get_or_load(&self) -> anyhow::Result<Arc<dyn Transcriber>> {
        let mut slot = self.lock();
        if let Some(loaded) = slot.as_ref() {
            return Ok(Arc::clone(loaded));
        }
        let transcriber: Arc<dyn Transcriber> = match self.engine {
            AsrEngine::Direct => Arc::new(DirectTranscriber::new(
                &self.model,
                self.language.as_deref(),
                self.prompt.as_deref(),
            )?),
            AsrEngine::Handy => Arc::new(HandyTranscriber::new(&self.model)),
        };
        *slot = Some(Arc::clone(&transcriber));
        Ok(transcriber)
    }

    /// Whether the weights are in memory already.
    pub fn is_loaded(&self) -> bool {
        self.loaded
            .try_lock()
            .map(|slot| slot.is_some())
            .unwrap_or(false)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Arc<dyn Transcriber>>> {
        self.loaded.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl std::fmt::Debug for BigModels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BigModels")
            .field("model", &self.model)
            .field("loaded", &self.is_loaded())
            .finish()
    }
}

#[cfg(test)]
#[path = "tests/wake.rs"]
mod tests;
