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
    /// The wake word on its own: wait for the request.
    WakeOnly,
    /// Not a command after all.
    Nothing(Option<ParseMiss>),
}

/// The grammar applied to the larger model's transcript of an utterance.
///
/// `armed` is an utterance that followed a bare wake word, which is the request
/// whether or not it repeats the wake word.
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

/// Acts on what [`crate::wake`] hears: acknowledge the wake word, transcribe
/// the utterance with the larger model, and turn it into a song.
pub struct ListenerPipeline {
    /// The larger model, loaded on the first wake word rather than at startup.
    pub models: Arc<BigModels>,
    pub parser: CommandParser,
    pub player: Arc<Player>,
    /// Speakers waiting to say their request after a bare wake word.
    armed: Armed,
    /// Who each voice stream belongs to, for the log.
    speakers: Speakers,
    /// Utterances that produced no words, reported once there is something
    /// real to say.
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

    /// Starts the task that handles what the gate hears, and returns its inbox.
    ///
    /// Utterances are handled one at a time, in the order they were heard. The
    /// acknowledgement tone is played on its own task, so a request still being
    /// resolved cannot delay telling the next speaker they were heard.
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

    /// The wake word was heard while the speaker is still talking.
    ///
    /// The music is lowered at once, so it does not bleed into the rest of
    /// what they say through other people's microphones. That part is
    /// synchronous on purpose: the utterance that follows raises it again, and
    /// must never be handled before it was lowered.
    fn handle_wake(self: &Arc<Self>, guild_id: GuildId, ssrc: Ssrc) {
        self.player.duck(guild_id, ssrc, true);
        let this = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(e) = this.player.acknowledge(guild_id, Tone::Wake).await {
                tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "wake tone failed");
            }
            // Start loading the larger model now, so it is ready by the time
            // the speaker finishes. A no-op once it is resident.
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

    /// Returns whether the music should stay lowered, which it does only while
    /// the speaker is armed to say their request.
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
                tracing::debug!(guild_id = %guild_id, reason = ?miss, "not a play command");
                return false;
            }
        };

        // The music comes back while the song is looked up, rather than after.
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

    /// The speaker said only the wake word: tell them to go ahead, and take
    /// their next utterance as the request.
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

    /// The larger model's transcript of `audio`, or `None` if it failed.
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

    /// The rewriting stage, or `None` when it is off or produced nothing better.
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
mod tests {
    use super::*;

    fn parser() -> CommandParser {
        CommandParser::new(["bot".to_string(), "shamash".to_string()])
    }

    #[test]
    fn a_request_in_the_same_breath_is_understood() {
        let Understood::Request(r) =
            understand(&parser(), "Bot, play Dracula by Tame Impala.", false)
        else {
            panic!("expected a request");
        };
        assert_eq!(r.title.as_deref(), Some("dracula"));
    }

    #[test]
    fn play_is_not_needed_after_an_opening_wake_word() {
        assert!(matches!(
            understand(&parser(), "Shamash, Bohemian Rhapsody", false),
            Understood::Request(_)
        ));
    }

    #[test]
    fn a_bare_wake_word_waits_for_the_request() {
        assert_eq!(understand(&parser(), "Bot.", false), Understood::WakeOnly);
    }

    #[test]
    fn an_armed_utterance_is_the_request_without_a_wake_word() {
        let Understood::Request(r) = understand(&parser(), "Dracula by Tame Impala", true) else {
            panic!("expected a request");
        };
        assert_eq!(r.artist.as_deref(), Some("tame impala"));
    }

    #[test]
    fn the_wake_model_mishearing_is_not_a_request() {
        assert!(matches!(
            understand(&parser(), "what's for lunch", false),
            Understood::Nothing(Some(ParseMiss::NoWakeWord))
        ));
    }
}
