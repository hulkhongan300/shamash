use crate::config::Config;
use crate::llm;
use crate::parser::{CommandParser, ParseMiss, PlayRequest};
use crate::player::{Player, Tone};
use crate::state::HttpClientKey;
use crate::wake::{BigModels, Heard, WakeHit};
use serenity::model::id::GuildId;
use serenity::prelude::Context as SerenityContext;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// How long a "bot play" with no title keeps waiting for one.
///
/// Speaking the command and the song title with a pause between them produces
/// two utterances. The first carries the wake word and "play", the second is
/// just the title, and the second is only recognisable as a command because of
/// the first.
const PENDING_QUERY_TTL: Duration = Duration::from_secs(8);

/// Remembers a "bot play" that is still waiting for a title.
#[derive(Debug, Default)]
struct PendingQuery {
    since: std::sync::Mutex<Option<Instant>>,
}

impl PendingQuery {
    /// Notes that a wake word and "play" were heard with no title after them.
    fn expect(&self) {
        if let Ok(mut slot) = self.since.lock() {
            *slot = Some(Instant::now());
        }
    }

    /// Whether a title is still being awaited, clearing the slot either way so
    /// one stray utterance cannot claim a command spoken much later.
    fn take(&self) -> bool {
        let Ok(mut slot) = self.since.lock() else {
            return false;
        };
        let pending = slot.is_some_and(|at| at.elapsed() < PENDING_QUERY_TTL);
        *slot = None;
        pending
    }

    fn clear(&self) {
        if let Ok(mut slot) = self.since.lock() {
            *slot = None;
        }
    }
}

/// Acts on what [`crate::wake`] hears: acknowledge the wake word, bring the
/// larger model up, then turn the captured request into a song.
pub struct ListenerPipeline {
    /// The larger model, loaded on the first wake word rather than at startup.
    pub models: Arc<BigModels>,
    pub parser: CommandParser,
    pub player: Arc<Player>,
    /// Utterances that produced no words, reported once there is something
    /// real to say. Background noise trips the voice detector often enough
    /// that printing every one of them drowns out real commands.
    muted_utterances: std::sync::atomic::AtomicUsize,
    /// Set when a wake word and "play" were heard with no title after them.
    awaiting_query: PendingQuery,
}

impl ListenerPipeline {
    pub fn new(models: Arc<BigModels>, parser: CommandParser, player: Arc<Player>) -> Self {
        Self {
            models,
            parser,
            player,
            muted_utterances: std::sync::atomic::AtomicUsize::new(0),
            awaiting_query: PendingQuery::default(),
        }
    }

    /// Dispatches one event from the gate.
    ///
    /// Awaited rather than spawned by the caller, so the request can never be
    /// acted on before the model for its wake word is up.
    pub async fn handle(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        heard: Heard,
    ) {
        match heard {
            Heard::Wake(hit) => self.handle_wake(guild_id, hit).await,
            Heard::Command(audio) => self.handle_request(ctx, config, guild_id, audio).await,
        }
    }

    /// The first tone: the wake word was heard.
    ///
    /// This is the only acknowledgement a speaker gets for being heard, and it
    /// comes before any waiting, so it cannot be mistaken for the answer.
    async fn handle_wake(&self, guild_id: GuildId, hit: WakeHit) {
        if let Err(e) = self.player.acknowledge(guild_id, Tone::Wake).await {
            tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "wake tone failed");
        }

        let models = Arc::clone(&self.models);
        // The load reads the whole model off disk, so it cannot run on the
        // reactor thread the rest of the bot is on.
        let loaded = tokio::task::spawn_blocking(move || models.get_or_load()).await;
        let transcriber = match loaded {
            Ok(Ok(transcriber)) => transcriber,
            Ok(Err(e)) => {
                // Nothing can be answered now, so the gate is not told the model
                // is ready: it closes the capture on its length bound instead of
                // transcribing with a model that is not there.
                tracing::error!(guild_id = %guild_id, error = %format!("{e:#}"), "could not load the larger model");
                return;
            }
            Err(e) => {
                tracing::error!(guild_id = %guild_id, error = %format!("{e:#}"), "model load task failed");
                return;
            }
        };
        tracing::info!(
            guild_id = %guild_id,
            sample_rate = transcriber.sample_rate(),
            "the larger model is ready; listening for a request"
        );

        if let Err(e) = self.player.acknowledge(guild_id, Tone::Ready).await {
            tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "ready tone failed");
        }
        // Set only now, so the silence before the speaker starts talking cannot
        // close the capture before anything has been said.
        hit.ready.store(true, Ordering::Relaxed);
    }

    /// The second tone has played and a request was captured: transcribe it,
    /// clean it up, and play whatever that resolves to.
    async fn handle_request(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        audio: Vec<f32>,
    ) {
        if audio.is_empty() {
            tracing::debug!(guild_id = %guild_id, "ready for a request, but nothing was said");
            return;
        }

        // One timing for the whole request, from the audio to the first sample
        // out of the speaker. Every stage below records into it, so the single
        // line it logs says where the wait went.
        let timing = crate::timing::Timing::default();
        self.handle_request_timed(ctx, config, guild_id, audio, &timing)
            .await;
        timing.log("voice");
    }

    async fn handle_request_timed(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        audio: Vec<f32>,
        timing: &crate::timing::Timing,
    ) {
        let models = Arc::clone(&self.models);
        // Already loaded by the wake word, so this is a lookup in all but the
        // case where the first load failed.
        let loaded = tokio::task::spawn_blocking(move || models.get_or_load()).await;
        let transcriber = match loaded {
            Ok(Ok(transcriber)) => transcriber,
            Ok(Err(e)) => {
                tracing::error!(guild_id = %guild_id, error = %format!("{e:#}"), "no larger model available");
                return;
            }
            Err(e) => {
                tracing::error!(guild_id = %guild_id, error = %format!("{e:#}"), "model lookup task failed");
                return;
            }
        };

        let inferring = Instant::now();
        let inferred = tokio::task::spawn_blocking(move || transcriber.transcribe(&audio)).await;
        // Recorded before the outcome is examined, so a transcription that
        // failed still costs the time it actually cost.
        timing.record("transcription", inferring.elapsed());
        let transcript = match inferred {
            Ok(Ok(transcript)) => transcript,
            Ok(Err(e)) => {
                tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "transcription failed");
                return;
            }
            Err(e) => {
                tracing::error!(guild_id = %guild_id, error = %format!("{e:#}"), "transcription task failed");
                return;
            }
        };

        if transcript.trim().is_empty() {
            self.muted_utterances
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::debug!(guild_id = %guild_id, "heard a request but no words in it (too quiet, or too short?)");
            return;
        }
        let skipped = self
            .muted_utterances
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        if skipped > 0 {
            tracing::info!(
                guild_id = %guild_id,
                transcript = %transcript,
                skipped = skipped,
                "request heard"
            );
        } else {
            tracing::info!(guild_id = %guild_id, transcript = %transcript, "request heard");
        }

        let (Some(request), rewritten) = self
            .request_for(ctx, config, guild_id, &transcript, timing)
            .await
        else {
            return;
        };
        tracing::info!(guild_id = %guild_id, request = %request.query, "playing a request");
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
    }

    /// Turns a transcript into a request, cleaned by the rewriting stage when
    /// one is configured.
    ///
    /// Every failure of that stage falls back to the words that were actually
    /// heard, which is the whole pre-rewriting behaviour: a model that is down,
    /// slow or unsure must not be the reason a request goes unplayed.
    async fn request_for(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        transcript: &str,
        timing: &crate::timing::Timing,
    ) -> (Option<PlayRequest>, Option<String>) {
        // The rewrite is a *second candidate transcript*, not a replacement.
        // Both are resolved and the better verdict wins, so a rewriting model
        // can rescue a request the catalog cannot read, while still never being
        // able to talk the bot into a song the catalog did not find.
        let rewritten = self
            .rewrite(ctx, config, guild_id, transcript, timing)
            .await
            // The reply may still carry a wake word, which is why the grammar is
            // applied to it rather than trusting it to have stripped one.
            .and_then(|text| self.parser.strip_command(&text));
        if let Some(rewritten) = &rewritten {
            tracing::info!(
                guild_id = %guild_id,
                rewrite = %rewritten,
                "the transcript rewrite is a second candidate for this request"
            );
        }
        (self.parse_heard(guild_id, transcript), rewritten)
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
        // Both of these return early when the stage is off, so nothing is
        // recorded: a summary line without `llm_rewrite` is the stage being
        // unconfigured, not the stage being free.
        let endpoint = config.llm_endpoint.as_deref()?;
        let http = {
            let data = ctx.data.read().await;
            data.get::<HttpClientKey>().cloned()
        }?;
        let rewriting = Instant::now();
        let reply = llm::rewrite(&http, endpoint, &config.llm_model, transcript).await;
        // Recorded whatever the outcome, because a rewrite that failed is
        // still a rewrite that was waited for.
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

    /// The grammar, as applied to what was heard.
    ///
    /// Also keeps the split-command case: a "bot play" with a pause before the
    /// title is two captures, and the second is only recognisable because of
    /// the first.
    fn parse_heard(&self, guild_id: GuildId, transcript: &str) -> Option<PlayRequest> {
        match self.parser.parse_with_reason(transcript) {
            Ok(request) => {
                self.awaiting_query.clear();
                Some(request)
            }
            Err(ParseMiss::EmptyRequest) => {
                self.awaiting_query.expect();
                tracing::info!(guild_id = %guild_id, "heard the wake word and \"play\"; waiting for a title");
                None
            }
            Err(miss) => {
                let completed = self
                    .awaiting_query
                    .take()
                    .then(|| self.parser.request_from_query(transcript))
                    .flatten();
                match completed {
                    Some(request) => {
                        tracing::info!(
                            guild_id = %guild_id,
                            transcript = %transcript,
                            "taking this as the title for the waiting command"
                        );
                        Some(request)
                    }
                    None => {
                        let wake_words = self
                            .parser
                            .wake_words()
                            .iter()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ");
                        tracing::debug!(
                            guild_id = %guild_id,
                            reason = %miss,
                            wake_words = %wake_words,
                            "not a play command"
                        );
                        None
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_can_only_be_claimed_once() {
        let pending = PendingQuery::default();
        assert!(
            !pending.take(),
            "nothing is pending before a command is heard"
        );

        pending.expect();
        assert!(
            pending.take(),
            "the next utterance should complete the command"
        );
        assert!(!pending.take(), "a later utterance must not claim it again");
    }

    #[test]
    fn a_pending_title_expires() {
        let pending = PendingQuery::default();
        pending.expect();
        *pending.since.lock().unwrap() =
            Some(Instant::now() - PENDING_QUERY_TTL - Duration::from_millis(1));

        assert!(
            !pending.take(),
            "a title spoken long after the command should not be played"
        );
    }

    #[test]
    fn a_complete_command_clears_the_pending_title() {
        let pending = PendingQuery::default();
        pending.expect();
        pending.clear();
        assert!(!pending.take());
    }
}
