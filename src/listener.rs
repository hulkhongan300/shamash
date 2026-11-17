//! Receiving voice from Discord, one stream (SSRC) per speaker, never mixed:
//! the model hears overlapping voices badly.

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

/// Mono sample rate songbird decodes to.
pub const LISTEN_SAMPLE_RATE: u32 = 16_000;
/// One 20 ms Discord packet.
pub const FRAME_SAMPLES: usize = 320;

/// -40 dBFS; only a `VOICE_DIAG` hint, the voice detector ignores level.
pub const QUIET_SPEECH_RMS: f32 = 0.01;

/// SSRC to Discord user, from speaking-state events.
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

/// i16 PCM to f32 in [-1, 1].
pub fn to_f32(frame: &[i16]) -> Vec<f32> {
    frame.iter().map(|s| f32::from(*s) / 32768.0).collect()
}

/// Splits a tick per speaker; silent speakers get zeros, since Discord never
/// sends silence and an utterance could otherwise never end.
pub fn split_tick<'a>(
    speaking: impl IntoIterator<Item = (Ssrc, Option<&'a [i16]>)>,
    silent: impl IntoIterator<Item = Ssrc>,
) -> Tick {
    let mut tick: Tick = speaking
        .into_iter()
        .map(|(ssrc, decoded)| match decoded {
            Some(pcm) if !pcm.is_empty() => (ssrc, to_f32(pcm)),
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

/// Forwards each tick's per-speaker audio.
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

    fn announce_first_tick(&mut self) -> bool {
        let first = !self.announced;
        self.announced = true;
        first
    }

    /// `voiced`: speakers that sent real audio, not padded silence.
    fn observe(&mut self, tick: &Tick, voiced: usize) {
        self.ticks += 1;
        self.frames += voiced as u64;
        self.speakers = self.speakers.max(tick.len());
        for (_, audio) in tick {
            self.peak = self.peak.max(rms(audio));
        }
    }

    /// Reports and resets once `interval` passed; tick-driven so "nothing
    /// received" differs from "silence received".
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
                    .map(|(ssrc, data)| (*ssrc, data.decoded_voice.as_deref())),
                tick.silent.iter().copied(),
            );
            self.record(&split, voiced);
            if !split.is_empty() {
                let _ = self.tx.send(split);
            }
        }
        // Must stay `None`: returning the event silently unregisters the
        // handler (songbird `EventStore::process_untimed`), deafening the bot.
        None
    }
}

#[cfg(test)]
#[path = "tests/listener.rs"]
mod tests;
