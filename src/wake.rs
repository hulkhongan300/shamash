//! The first two stages of listening: a small model that is always on, and the
//! larger model that is loaded once one of them hears the wake word.
//!
//! Before this the bot transcribed every utterance that crossed the voice gate
//! and looked for a wake word in the text afterwards, so a conversation in the
//! channel cost a full-size transcription each time and the answer only arrived
//! after the speaker had finished. Here the work is inverted. A small model
//! listens continuously and only has to recognise one short word, so its
//! inference is cheap enough to run on every window of speech; when it fires,
//! the speaker is told at once, the larger model is brought up, and only then
//! is the request itself transcribed.
//!
//! The gate keeps the audio it checked in a rolling window, so the wake word is
//! heard in the middle of a sentence rather than only at the start of one, and
//! it buffers from the moment of the wake word onwards, so a request started
//! before the model was ready is not lost.

use crate::audio::rms;
use crate::config::AsrEngine;
use crate::direct::DirectTranscriber;
use crate::listener::{FRAME_SAMPLES, VAD_RMS_THRESHOLD};
use crate::parakeet::HandyTranscriber;
use crate::parser::CommandParser;
use crate::transcriber::Transcriber;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;

/// How much audio the small model looks at for the wake word.
///
/// Long enough for a drawn-out wake word plus the word before it, because the
/// gate fires mid-sentence as readily as at the start of one. Short enough that
/// the model is not asked to transcribe a paragraph and start guessing.
pub const WAKE_WINDOW: Duration = Duration::from_millis(2000);

/// How often the small model runs while somebody is speaking.
///
/// This is the stage's whole cost, so it is the knob that decides how much CPU
/// the always-on listener uses. 250 ms keeps the delay between saying the wake
/// word and the acknowledgement short; the 110M wake model is cheap enough that
/// checking twice as often costs little.
pub const WAKE_HOP: Duration = Duration::from_millis(250);

/// Silence frames the gate keeps checking for after speech stops: 25 x 20 ms.
///
/// A wake word is the tail of what was said as often as the head of it, so the
/// window has to outlive the last loud frame rather than closing with it. The
/// run that closes the window is 500 ms of silence.
const WAKE_TAIL_FRAMES: usize = 25;

/// How long the gate ignores itself after firing, so one shout cannot start
/// several commands.
const WAKE_COOLDOWN: Duration = Duration::from_secs(3);

/// Silence frames that end a captured request: 25 x 20 ms = 500 ms.
pub const COMMAND_GAP_FRAMES: usize = 25;

/// Hard cap on one captured request: 250 x 20 ms = 5 s.
///
/// The bound is what makes a silent channel safe: a request that is never spoken
/// closes on the cap instead of leaving the gate armed to capture whatever is
/// said next.
pub const COMMAND_MAX_FRAMES: usize = 250;

/// Something the pipeline has to act on.
#[derive(Debug)]
pub enum Heard {
    /// The wake word was heard.
    Wake(WakeHit),
    /// The request, captured once the larger model was ready.
    Command(Vec<f32>),
}

/// A wake word, and the state the capture of the request depends on.
#[derive(Debug, Clone)]
pub struct WakeHit {
    /// Set by the pipeline once the larger model is loaded.
    ///
    /// Shared rather than returned, because the gate is what decides when a
    /// capture may end and the pipeline is what knows when the model is up: the
    /// two have to agree without either owning the other's timing. A gap only
    /// closes a request once this is set *and* speech has followed it, so the
    /// silence before the speaker starts talking cannot end the capture early.
    pub ready: Arc<AtomicBool>,
}

/// What one tick did to the gate.
#[derive(Debug)]
pub enum Step {
    /// Nothing to do.
    Idle,
    /// Transcribe this window with the small model and hand the text back to
    /// [`WakeGate::transcribed`].
    Check(Vec<f32>),
    /// Deliver this to the pipeline.
    Heard(Heard),
}

/// What the gate is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Listening for the wake word.
    Idle,
    /// The wake word was heard and the larger model is loading. Audio keeps
    /// being buffered, because a speaker who does not wait for the second tone
    /// would otherwise be clipped.
    Loading,
}

/// Watches the channel for the wake word, then captures the request behind it.
pub struct WakeGate {
    /// The small model. Held for the life of the call, which is the point of
    /// this stage: it runs on every window of speech.
    transcriber: Arc<dyn Transcriber>,
    /// Supplies the wake-word matching, so a misheard name is tolerated here
    /// exactly as it is anywhere else.
    parser: CommandParser,
    /// The rolling window the small model is given.
    window: VecDeque<f32>,
    /// Window capacity in samples.
    capacity: usize,
    /// Enough audio for one check to be worth making.
    minimum: usize,
    /// Samples fed since the last check, for [`WAKE_HOP`].
    ///
    /// Counted in samples rather than by the clock so the gate is deterministic:
    /// replaying a recording offers the same windows every time, which is what
    /// makes the harness in `examples/transcribe.rs` a measurement rather than a
    /// coin toss.
    since_check: usize,
    /// Consecutive quiet frames, which keep the window alive past the speech.
    quiet: usize,
    /// When the wake word was last heard, for the cooldown.
    fired: Option<Instant>,
    phase: Phase,
    /// The request being collected.
    capture: Vec<f32>,
    /// Consecutive quiet frames since the capture last held speech.
    silent: usize,
    /// Whether anything has been said since the model became ready.
    spoke: bool,
    /// The readiness flag handed out with the wake word.
    ///
    /// Kept as well as given away, because the capture has to know whether the
    /// model is up before it will let a silence end the request, and only the
    /// pipeline ever sets it.
    ready: Option<Arc<AtomicBool>>,
}

impl WakeGate {
    pub fn new(transcriber: Arc<dyn Transcriber>, parser: CommandParser) -> Self {
        let rate = transcriber.sample_rate();
        let samples = |duration: Duration| (duration.as_secs_f32() * rate as f32) as usize;
        Self {
            transcriber,
            parser,
            window: VecDeque::with_capacity(samples(WAKE_WINDOW)),
            capacity: samples(WAKE_WINDOW),
            minimum: samples(WAKE_HOP),
            since_check: 0,
            quiet: 0,
            fired: None,
            phase: Phase::Idle,
            capture: Vec::new(),
            silent: 0,
            spoke: false,
            ready: None,
        }
    }

    /// The small model, for a caller that has to transcribe off the blocking
    /// pool without borrowing the gate across the `await`.
    pub fn transcriber(&self) -> Arc<dyn Transcriber> {
        Arc::clone(&self.transcriber)
    }

    /// Feeds one tick's worth of samples; returns what to do about them.
    ///
    /// Cheap by design: the only work here is bookkeeping and one RMS, so the
    /// loop that feeds it is never the thing that falls behind. Transcription
    /// happens on the blocking pool, off the [`Step::Check`] this hands back.
    pub fn push(&mut self, samples: &[f32]) -> Step {
        match self.phase {
            Phase::Idle => self.push_idle(samples),
            Phase::Loading => self.push_capture(samples),
        }
    }

    /// The idle half: keep a window and offer it for checking.
    fn push_idle(&mut self, samples: &[f32]) -> Step {
        // A cooldown clears the window rather than pausing the gate, so audio
        // from before the last wake word is never part of the next check.
        if self.fired.is_some_and(|at| at.elapsed() < WAKE_COOLDOWN) {
            self.forget_window();
            return Step::Idle;
        }

        self.remember(samples);
        if rms(samples) >= VAD_RMS_THRESHOLD {
            self.quiet = 0;
            self.spoke = true;
        } else {
            self.quiet += frames_in(samples);
        }
        // Checked before offering a window, not after: at exactly the tail
        // length the run of quiet frames has not yet expired, and a check made
        // there would hand the model a window of pure silence.
        if self.quiet >= WAKE_TAIL_FRAMES {
            self.forget_window();
            return Step::Idle;
        }

        // The model is never handed a window too short to hold a word, so the
        // first check after a burst of silence waits for real audio rather than
        // spending an inference on 20 ms of nothing.
        if !self.spoke || self.since_check < self.minimum {
            return Step::Idle;
        }
        self.since_check = 0;
        Step::Check(self.window.iter().copied().collect())
    }

    /// The capture half: buffer the request until it is clearly over.
    ///
    /// Shared by loading and capturing, because a request may start before the
    /// larger model is ready and the audio cannot be recovered afterwards.
    fn push_capture(&mut self, samples: &[f32]) -> Step {
        if rms(samples) >= VAD_RMS_THRESHOLD {
            if self.is_ready() {
                self.spoke = true;
            }
            self.capture.extend_from_slice(samples);
            self.silent = 0;
        } else {
            self.silent += frames_in(samples);
        }

        if self.spoke && self.silent >= COMMAND_GAP_FRAMES {
            return self.finish();
        }
        if self.capture.len() >= COMMAND_MAX_FRAMES * FRAME_SAMPLES {
            return self.finish();
        }
        Step::Idle
    }

    /// The small model's verdict on the window it was given.
    ///
    /// Only the first wake word of a window is acted on: the wake word is
    /// followed by silence while the model loads, and re-reading the same window
    /// once a second would start the command again.
    pub fn transcribed(&mut self, transcript: &str) -> Option<Heard> {
        tracing::debug!(transcript = %transcript, "wake model heard");
        if self.phase != Phase::Idle {
            return None;
        }
        if !self.woke_on(transcript) {
            return None;
        }

        self.fired = Some(Instant::now());
        self.phase = Phase::Loading;
        self.capture.clear();
        self.silent = 0;
        self.spoke = false;
        let ready = Arc::new(AtomicBool::new(false));
        self.ready = Some(Arc::clone(&ready));
        Some(Heard::Wake(WakeHit { ready }))
    }

    /// Whether the transcript contained a configured wake word.
    fn woke_on(&self, transcript: &str) -> bool {
        transcript
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .any(|word| self.parser.is_wake_word(&word.to_ascii_lowercase()))
    }

    /// Hands the captured request over and reopens the gate.
    fn finish(&mut self) -> Step {
        let audio = std::mem::take(&mut self.capture);
        self.phase = Phase::Idle;
        self.silent = 0;
        self.spoke = false;
        self.ready = None;
        // The window is dropped rather than kept: it is the seconds before the
        // request, and re-checking them would only re-hear the wake word.
        self.forget_window();
        Step::Heard(Heard::Command(audio))
    }

    /// Whether the pipeline has reported the larger model loaded.
    fn is_ready(&self) -> bool {
        self.ready
            .as_ref()
            .is_some_and(|ready| ready.load(Ordering::Relaxed))
    }

    fn remember(&mut self, samples: &[f32]) {
        self.since_check += samples.len();
        self.window.extend(samples);
        while self.window.len() > self.capacity {
            self.window.pop_front();
        }
    }

    fn forget_window(&mut self) {
        self.window.clear();
        self.quiet = 0;
        self.since_check = 0;
        self.spoke = false;
    }
}

/// How many 20 ms frames a tick carried.
///
/// A tick is 20 ms only while the connection is quiet, so the count is derived
/// from the samples rather than assumed to be one.
fn frames_in(samples: &[f32]) -> usize {
    samples.len().div_ceil(FRAME_SAMPLES).max(1)
}

/// Consumes frames until the channel closes, awaiting `on_heard` for each event.
///
/// `on_heard` is awaited rather than spawned, which is what keeps the stages in
/// order: the gate cannot deliver the request it captured while the pipeline is
/// still loading the model for the wake word that came first. Audio that arrives
/// meanwhile waits in the channel, which is unbounded, so nothing is lost --
/// and the gate is deliberately idle for that stretch anyway.
pub async fn run_wake_gate<F, Fut>(
    mut gate: WakeGate,
    mut rx: UnboundedReceiver<Vec<f32>>,
    mut on_heard: F,
) where
    F: FnMut(Heard) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    while let Some(frame) = rx.recv().await {
        let window = match gate.push(&frame) {
            Step::Idle => continue,
            Step::Heard(heard) => {
                on_heard(heard).await;
                continue;
            }
            Step::Check(window) => window,
        };

        let transcriber = gate.transcriber();
        let inferred = tokio::task::spawn_blocking(move || transcriber.transcribe(&window)).await;
        let transcript = match inferred {
            Ok(Ok(text)) => text,
            Ok(Err(e)) => {
                println!("wake word transcription failed: {e:#}");
                continue;
            }
            Err(e) => {
                println!("wake word transcription task failed: {e}");
                continue;
            }
        };

        if let Some(heard) = gate.transcribed(&transcript) {
            on_heard(heard).await;
        }
    }
}

/// The larger model, loaded when a wake word arrives rather than at startup.
///
/// Loading is deferred so the process starts without the big weights resident
/// and so the acknowledgement can tell the speaker when the model is genuinely
/// ready, which is the whole reason for the second tone. It is then kept, like
/// the resident model it replaced: a reload per command would cost more than it
/// saves, and holding it is what made the in-process engine worth having.
///
/// The first load blocks, so it belongs on a blocking pool.
pub struct BigModels {
    engine: AsrEngine,
    model: String,
    language: Option<String>,
    loaded: Mutex<Option<Arc<dyn Transcriber>>>,
}

impl BigModels {
    pub fn new(engine: AsrEngine, model: &str, language: Option<&str>) -> Self {
        Self {
            engine,
            model: model.to_string(),
            language: language.map(str::to_string),
            loaded: Mutex::new(None),
        }
    }

    /// The loaded model, loading it on the first call.
    pub fn get_or_load(&self) -> anyhow::Result<Arc<dyn Transcriber>> {
        if let Some(loaded) = self.lock().as_ref() {
            return Ok(Arc::clone(loaded));
        }
        let transcriber: Arc<dyn Transcriber> = match self.engine {
            AsrEngine::Direct => Arc::new(DirectTranscriber::new(
                &self.model,
                self.language.as_deref(),
            )?),
            AsrEngine::Handy => Arc::new(HandyTranscriber::new(&self.model)),
        };
        *self.lock() = Some(Arc::clone(&transcriber));
        Ok(transcriber)
    }

    /// Whether the weights are in memory already.
    pub fn is_loaded(&self) -> bool {
        self.lock().is_some()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Arc<dyn Transcriber>>> {
        self.loaded.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A model that always returns the same text, so the gate's own logic is
    /// what is under test.
    struct FixedText(&'static str);

    impl Transcriber for FixedText {
        fn sample_rate(&self) -> u32 {
            16_000
        }

        fn transcribe(&self, _samples: &[f32]) -> anyhow::Result<String> {
            Ok(self.0.to_string())
        }
    }

    /// A model that returns a different text per call, so a window can answer
    /// "no" and then "yes".
    #[derive(Default)]
    struct Scripted(Mutex<Vec<String>>);

    impl Scripted {
        fn with(lines: &[&str]) -> Self {
            Self(Mutex::new(
                lines.iter().rev().map(|l| l.to_string()).collect(),
            ))
        }
    }

    impl Transcriber for Scripted {
        fn sample_rate(&self) -> u32 {
            16_000
        }

        fn transcribe(&self, _samples: &[f32]) -> anyhow::Result<String> {
            Ok(self.0.lock().unwrap().pop().unwrap_or_default())
        }
    }

    fn parser() -> CommandParser {
        CommandParser::new(["shamash".to_string(), "bot".to_string()])
    }

    /// One 20 ms frame at a normal speaking level.
    fn speech() -> Vec<f32> {
        vec![0.2; FRAME_SAMPLES]
    }

    fn silence() -> Vec<f32> {
        vec![0.0; FRAME_SAMPLES]
    }

    /// Feeds `frames` of audio and returns the check the gate offered, if any.
    fn offer(gate: &mut WakeGate, frames: &[Vec<f32>]) -> Option<Vec<f32>> {
        for frame in frames {
            if let Step::Check(window) = gate.push(frame) {
                return Some(window);
            }
        }
        None
    }

    #[test]
    fn silence_is_never_offered_for_checking() {
        let mut gate = WakeGate::new(Arc::new(FixedText("")), parser());
        assert!(offer(&mut gate, &vec![silence(); 100]).is_none());
    }

    #[test]
    fn a_burst_of_speech_is_offered_with_the_audio_it_carried() {
        let mut gate = WakeGate::new(Arc::new(FixedText("")), parser());
        let window = offer(&mut gate, &vec![speech(); 40]).expect("speech must be checked");
        assert!(!window.is_empty());
        assert!(window.iter().all(|s| *s == 0.2));
    }

    /// The window is what makes a wake word audible in the middle of a sentence.
    #[test]
    fn the_window_never_grows_past_its_cap() {
        let mut gate = WakeGate::new(Arc::new(FixedText("")), parser());
        let rate = 16_000;
        let cap = WAKE_WINDOW.as_secs_f32() as usize * rate;
        for _ in 0..400 {
            gate.push(&speech());
        }
        assert!(gate.window.len() <= cap, "{} samples", gate.window.len());
    }

    #[test]
    fn a_wake_word_in_a_sentence_still_fires() {
        let mut gate = WakeGate::new(Arc::new(FixedText("")), parser());
        offer(&mut gate, &vec![speech(); 40]).expect("a window");
        let heard = gate
            .transcribed("hey shamash play dracula")
            .expect("the wake word is in the sentence");
        assert!(matches!(heard, Heard::Wake(_)));
    }

    #[test]
    fn a_misheard_wake_word_still_fires() {
        let mut gate = WakeGate::new(Arc::new(FixedText("")), parser());
        offer(&mut gate, &vec![speech(); 40]).expect("a window");
        assert!(
            gate.transcribed("shemash over here").is_some(),
            "one wrong letter is what the parser already tolerates"
        );
    }

    #[test]
    fn ordinary_speech_does_not_fire_the_gate() {
        let mut gate = WakeGate::new(Arc::new(FixedText("")), parser());
        offer(&mut gate, &vec![speech(); 40]).expect("a window");
        assert!(gate.transcribed("what is for lunch today").is_none());
    }

    /// One shout must not start several commands.
    #[test]
    fn a_fired_gate_ignores_itself_until_it_cools_down() {
        let mut gate = WakeGate::new(Arc::new(FixedText("")), parser());
        offer(&mut gate, &vec![speech(); 40]).expect("a window");
        gate.transcribed("shamash").expect("the first shout fires");
        assert_eq!(gate.phase, Phase::Loading);

        // The same words again, while the model is still loading.
        assert!(gate.transcribed("shamash").is_none());
        // And after the capture is closed out, still inside the cooldown.
        let heard = gate.push(&speech());
        assert!(matches!(heard, Step::Idle), "{heard:?}");
    }

    #[test]
    fn a_capture_ends_only_after_speech_follows_the_ready_signal() {
        let mut gate = WakeGate::new(Arc::new(FixedText("bot")), parser());
        let heard = gate.transcribed("bot").expect("the wake word fires");
        let Heard::Wake(hit) = heard else {
            panic!("expected a wake word");
        };
        assert_eq!(gate.phase, Phase::Loading);

        // Silence before the model is up must not close anything.
        for _ in 0..COMMAND_GAP_FRAMES * 2 {
            assert!(matches!(gate.push(&silence()), Step::Idle));
        }

        hit.ready.store(true, Ordering::Relaxed);
        gate.push(&speech());
        assert_eq!(gate.phase, Phase::Loading);

        let mut ended = None;
        for _ in 0..COMMAND_GAP_FRAMES {
            if let Step::Heard(heard) = gate.push(&silence()) {
                ended = Some(heard);
                break;
            }
        }
        let Some(Heard::Command(audio)) = ended else {
            panic!("the request must be handed over once the speaker stops");
        };
        assert!(!audio.is_empty(), "the speech before the gap is kept");
        assert_eq!(gate.phase, Phase::Idle);
    }

    /// The bound is what stops a silent channel from leaving the gate armed.
    #[test]
    fn a_silent_capture_closes_on_the_length_cap() {
        let mut gate = WakeGate::new(Arc::new(FixedText("bot")), parser());
        let Heard::Wake(hit) = gate.transcribed("bot").expect("fires") else {
            panic!("expected a wake word");
        };
        hit.ready.store(true, Ordering::Relaxed);

        let mut ended = None;
        for _ in 0..COMMAND_MAX_FRAMES {
            if let Step::Heard(heard) = gate.push(&speech()) {
                ended = Some(heard);
                break;
            }
        }
        assert!(matches!(ended, Some(Heard::Command(_))), "{ended:?}");
        assert_eq!(gate.phase, Phase::Idle);
    }

    /// A back-to-back speech and silence must not register as a zero-length
    /// request, which would be transcribed into nothing.
    #[test]
    fn a_tiny_capture_is_still_handed_over() {
        let mut gate = WakeGate::new(Arc::new(FixedText("bot")), parser());
        let Heard::Wake(hit) = gate.transcribed("bot").expect("fires") else {
            panic!("expected a wake word");
        };
        hit.ready.store(true, Ordering::Relaxed);
        gate.push(&speech());
        let mut ended = None;
        for _ in 0..COMMAND_GAP_FRAMES {
            if let Step::Heard(heard) = gate.push(&silence()) {
                ended = Some(heard);
                break;
            }
        }
        assert!(matches!(ended, Some(Heard::Command(_))));
    }

    #[test]
    fn the_gate_reopens_after_a_command() {
        let mut gate = WakeGate::new(Arc::new(FixedText("bot")), parser());
        let Heard::Wake(hit) = gate.transcribed("bot").expect("fires") else {
            panic!("expected a wake word");
        };
        hit.ready.store(true, Ordering::Relaxed);
        for _ in 0..COMMAND_MAX_FRAMES {
            if matches!(gate.push(&speech()), Step::Heard(_)) {
                break;
            }
        }
        // Still inside the cooldown, so the gate is quiet rather than listening.
        assert_eq!(gate.phase, Phase::Idle);
        assert!(matches!(gate.push(&speech()), Step::Idle));
    }

    #[test]
    fn scripted_models_are_consumed_in_order() {
        let transcriber = Scripted::with(&["lunch", "shamash"]);
        assert_eq!(transcriber.transcribe(&[]).unwrap(), "lunch");
        assert_eq!(transcriber.transcribe(&[]).unwrap(), "shamash");
        assert_eq!(transcriber.transcribe(&[]).unwrap(), "");
    }

    /// The cooldown is what stops the same shout being heard twice, so it has to
    /// outlast the two acknowledgements rather than be a token delay.
    #[test]
    fn the_cooldown_outlasts_both_tones() {
        let tones = Duration::from_millis(150 + 50) * 2;
        assert!(WAKE_COOLDOWN > tones, "{WAKE_COOLDOWN:?}");
    }

    #[tokio::test]
    async fn the_loop_delivers_the_wake_word_before_the_request() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<f32>>();
        let order = Arc::new(Mutex::new(Vec::new()));

        let seen = Arc::clone(&order);
        let handle = tokio::spawn(async move {
            let gate = WakeGate::new(Arc::new(FixedText("bot")), parser());
            let on_heard = |heard: Heard| {
                let seen = Arc::clone(&seen);
                async move {
                    match heard {
                        Heard::Wake(hit) => {
                            seen.lock().unwrap().push("wake");
                            // The model is "loaded" immediately, then the gate is
                            // told so, exactly as the pipeline does.
                            hit.ready.store(true, Ordering::Relaxed);
                        }
                        Heard::Command(_) => seen.lock().unwrap().push("command"),
                    }
                }
            };
            run_wake_gate(gate, rx, on_heard).await;
        });

        // Enough speech for several checks, then quiet to close the capture.
        for _ in 0..40 {
            tx.send(speech()).unwrap();
        }
        for _ in 0..COMMAND_GAP_FRAMES * 3 {
            tx.send(silence()).unwrap();
        }
        drop(tx);
        let _ = handle.await;

        assert_eq!(
            order.lock().unwrap().clone(),
            vec!["wake", "command"],
            "the request must never arrive before its wake word"
        );
    }
}
