//! Listening: one wake gate per speaker, and the larger model it wakes.
//!
//! Each speaker's audio is cut into whole utterances by [`crate::segment`].
//! While an utterance is in progress the small, always-resident model checks
//! its most recent two seconds every half second for the wake word, so the
//! speaker hears the acknowledgement while they are still talking. When the
//! utterance ends, the **whole** of it — the wake word, whatever followed it in
//! the same breath, and every quiet frame in between — goes to the larger
//! model once.
//!
//! That is what fixes the two ways this used to lose requests. The old gate
//! started recording only after the wake word was recognised, so the words
//! said in the same breath ("bot, play Drac…") were already gone; and it kept
//! only frames above a loudness gate, so what it did record had its quiet parts
//! spliced out.
//!
//! A wake word said on its own ("bot", pause) leaves the speaker *armed*: the
//! pipeline plays the ready tone, and the speaker's next utterance is taken as
//! the request without needing the wake word again. See [`Armed`].

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

/// One tick of audio: every speaker's samples for it. A speaker the call knows
/// about but who sent nothing this tick is given silence, so their utterance
/// can end.
pub type Tick = Vec<(Ssrc, Vec<f32>)>;

/// How much audio the small model looks at for the wake word.
pub const WAKE_WINDOW: Duration = Duration::from_millis(2000);

/// How often the small model runs while somebody is speaking.
///
/// This is the always-on stage's whole cost: one inference on the small model
/// per hop per speaker.
pub const WAKE_HOP: Duration = Duration::from_millis(250);

/// How long a speaker who said only the wake word stays armed for the request.
pub const ARM_TTL: Duration = Duration::from_secs(8);

fn samples(duration: Duration) -> usize {
    (duration.as_secs_f32() * SAMPLE_RATE as f32) as usize
}

/// Something the pipeline has to act on.
#[derive(Debug)]
pub enum Heard {
    /// The small model heard the wake word in an utterance still in progress.
    /// Acknowledge now; the utterance itself follows.
    Wake { ssrc: Ssrc },
    /// A finished utterance worth transcribing with the larger model.
    Utterance {
        ssrc: Ssrc,
        audio: Vec<f32>,
        /// The small model heard the wake word in it.
        woke: bool,
        /// The speaker had said the wake word on its own just before, so this
        /// is the request even if it carries no wake word.
        armed: bool,
    },
}

/// What one push asked the caller to do.
#[derive(Debug)]
pub enum Step {
    /// Run the small model on `window` and hand the text to
    /// [`WakeGate::transcribed`], with `closing` passed back unchanged.
    Check {
        ssrc: Ssrc,
        window: Vec<f32>,
        /// The finished utterance, when this is the final check on one that
        /// no rolling check had woken on.
        closing: Option<Vec<f32>>,
    },
    /// Deliver this to the pipeline.
    Heard(Heard),
}

/// Speakers who said only the wake word and whose next utterance is the
/// request. Shared between the gate, which consumes it, and the pipeline,
/// which sets it once the larger model has heard a bare wake word.
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

    /// Disarms `ssrc` only if it is still armed with `deadline`, so a later
    /// arming is not cancelled by an earlier one's timer. Returns whether it
    /// was.
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

/// One speaker's state.
struct Speaker {
    segmenter: Segmenter,
    /// Samples since the last rolling check.
    since_check: usize,
    /// The wake word was heard in the utterance in progress.
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
                        // The rolling checks may not have covered the last
                        // stretch, and a wake word at the very end of what was
                        // said is as valid as one at the start.
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
        if !self.parser.contains_wake_word(transcript) {
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

/// The last `len` samples of `audio`.
fn tail(audio: &[f32], len: usize) -> Vec<f32> {
    audio[audio.len().saturating_sub(len)..].to_vec()
}

/// Consumes ticks until the channel closes, calling `on_heard` for each event.
///
/// `locked` is asked on every tick: while it is true the audio is dropped and
/// every speaker's state forgotten, so nothing said while listening was locked
/// can be acted on once it is unlocked.
///
/// `on_heard` must not block: it hands the event to the pipeline's own task, so
/// a request that takes seconds to resolve never delays hearing the next one.
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

/// The larger model, loaded on the first wake word rather than at startup, then
/// kept resident.
///
/// The first load blocks, so it belongs on a blocking pool.
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

    /// The loaded model, loading it on the first call.
    ///
    /// The lock is held across the load, so the preload started by a wake word
    /// and the request that follows it cannot both load the weights.
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
mod tests {
    use super::*;
    use crate::segment::tests::{Marked, quiet, voice};

    /// A model that answers every check with the same text.
    struct FixedText(&'static str);

    impl Transcriber for FixedText {
        fn sample_rate(&self) -> u32 {
            16_000
        }
        fn transcribe(&self, _samples: &[f32]) -> anyhow::Result<String> {
            Ok(self.0.to_string())
        }
    }

    fn parser() -> CommandParser {
        CommandParser::new(["shamash".to_string(), "bot".to_string()])
    }

    fn gate(armed: &Armed) -> WakeGate {
        WakeGate::with_detector(Arc::new(FixedText("")), parser(), armed.clone(), || {
            Box::new(Marked)
        })
    }

    fn checks(steps: &[Step]) -> usize {
        steps
            .iter()
            .filter(|s| matches!(s, Step::Check { closing: None, .. }))
            .count()
    }

    #[test]
    fn silence_is_never_offered_for_checking() {
        let armed = Armed::default();
        let mut gate = gate(&armed);
        assert!(gate.push(1, &quiet(300)).is_empty());
    }

    #[test]
    fn speech_is_checked_on_a_rolling_window() {
        let armed = Armed::default();
        let mut gate = gate(&armed);
        let steps: Vec<Step> = voice(200)
            .chunks(320)
            .flat_map(|c| gate.push(1, c))
            .collect();
        // 200 x 16 ms = 3.2 s of speech, one check per 500 ms.
        assert!(checks(&steps) >= 5, "{} checks", checks(&steps));
        for step in &steps {
            if let Step::Check { window, .. } = step {
                assert!(window.len() <= samples(WAKE_WINDOW));
            }
        }
    }

    /// The fix for losing the words said with the wake word: the utterance
    /// handed over is the whole thing, from before the wake word to the end.
    #[test]
    fn a_woken_utterance_is_delivered_whole() {
        let armed = Armed::default();
        let mut gate = gate(&armed);
        gate.push(1, &voice(40));
        let heard = gate.transcribed(1, "bot play dracula", None);
        assert!(matches!(heard[..], [Heard::Wake { ssrc: 1 }]));

        gate.push(1, &voice(100));
        let steps = gate.push(1, &quiet(50));
        let audio = steps
            .into_iter()
            .find_map(|s| match s {
                Step::Heard(Heard::Utterance {
                    audio, woke: true, ..
                }) => Some(audio),
                _ => None,
            })
            .expect("the utterance");
        assert!(audio.len() >= 140 * crate::segment::VAD_FRAME);
    }

    #[test]
    fn ordinary_speech_is_not_delivered() {
        let armed = Armed::default();
        let mut gate = gate(&armed);
        gate.push(1, &voice(40));
        assert!(gate.transcribed(1, "what is for lunch", None).is_empty());
        let steps = gate.push(1, &quiet(50));
        // Only a final check on the tail, which also finds nothing.
        let Some(Step::Check {
            closing: Some(audio),
            window,
            ..
        }) = steps.into_iter().next()
        else {
            panic!("expected a closing check");
        };
        assert!(window.len() <= samples(WAKE_WINDOW));
        assert!(
            gate.transcribed(1, "what is for lunch", Some(audio))
                .is_empty()
        );
    }

    #[test]
    fn a_wake_word_at_the_very_end_is_caught_by_the_closing_check() {
        let armed = Armed::default();
        let mut gate = gate(&armed);
        gate.push(1, &voice(40));
        let steps = gate.push(1, &quiet(50));
        let Some(Step::Check {
            closing: Some(audio),
            ..
        }) = steps.into_iter().next()
        else {
            panic!("expected a closing check");
        };
        let heard = gate.transcribed(1, "okay then shamash", Some(audio));
        assert!(matches!(
            heard[..],
            [
                Heard::Wake { .. },
                Heard::Utterance {
                    woke: true,
                    armed: false,
                    ..
                }
            ]
        ));
    }

    #[test]
    fn one_utterance_wakes_only_once() {
        let armed = Armed::default();
        let mut gate = gate(&armed);
        gate.push(1, &voice(40));
        assert_eq!(gate.transcribed(1, "bot", None).len(), 1);
        assert!(gate.transcribed(1, "bot", None).is_empty());
        // And no more rolling checks once woken.
        assert_eq!(checks(&gate.push(1, &voice(100))), 0);
    }

    #[test]
    fn an_armed_speaker_is_delivered_without_a_wake_word() {
        let armed = Armed::default();
        let mut gate = gate(&armed);
        armed.arm(7);
        let steps = gate.push(7, &voice(60));
        assert_eq!(checks(&steps), 0, "an armed speaker is not wake-checked");
        let steps = gate.push(7, &quiet(50));
        assert!(matches!(
            steps[..],
            [Step::Heard(Heard::Utterance {
                armed: true,
                woke: false,
                ..
            })]
        ));
        assert!(!armed.is_armed(7), "arming is consumed");
    }

    #[test]
    fn speakers_are_kept_apart() {
        let armed = Armed::default();
        let mut gate = gate(&armed);
        gate.push(1, &voice(40));
        gate.push(2, &voice(40));
        gate.transcribed(1, "bot", None);
        let other = gate.push(2, &quiet(50));
        assert!(
            other
                .iter()
                .all(|s| !matches!(s, Step::Heard(Heard::Utterance { .. }))),
            "speaker 2 never said the wake word"
        );
        let first = gate.push(1, &quiet(50));
        assert!(matches!(
            first[..],
            [Step::Heard(Heard::Utterance { ssrc: 1, .. })]
        ));
    }

    #[test]
    fn an_expired_arming_is_not_live() {
        let armed = Armed::default();
        let deadline = armed.arm(3);
        assert!(armed.expire(3, deadline));
        assert!(!armed.take(3));
        // A newer arming survives an older timer.
        let old = Instant::now();
        armed.arm(3);
        assert!(!armed.expire(3, old));
        assert!(armed.is_armed(3));
    }

    #[tokio::test]
    async fn the_loop_drops_audio_while_locked() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Tick>();
        let heard = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&heard);
        let armed = Armed::default();
        let gate = WakeGate::with_detector(
            Arc::new(FixedText("bot play dracula")),
            parser(),
            armed,
            || Box::new(Marked),
        );
        let handle = tokio::spawn(run_wake_gate(
            gate,
            rx,
            || true,
            move |h| seen.lock().unwrap().push(format!("{h:?}").len()),
        ));
        tx.send(vec![(1, voice(100))]).unwrap();
        tx.send(vec![(1, quiet(60))]).unwrap();
        drop(tx);
        handle.await.unwrap();
        assert!(heard.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_loop_delivers_the_wake_before_the_utterance() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Tick>();
        let order = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&order);
        let gate = WakeGate::with_detector(
            Arc::new(FixedText("bot play dracula")),
            parser(),
            Armed::default(),
            || Box::new(Marked),
        );
        let handle = tokio::spawn(run_wake_gate(
            gate,
            rx,
            || false,
            move |h| {
                seen.lock().unwrap().push(match h {
                    Heard::Wake { .. } => "wake",
                    Heard::Utterance { .. } => "utterance",
                })
            },
        ));
        for chunk in voice(100).chunks(320) {
            tx.send(vec![(1, chunk.to_vec())]).unwrap();
        }
        tx.send(vec![(1, quiet(60))]).unwrap();
        drop(tx);
        handle.await.unwrap();
        assert_eq!(order.lock().unwrap().clone(), vec!["wake", "utterance"]);
    }
}
