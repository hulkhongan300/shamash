use crate::config::Config;
use crate::listener::{Speakers, SpeakingHandler, VoiceTickHandler};
use crate::parser::CommandParser;
use crate::pipeline::ListenerPipeline;
use crate::player::Player;
use crate::state::{BigModelsKey, ConfigKey, ListenLockKey, PlayerKey, TranscriberKey};
use crate::wake::{Armed, WakeGate, run_wake_gate};
use anyhow::Context;
use serenity::model::id::{ChannelId, GuildId, UserId};
use serenity::prelude::Context as SerenityContext;
use songbird::Songbird;
use songbird::driver::{Channels, DecodeConfig, DecodeMode, SampleRate};
use songbird::events::CoreEvent;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// The reconciliation `sync_channel` performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    Join,
    Leave,
    None,
}

/// How many times a join is attempted before the failure is reported.
const JOIN_ATTEMPTS: usize = 3;
const JOIN_RETRY_DELAY: Duration = Duration::from_secs(3);
/// How long a channel must stay empty before the bot leaves it.
const LEAVE_GRACE: Duration = Duration::from_secs(15);

/// Songbird config decoding voice to mono 16 kHz, the format the models expect.
pub fn decode_config() -> songbird::Config {
    songbird::Config::default().decode_mode(DecodeMode::Decode(DecodeConfig::new(
        Channels::Mono,
        SampleRate::Hz16000,
    )))
}

/// Joins/leaves the target channel and runs a listener for the call.
pub struct VoiceService {
    pub manager: Arc<Songbird>,
    pub config: Config,
}

impl VoiceService {
    pub async fn from_ctx(ctx: &SerenityContext) -> anyhow::Result<Self> {
        let config = {
            let data = ctx.data.read().await;
            data.get::<ConfigKey>()
                .cloned()
                .context("config not present in type map")?
        };
        let manager = songbird::get(ctx)
            .await
            .context("songbird not registered in type map")?;
        Ok(Self { manager, config })
    }

    async fn player(&self, ctx: &SerenityContext) -> anyhow::Result<Arc<Player>> {
        let data = ctx.data.read().await;
        data.get::<PlayerKey>()
            .cloned()
            .context("player not present in type map")
    }

    /// Joins `channel` and spawns a listener that exits when the call is removed.
    pub async fn join_and_listen(
        &self,
        ctx: &SerenityContext,
        guild_id: GuildId,
        channel: ChannelId,
    ) -> anyhow::Result<()> {
        // Gathered before joining, so a failure can't leave the bot in the channel deaf.
        let (wake_model, big_models, lock) = {
            let data = ctx.data.read().await;
            (
                data.get::<TranscriberKey>()
                    .cloned()
                    .context("wake word model not present in type map")?,
                data.get::<BigModelsKey>()
                    .cloned()
                    .context("larger model not present in type map")?,
                data.get::<ListenLockKey>()
                    .cloned()
                    .context("listen lock not present in type map")?,
            )
        };
        let player = self.player(ctx).await?;

        let call = self
            .manager
            .join(guild_id, channel)
            .await
            .map_err(|e| anyhow::anyhow!("failed to join voice channel {channel}: {e}"))?;

        let (tx, rx) = mpsc::unbounded_channel();
        let speakers = Speakers::default();
        {
            let mut handler = call.lock().await;
            // A rejoin reuses the call; without this each rejoin adds another receiver.
            handler.remove_all_global_events();
            handler.add_global_event(CoreEvent::VoiceTick.into(), VoiceTickHandler::new(tx));
            handler.add_global_event(
                CoreEvent::SpeakingStateUpdate.into(),
                SpeakingHandler(speakers.clone()),
            );
        }

        // join() returns once the gateway request is sent; wait for the real connection.
        let mut connected = false;
        for _ in 0..50 {
            if call.lock().await.current_connection().is_some() {
                connected = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        {
            let handler = call.lock().await;
            tracing::info!(
                guild_id = %guild_id,
                deaf = handler.is_deaf(),
                mute = handler.is_mute(),
                voice_server = if connected { "connected" } else { "NOT CONNECTED after 10s" },
                "joined the voice channel"
            );
        }
        if !connected {
            tracing::error!(
                guild_id = %guild_id,
                "no audio can arrive while the voice server connection is missing; \
                 set RUST_LOG=shamash=debug for the reason"
            );
        }

        let parser = CommandParser::new(self.config.wake_words.clone());
        let armed = Armed::default();
        let gate = WakeGate::new(wake_model, parser.clone(), armed.clone());
        let pipeline = Arc::new(ListenerPipeline::new(
            big_models, parser, player, armed, speakers,
        ));
        let inbox = pipeline.start(ctx.clone(), self.config.clone(), guild_id);

        let mut wake_words: Vec<&str> = self.config.wake_words.iter().map(String::as_str).collect();
        wake_words.sort_unstable();
        tracing::info!(
            guild_id = %guild_id,
            channel = %channel,
            wake_words = %wake_words.join(", "),
            locked = lock.is_locked(guild_id),
            "listening for a wake word"
        );
        tokio::spawn(run_wake_gate(
            gate,
            rx,
            move || lock.is_locked(guild_id),
            move |heard| {
                let _ = inbox.send(heard);
            },
        ));
        Ok(())
    }

    /// Joins the target channel when people are there; leaves its channel once empty.
    pub async fn sync_channel(
        &self,
        ctx: &SerenityContext,
        guild_id: GuildId,
    ) -> anyhow::Result<()> {
        let target = ChannelId::new(self.config.voice_channel_id);
        match self.presence(ctx, guild_id).await {
            Presence::Join => {
                self.join_with_retry(ctx, guild_id, target).await?;
                tracing::info!(guild_id = %guild_id, channel = %target, "joined the voice channel");
            }
            Presence::Leave => {
                // A brief dropout must not cost everyone the queue.
                tokio::time::sleep(LEAVE_GRACE).await;
                if self.presence(ctx, guild_id).await != Presence::Leave {
                    tracing::debug!(guild_id = %guild_id, "someone came back; staying");
                    return Ok(());
                }
                self.leave_empty(ctx, guild_id).await?;
            }
            Presence::None => {}
        }
        Ok(())
    }

    async fn presence(&self, ctx: &SerenityContext, guild_id: GuildId) -> Presence {
        let target = ChannelId::new(self.config.voice_channel_id);
        let current = match self.manager.get(guild_id) {
            Some(call) => call
                .lock()
                .await
                .current_channel()
                .map(|c| ChannelId::new(c.0.get())),
            None => None,
        };
        let in_target = self.people_in(ctx, guild_id, target);
        let here = current.map(|channel| self.people_in(ctx, guild_id, channel));
        Self::what_to_do(in_target, here)
    }

    /// People in `channel`, excluding bots, so another bot can't keep this one there forever.
    fn people_in(&self, ctx: &SerenityContext, guild_id: GuildId, channel: ChannelId) -> usize {
        let me = ctx.cache.current_user().id;
        let Some(guild) = ctx.cache.guild(guild_id) else {
            return 0;
        };
        guild
            .voice_states
            .values()
            .filter(|vs| vs.channel_id == Some(channel) && vs.user_id != me)
            .filter(|vs| {
                let bot = vs
                    .member
                    .as_ref()
                    .map(|m| m.user.bot)
                    .or_else(|| guild.members.get(&vs.user_id).map(|m| m.user.bot));
                // Unknown counts as a person: staying is the safe mistake.
                !bot.unwrap_or(false)
            })
            .count()
    }

    /// Leaves an empty channel the way `/stop` would, clearing queue and dashboard.
    async fn leave_empty(&self, ctx: &SerenityContext, guild_id: GuildId) -> anyhow::Result<()> {
        if self.manager.get(guild_id).is_none() {
            return Ok(());
        }
        tracing::info!(guild_id = %guild_id, "nobody is left in the voice channel; leaving");
        let player = ctx.data.read().await.get::<PlayerKey>().cloned();
        if let Some(player) = player {
            player.release_duck(guild_id);
            player.stop(ctx, &self.config, guild_id).await;
        }
        self.stop(guild_id).await
    }

    /// `in_target`: people in the configured channel; `here`: people in the bot's
    /// current channel (may differ if it was dragged), if connected.
    fn what_to_do(in_target: usize, here: Option<usize>) -> Presence {
        match here {
            Some(0) => Presence::Leave,
            Some(_) => Presence::None,
            None if in_target > 0 => Presence::Join,
            None => Presence::None,
        }
    }

    /// Joins `channel` with retries: the gateway can exceed songbird's 10 s timeout.
    async fn join_with_retry(
        &self,
        ctx: &SerenityContext,
        guild_id: GuildId,
        channel: ChannelId,
    ) -> anyhow::Result<()> {
        let mut last = None;
        for attempt in 1..=JOIN_ATTEMPTS {
            match self.join_and_listen(ctx, guild_id, channel).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!(
                        guild_id = %guild_id,
                        attempt = attempt,
                        of = JOIN_ATTEMPTS,
                        error = %format!("{e:#}"),
                        "joining the voice channel failed; retrying"
                    );
                    last = Some(e);
                    tokio::time::sleep(JOIN_RETRY_DELAY).await;
                }
            }
        }
        Err(last.expect("at least one join attempt is always made"))
    }

    /// Whether `user_id` is in the bot's voice channel; gates every playback control.
    pub fn is_in_bot_channel(
        &self,
        ctx: &SerenityContext,
        guild_id: GuildId,
        user_id: UserId,
    ) -> bool {
        let target = ChannelId::new(self.config.voice_channel_id);
        let channel = ctx.cache.guild(guild_id).and_then(|guild| {
            guild
                .voice_states
                .get(&user_id)
                .and_then(|state| state.channel_id)
        });
        Self::member_is_in(channel, target)
    }

    /// The rule, split out because serenity's `Cache` can't be filled in tests.
    /// Unknown (`None`) is refused: a wrong refusal costs a retry, a wrong allow doesn't.
    pub fn member_is_in(channel: Option<ChannelId>, target: ChannelId) -> bool {
        channel == Some(target)
    }

    /// Stops playback and removes the bot from the guild's voice channel.
    pub async fn stop(&self, guild_id: GuildId) -> anyhow::Result<()> {
        if self.manager.get(guild_id).is_none() {
            return Ok(());
        }
        self.manager
            .remove(guild_id)
            .await
            .map_err(|e| anyhow::anyhow!("failed to leave voice channel: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "tests/voice.rs"]
mod tests;
