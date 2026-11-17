use crate::config::Config;
use crate::listener::Speakers;
use crate::llm;
use crate::parser::{CommandParser, ParseMiss, PlayRequest};
use crate::player::{Player, Tone};
use crate::state::HttpClientKey;
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
        Err(miss) => Understood::Nothing(Some(miss)),
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
    ) -> Self {
        Self {
            models,
            parser,
            player,
            armed,
            speakers,
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
            Understood::Nothing(miss) => {
                // The false-wake log line.
                let reason = miss.map_or_else(|| "no request".to_string(), |m| m.to_string());
                tracing::info!(guild_id = %guild_id, user, transcript = %transcript, armed, reason, "woke, but heard no request");
                return false;
            }
        };

        // Unduck before the lookup, not after.
        self.player.duck(guild_id, ssrc, false);
        let rewritten = self
            .rewrite(ctx, config, guild_id, &transcript, timing)
            .await
            .and_then(|text| self.parser.strip_command(&text));
        if let Some(rewritten) = &rewritten {
            tracing::info!(guild_id = %guild_id, rewrite = %rewritten, "the transcript rewrite is a second candidate for this request");
        }
        tracing::info!(guild_id = %guild_id, user, request = %request.query, "playing a request");
        if let Err(e) = self
            .player
            .play(
                ctx,
                config,
                guild_id,
                &request,
                rewritten.as_deref(),
                timing,
            )
            .await
        {
            tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "playback failed");
        }
        false
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
