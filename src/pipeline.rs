use crate::config::Config;
use crate::listener::Speakers;
use crate::llm;
use crate::parser::{CommandParser, Control, ParseMiss, PlayRequest};
use crate::player::{Player, Tone};
use crate::state::{HttpClientKey, ListenLock};
use crate::voice::VoiceService;
use crate::wake::{ARM_TTL, Armed, BigModels, Heard, Ssrc};
use serenity::model::id::GuildId;
use serenity::prelude::Context as SerenityContext;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

/// What a transcribed utterance turned out to be.
#[derive(Debug, PartialEq)]
pub enum Understood {
    /// A request to play something.
    Request(PlayRequest),
    /// "groovy play" on its own: wait for the request.
    WakeOnly,
    /// "groovy skip", "groovy stop", "groovy pause", "groovy lock" and the like.
    Control(Control),
    /// Not a command after all.
    Nothing(Option<ParseMiss>),
}

/// Parses the larger model's transcript; `armed` means it follows a bare wake word.
pub fn understand(parser: &CommandParser, transcript: &str, armed: bool) -> Understood {
    if armed {
        return match parser.armed_request(transcript) {
            Some(request) => Understood::Request(request),
            None => Understood::Nothing(Some(ParseMiss::EmptyRequest)),
        };
    }
    match parser.command(transcript) {
        Ok(request) => Understood::Request(request),
        Err(ParseMiss::EmptyRequest) => Understood::WakeOnly,
        Err(miss) => match parser.control(transcript) {
            Some(control) => Understood::Control(control),
            None => Understood::Nothing(Some(miss)),
        },
    }
}

/// Turns what [`crate::wake`] hears into acknowledgements and songs.
pub struct ListenerPipeline {
    /// Loaded on the first wake word, not at startup.
    pub models: Arc<BigModels>,
    pub parser: CommandParser,
    pub player: Arc<Player>,
    armed: Armed,
    speakers: Speakers,
    lock: ListenLock,
    /// Wordless utterances, logged with the next real one.
    muted_utterances: AtomicUsize,
}

impl ListenerPipeline {
    pub fn new(
        models: Arc<BigModels>,
        parser: CommandParser,
        player: Arc<Player>,
        armed: Armed,
        speakers: Speakers,
        lock: ListenLock,
    ) -> Self {
        Self {
            models,
            parser,
            player,
            armed,
            speakers,
            lock,
            muted_utterances: AtomicUsize::new(0),
        }
    }

    /// Starts the in-order handler task and returns its inbox.
    pub fn start(
        self: Arc<Self>,
        ctx: SerenityContext,
        config: Config,
        guild_id: GuildId,
    ) -> UnboundedSender<Heard> {
        let (tx, mut rx) = unbounded_channel::<Heard>();
        tokio::spawn(async move {
            while let Some(heard) = rx.recv().await {
                match heard {
                    Heard::Wake { ssrc } => self.handle_wake(guild_id, ssrc),
                    Heard::Utterance {
                        ssrc, audio, armed, ..
                    } => {
                        self.handle_utterance(&ctx, &config, guild_id, ssrc, audio, armed)
                            .await
                    }
                }
            }
        });
        tx
    }

    /// Ducks synchronously so the following utterance can never unduck first;
    /// the tone runs on its own task so a slow request cannot delay it.
    fn handle_wake(self: &Arc<Self>, guild_id: GuildId, ssrc: Ssrc) {
        self.player.duck(guild_id, ssrc, true);
        let this = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(e) = this.player.acknowledge(guild_id, Tone::Wake).await {
                tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "wake tone failed");
            }
            // Warm the larger model while the speaker is still talking.
            let models = Arc::clone(&this.models);
            if let Ok(Err(e)) = tokio::task::spawn_blocking(move || models.get_or_load()).await {
                tracing::error!(guild_id = %guild_id, error = %format!("{e:#}"), "could not load the larger model");
            }
        });
    }

    async fn handle_utterance(
        self: &Arc<Self>,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        ssrc: Ssrc,
        audio: Vec<f32>,
        armed: bool,
    ) {
        let timing = crate::timing::Timing::default();
        let keep_ducked = self
            .handle_utterance_timed(ctx, config, guild_id, ssrc, audio, armed, &timing)
            .await;
        if !keep_ducked {
            self.player.duck(guild_id, ssrc, false);
        }
        timing.log("voice");
    }

    /// Returns whether to stay ducked (only while the speaker is armed).
    #[allow(clippy::too_many_arguments)]
    async fn handle_utterance_timed(
        self: &Arc<Self>,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        ssrc: Ssrc,
        audio: Vec<f32>,
        armed: bool,
        timing: &crate::timing::Timing,
    ) -> bool {
        let Some(transcript) = self.transcribe(guild_id, audio, timing).await else {
            return false;
        };
        let user = self.speakers.user(ssrc).map(|u| u.to_string());
        let user = user.as_deref().unwrap_or("unknown");

        // Locked: act on "groovy unlock" only, and log nothing that was said.
        if self.lock.is_locked(guild_id) {
            if understand(&self.parser, &transcript, false) == Understood::Control(Control::Unlock)
            {
                self.control(ctx, config, guild_id, Control::Unlock, user)
                    .await;
            }
            return false;
        }

        if transcript.trim().is_empty() {
            self.muted_utterances.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(guild_id = %guild_id, user, "heard an utterance but no words in it");
            return false;
        }
        let skipped = self.muted_utterances.swap(0, Ordering::Relaxed);
        tracing::info!(guild_id = %guild_id, user, transcript = %transcript, armed, skipped, "request heard");

        let request = match understand(&self.parser, &transcript, armed) {
            Understood::Request(request) => request,
            Understood::WakeOnly => {
                self.arm(guild_id, ssrc);
                return true;
            }
            Understood::Control(control) => {
                self.control(ctx, config, guild_id, control, user).await;
                return false;
            }
            Understood::Nothing(miss) => {
                // The false-wake log line.
                let reason = miss.map_or_else(|| "no request".to_string(), |m| m.to_string());
                tracing::info!(guild_id = %guild_id, user, transcript = %transcript, armed, reason, "woke, but heard no request");
                return false;
            }
        };

        // Unduck before the lookup, not after.
        self.player.duck(guild_id, ssrc, false);
        let songs = self.songs(guild_id, request, timing).await;
        // The rewrite reads the whole transcript, so it only fits a single song.
        let rewritten = match songs.len() {
            1 => self
                .rewrite(ctx, config, guild_id, &transcript, timing)
                .await
                .and_then(|text| self.parser.strip_command(&text)),
            _ => None,
        };
        if let Some(rewritten) = &rewritten {
            tracing::info!(guild_id = %guild_id, rewrite = %rewritten, "the transcript rewrite is a second candidate for this request");
        }
        // In order: the first starts playing and the rest queue behind it.
        for request in &songs {
            tracing::info!(guild_id = %guild_id, user, request = %request.query, songs = songs.len(), "playing a request");
            if let Err(e) = self
                .player
                .play(ctx, config, guild_id, request, rewritten.as_deref(), timing)
                .await
            {
                tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "playback failed");
            }
        }
        false
    }

    /// The songs in one request: "x and play y" always splits; a bare "and"
    /// only when the whole phrase is not one song, as "Me and Bobby McGee" is.
    async fn songs(
        &self,
        guild_id: GuildId,
        request: PlayRequest,
        timing: &crate::timing::Timing,
    ) -> Vec<PlayRequest> {
        let sure = crate::parser::songs(&request.query, false);
        if sure.len() > 1 {
            return sure;
        }
        let every = crate::parser::songs(&request.query, true);
        if every.len() < 2
            || crate::parser::album_request(&request).is_some()
            || self.player.is_one_song(guild_id, &request, timing).await
        {
            return vec![request];
        }
        every
    }

    /// Carries out a spoken control the way its slash command or button would.
    async fn control(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        control: Control,
        user: &str,
    ) {
        tracing::info!(guild_id = %guild_id, user, ?control, "voice control");
        match control {
            Control::Skip => self.player.skip(ctx, config, guild_id).await,
            Control::Stop => {
                self.player.release_duck(guild_id);
                self.player.stop(ctx, config, guild_id).await;
                let left = match VoiceService::from_ctx(ctx).await {
                    Ok(service) => service.stop(guild_id).await,
                    Err(e) => Err(e),
                };
                if let Err(e) = left {
                    tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "could not leave the voice channel");
                }
            }
            Control::Lock => {
                self.lock.set(guild_id, true);
                self.player.release_duck(guild_id);
            }
            Control::Unlock => {
                self.lock.set(guild_id, false);
            }
            Control::Version => {
                self.player
                    .post_info(ctx, config, guild_id, &crate::bot::version())
                    .await;
            }
            Control::Pause | Control::Resume => {
                let paused = control == Control::Pause;
                if self.player.set_paused(guild_id, paused).await.is_none() {
                    tracing::info!(guild_id = %guild_id, "nothing to pause");
                }
                self.player.draw_dashboard(ctx, config, guild_id).await;
            }
        }
    }

    /// Bare wake word: play the ready tone and take the next utterance as the request.
    fn arm(self: &Arc<Self>, guild_id: GuildId, ssrc: Ssrc) {
        let deadline = self.armed.arm(ssrc);
        tracing::info!(guild_id = %guild_id, ssrc, "heard the wake word on its own; waiting for the request");
        let this = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(e) = this.player.acknowledge(guild_id, Tone::Ready).await {
                tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "ready tone failed");
            }
            tokio::time::sleep(ARM_TTL).await;
            if this.armed.expire(ssrc, deadline) {
                tracing::debug!(guild_id = %guild_id, ssrc, "no request followed the wake word");
                this.player.duck(guild_id, ssrc, false);
            }
        });
    }

    async fn transcribe(
        &self,
        guild_id: GuildId,
        audio: Vec<f32>,
        timing: &crate::timing::Timing,
    ) -> Option<String> {
        let models = Arc::clone(&self.models);
        let inferring = Instant::now();
        let inferred = tokio::task::spawn_blocking(move || {
            models
                .get_or_load()
                .and_then(|model| model.transcribe(&audio))
        })
        .await;
        timing.record("transcription", inferring.elapsed());
        match inferred {
            Ok(Ok(transcript)) => Some(transcript),
            Ok(Err(e)) => {
                tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "transcription failed");
                None
            }
            Err(e) => {
                tracing::error!(guild_id = %guild_id, error = %format!("{e:#}"), "transcription task failed");
                None
            }
        }
    }

    /// `None` when the rewrite is off or produced nothing better.
    async fn rewrite(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        transcript: &str,
        timing: &crate::timing::Timing,
    ) -> Option<String> {
        let endpoint = config.llm_endpoint.as_deref()?;
        let http = {
            let data = ctx.data.read().await;
            data.get::<HttpClientKey>().cloned()
        }?;
        let rewriting = Instant::now();
        let reply = llm::rewrite(&http, endpoint, &config.llm_model, transcript).await;
        timing.record("llm_rewrite", rewriting.elapsed());
        match reply {
            Ok(Some(rewritten)) => Some(rewritten),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "the transcript rewrite failed");
                None
            }
        }
    }
}

#[cfg(test)]
#[path = "tests/pipeline.rs"]
mod tests;
