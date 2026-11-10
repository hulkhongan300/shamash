//! Receiving voice from Discord.
//!
//! Every speaker arrives as their own stream (an RTP SSRC), and they are kept
//! apart rather than mixed: a model given two people talking at once, or one
//! person and someone else's music bleeding into their microphone, hears
//! neither of them well. [`crate::wake`] runs one gate per stream.

use crate::audio::rms;
use crate::wake::{Ssrc, Tick};
use serenity::async_trait;
use serenity::model::id::UserId;
use songbird::EventHandler;
use songbird::events::{Event, EventContext};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

/// Sample rate (Hz) of the mono audio songbird is configured to decode to.
pub const LISTEN_SAMPLE_RATE: u32 = 16_000;
/// Samples per 20 ms frame at [`LISTEN_SAMPLE_RATE`]: one Discord packet.
pub const FRAME_SAMPLES: usize = 320;

/// A frame RMS below which the diagnostic report calls audio too quiet to be
/// ordinary speech: -40 dBFS. Only a hint for `VOICE_DIAG`; the voice detector
/// does not use a level at all.
pub const QUIET_SPEECH_RMS: f32 = 0.01;

/// Which Discord user each voice stream belongs to, learnt from songbird's
/// speaking-state events. Used to say who asked.
#[derive(Debug, Clone, Default)]
pub struct Speakers(Arc<Mutex<HashMap<Ssrc, UserId>>>);

impl Speakers {
    pub fn user(&self, ssrc: Ssrc) -> Option<UserId> {
        self.lock().get(&ssrc).copied()
    }

    fn set(&self, ssrc: Ssrc, user: UserId) {
        self.lock().insert(ssrc, user);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Ssrc, UserId>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Records which user owns which voice stream.
pub struct SpeakingHandler(pub Speakers);

#[async_trait]
impl EventHandler for SpeakingHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::SpeakingStateUpdate(speaking) = ctx
            && let Some(user) = speaking.user_id
            && user.0 != 0
        {
            self.0.set(speaking.ssrc, UserId::new(user.0));
        }
        // `None` keeps the handler registered; see `VoiceTickHandler::act`.
        None
    }
}

/// Converts one speaker's decoded i16 frame to f32 in [-1, 1].
pub fn to_f32(frame: &[i16]) -> Vec<f32> {
    frame.iter().map(|s| f32::from(*s) / 32768.0).collect()
}

/// Splits one songbird tick into each speaker's audio.
///
/// A speaker the call knows about who sent nothing this tick gets a frame of
/// silence. Discord only sends audio while somebody transmits, so silence
/// never arrives on its own, and without it an utterance could never end.
pub fn split_tick(
    speaking: impl IntoIterator<Item = (Ssrc, Option<Vec<i16>>)>,
    silent: impl IntoIterator<Item = Ssrc>,
) -> Tick {
    let mut tick: Tick = speaking
        .into_iter()
        .map(|(ssrc, decoded)| match decoded {
            Some(pcm) if !pcm.is_empty() => (ssrc, to_f32(&pcm)),
            _ => (ssrc, vec![0.0; FRAME_SAMPLES]),
        })
        .collect();
    tick.extend(
        silent
            .into_iter()
            .map(|ssrc| (ssrc, vec![0.0; FRAME_SAMPLES])),
    );
    tick
}

/// Songbird event handler forwarding each tick's per-speaker audio.
#[derive(Debug)]
pub struct VoiceTickHandler {
    tx: UnboundedSender<Tick>,
    diag: Mutex<Option<TickDiag>>,
}

/// Rolling counters behind the `VOICE_DIAG` report.
#[derive(Debug)]
struct TickDiag {
    since: Instant,
    ticks: u64,
    frames: u64,
    speakers: usize,
    peak: f32,
    announced: bool,
}

impl VoiceTickHandler {
    pub fn new(tx: UnboundedSender<Tick>) -> Self {
        let diag = std::env::var("VOICE_DIAG")
            .ok()
            .filter(|v| !v.is_empty() && v != "0")
            .map(|_| {
                tracing::info!("voice diag: handler installed, expecting voice ticks");
                TickDiag::new(Instant::now())
            });
        Self {
            tx,
            diag: Mutex::new(diag),
        }
    }

    fn record(&self, tick: &Tick, voiced: usize) {
        let mut diag = self.diag.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(diag) = diag.as_mut() else {
            return;
        };
        if diag.announce_first_tick() {
            tracing::info!("voice diag: first voice tick received");
        }
        diag.observe(tick, voiced);
        if let Some(report) = diag.report(Instant::now(), DIAG_INTERVAL) {
            tracing::info!("{report}");
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
            speakers: 0,
            peak: 0.0,
            announced: false,
        }
    }

    /// Whether this is the first tick seen, so the arrival can be announced.
    fn announce_first_tick(&mut self) -> bool {
        let first = !self.announced;
        self.announced = true;
        first
    }

    /// Folds one tick into the counters. `voiced` is how many of its speakers
    /// actually sent audio, rather than being given silence.
    fn observe(&mut self, tick: &Tick, voiced: usize) {
        self.ticks += 1;
        self.frames += voiced as u64;
        self.speakers = self.speakers.max(tick.len());
        for (_, audio) in tick {
            self.peak = self.peak.max(rms(audio));
        }
    }

    /// Returns a report once `interval` has passed, and resets the counters.
    ///
    /// Driven by ticks rather than audio, so a bot that receives nothing can be
    /// told apart from one that receives silence.
    fn report(&mut self, now: Instant, interval: Duration) -> Option<String> {
        let elapsed = now.saturating_duration_since(self.since);
        if elapsed < interval {
            return None;
        }
        let seconds = elapsed.as_secs_f32().max(f32::EPSILON);
        let mut report = format!(
            "voice diag: {} ticks and {} audio frames in {seconds:.0}s from up to {} speaker(s), \
             peak rms {:.4} ({:.1} dBFS)",
            self.ticks,
            self.frames,
            self.speakers,
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
        } else if self.peak < QUIET_SPEECH_RMS {
            report.push_str(
                "\n  audio is arriving but very quietly: raise the microphone gain in Discord",
            );
        }
        let announced = self.announced;
        *self = Self::new(now);
        self.announced = announced;
        Some(report)
    }
}

/// Full-scale decibel level of a linear amplitude.
pub fn to_dbfs(amplitude: f32) -> f32 {
    20.0 * amplitude.max(1e-9).log10()
}

#[async_trait]
impl EventHandler for VoiceTickHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::VoiceTick(tick) = ctx {
            let voiced = tick
                .speaking
                .values()
                .filter(|data| data.decoded_voice.as_ref().is_some_and(|v| !v.is_empty()))
                .count();
            let split = split_tick(
                tick.speaking
                    .iter()
                    .map(|(ssrc, data)| (*ssrc, data.decoded_voice.clone())),
                tick.silent.iter().copied(),
            );
            self.record(&split, voiced);
            if !split.is_empty() {
                let _ = self.tx.send(split);
            }
        }
        // Must stay `None`. Returning the same event type we were invoked with
        // looks like a no-op but deletes the handler: songbird removes it from
        // the list it is iterating, re-adds it to the store it just emptied,
        // and then overwrites that store with the list minus the handler. The
        // bot then receives exactly one tick and is deaf from then on, with no
        // error anywhere. See `EventStore::process_untimed` in songbird, and
        // the `EventHandler::act` contract: `None` maintains the event type.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speakers_are_kept_apart_rather_than_mixed() {
        let tick = split_tick(
            [
                (1, Some(vec![1000i16; 320])),
                (2, Some(vec![-2000i16; 320])),
            ],
            [],
        );
        assert_eq!(tick.len(), 2);
        let one = tick.iter().find(|(s, _)| *s == 1).unwrap();
        let two = tick.iter().find(|(s, _)| *s == 2).unwrap();
        assert!((one.1[0] - 1000.0 / 32768.0).abs() < 1e-6);
        assert!((two.1[0] + 2000.0 / 32768.0).abs() < 1e-6);
    }

    /// The failure this prevents: a speaker stops transmitting, Discord sends
    /// nothing at all, and their utterance never ends.
    #[test]
    fn a_silent_speaker_is_given_silence() {
        let tick = split_tick([], [9]);
        assert_eq!(tick, vec![(9, vec![0.0; FRAME_SAMPLES])]);
    }

    #[test]
    fn a_lost_packet_is_silence_not_nothing() {
        let tick = split_tick([(4, None), (5, Some(Vec::new()))], []);
        assert!(tick.iter().all(|(_, a)| a.len() == FRAME_SAMPLES));
    }

    #[test]
    fn full_scale_converts_to_full_scale() {
        assert_eq!(to_f32(&[i16::MIN])[0], -1.0);
        assert!(to_f32(&[i16::MAX])[0] < 1.0);
    }

    #[test]
    fn the_first_tick_is_announced_only_once() {
        let mut diag = TickDiag::new(Instant::now());
        assert!(diag.announce_first_tick());
        assert!(!diag.announce_first_tick());
        let _ = diag.report(Instant::now() + DIAG_INTERVAL, DIAG_INTERVAL);
        assert!(!diag.announce_first_tick());
    }

    #[test]
    fn diag_names_the_ways_audio_can_be_missing() {
        let start = Instant::now();

        let mut diag = TickDiag::new(start);
        let report = diag.report(start + DIAG_INTERVAL, DIAG_INTERVAL).unwrap();
        assert!(report.contains("no voice ticks at all"), "{report}");

        let mut diag = TickDiag::new(start);
        diag.observe(&split_tick([], [1]), 0);
        let report = diag.report(start + DIAG_INTERVAL, DIAG_INTERVAL).unwrap();
        assert!(report.contains("carry no audio"), "{report}");

        let mut diag = TickDiag::new(start);
        diag.observe(&split_tick([(1, Some(vec![30i16; 320]))], []), 1);
        let report = diag.report(start + DIAG_INTERVAL, DIAG_INTERVAL).unwrap();
        assert!(report.contains("very quietly"), "{report}");

        let mut diag = TickDiag::new(start);
        diag.observe(&split_tick([(1, Some(vec![6000i16; 320]))], []), 1);
        let report = diag.report(start + DIAG_INTERVAL, DIAG_INTERVAL).unwrap();
        assert!(
            !report.contains("quietly") && !report.contains("no audio"),
            "{report}"
        );
    }

    #[test]
    fn diag_stays_quiet_until_the_interval_passes() {
        let start = Instant::now();
        let mut diag = TickDiag::new(start);
        assert!(
            diag.report(start + Duration::from_secs(4), DIAG_INTERVAL)
                .is_none()
        );
        assert!(diag.report(start + DIAG_INTERVAL, DIAG_INTERVAL).is_some());
        assert_eq!(diag.ticks, 0, "counters reset after a report");
    }
}
