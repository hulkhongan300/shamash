use crate::config::Config;
use crate::listener::VoiceTickHandler;
use crate::parser::CommandParser;
use crate::pipeline::ListenerPipeline;
use crate::player::Player;
use crate::state::{BigModelsKey, ConfigKey, PlayerKey, TranscriberKey};
use crate::wake::{WakeGate, run_wake_gate};
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
/// Pause between join attempts.
const JOIN_RETRY_DELAY: Duration = Duration::from_secs(3);

/// Builds the songbird driver config that decodes incoming voice to mono
/// 16 kHz PCM, the format Whisper expects.
pub fn decode_config() -> songbird::Config {
    songbird::Config::default().decode_mode(DecodeMode::Decode(DecodeConfig::new(
        Channels::Mono,
        SampleRate::Hz16000,
    )))
}

/// High-level voice-channel control: join/leave a target channel and keep a
/// listener task running for the duration of the call.
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

    /// The shared player, so the dashboard buttons and the voice listener act
    /// on the same queue.
    async fn player(&self, ctx: &SerenityContext) -> anyhow::Result<Arc<Player>> {
        let data = ctx.data.read().await;
        data.get::<PlayerKey>()
            .cloned()
            .context("player not present in type map")
    }

    /// Joins `channel` and starts listening for speech. A fresh listener is
    /// spawned per join; it exits automatically when the call is removed.
    pub async fn join_and_listen(
        &self,
        ctx: &SerenityContext,
        guild_id: GuildId,
        channel: ChannelId,
    ) -> anyhow::Result<()> {
        let call = self
            .manager
            .join(guild_id, channel)
            .await
            .map_err(|e| anyhow::anyhow!("failed to join voice channel {channel}: {e}"))?;

        let (tx, rx) = mpsc::unbounded_channel();
        {
            let mut handler = call.lock().await;
            handler.add_global_event(CoreEvent::VoiceTick.into(), VoiceTickHandler::new(tx));
        }

        // songbird's join() returns as soon as the gateway request is sent, so
        // a call that never reaches Discord's voice server is indistinguishable
        // from a working one. Wait for the connection and report the result,
        // because everything downstream depends on it.
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

        let wake_model = {
            let data = ctx.data.read().await;
            data.get::<TranscriberKey>()
                .cloned()
                .context("wake word model not present in type map")?
        };
        let big_models = {
            let data = ctx.data.read().await;
            data.get::<BigModelsKey>()
                .cloned()
                .context("larger model not present in type map")?
        };
        let parser = CommandParser::new(self.config.wake_words.clone());
        let gate = WakeGate::new(wake_model, parser.clone());
        let pipeline = Arc::new(ListenerPipeline::new(
            big_models,
            parser,
            self.player(ctx).await?,
        ));

        let ctx = ctx.clone();
        let config = self.config.clone();
        let mut wake_words: Vec<&str> = config.wake_words.iter().map(String::as_str).collect();
        wake_words.sort_unstable();
        tracing::info!(
            guild_id = %guild_id,
            channel = %channel,
            wake_words = %wake_words.join(", "),
            "listening for a wake word"
        );
        // The callback is awaited inside the gate loop rather than spawned, so a
        // request cannot be acted on while the model for its wake word is still
        // loading. Audio arriving in that window waits in the channel.
        tokio::spawn(run_wake_gate(gate, rx, {
            let pipeline = pipeline.clone();
            let ctx = ctx.clone();
            let config = config.clone();
            move |heard| {
                let pipeline = Arc::clone(&pipeline);
                let ctx = ctx.clone();
                let config = config.clone();
                async move {
                    pipeline.handle(&ctx, &config, guild_id, heard).await;
                }
            }
        }));
        Ok(())
    }

    /// Reconciles bot presence with the configured target channel: joins when
    /// members are present, leaves when it empties.
    ///
    /// Called both from `voice_state_update` and from `guild_create`, so a
    /// channel that was already occupied when the bot started counts the same
    /// as one somebody walks into afterwards.
    pub async fn sync_channel(
        &self,
        ctx: &SerenityContext,
        guild_id: GuildId,
    ) -> anyhow::Result<()> {
        let target = ChannelId::new(self.config.voice_channel_id);
        let bot_id = ctx.cache.current_user().id;

        let member_count = ctx
            .cache
            .guild(guild_id)
            .map(|guild| {
                guild
                    .voice_states
                    .values()
                    .filter(|vs| vs.channel_id == Some(target) && vs.user_id != bot_id)
                    .count()
            })
            .unwrap_or(0);

        let connected_to_target = match self.manager.get(guild_id) {
            Some(call) => {
                let handler = call.lock().await;
                handler.current_channel() == Some(target.into())
            }
            None => false,
        };

        match Self::what_to_do(member_count, connected_to_target) {
            Presence::Join => {
                self.join_with_retry(ctx, guild_id, target).await?;
                tracing::info!(guild_id = %guild_id, channel = %target, "joined the voice channel");
            }
            Presence::Leave => {
                tracing::info!(guild_id = %guild_id, channel = %target, "the voice channel is empty; leaving");
                self.manager
                    .remove(guild_id)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to leave voice channel: {e}"))?;
            }
            Presence::None => {}
        }
        Ok(())
    }

    /// What presence in the target channel should be reconciled to.
    ///
    /// Join whenever anyone is there, which is what makes a channel that was
    /// already occupied at startup count the same as one somebody just walked
    /// into. Leaving is only for a channel that has emptied, and a bot that is
    /// already where it should be is left alone rather than rejoined.
    fn what_to_do(member_count: usize, connected_to_target: bool) -> Presence {
        match (member_count, connected_to_target) {
            (0, true) => Presence::Leave,
            (0, false) => Presence::None,
            (_, true) => Presence::None,
            (_, false) => Presence::Join,
        }
    }

    /// Joins `channel`, retrying a few times before giving up.
    ///
    /// Discord's gateway sometimes takes longer than songbird's 10 s timeout to
    /// answer, and one such timeout used to leave the bot outside the channel
    /// for good: it only ever joins in response to a voice state change, so a
    /// user already sitting in the channel would never trigger another attempt.
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

    /// Whether `user_id` is currently in this guild's bot voice channel.
    ///
    /// The gate for every playback control. The dashboard is a text-channel
    /// message, so without this anyone in the server — or anyone who can see
    /// the message at all — could press Skip or Stop on someone else's music,
    /// and `/stop` could be used to kick the bot out from under the people who
    /// asked for it.
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

    /// The rule itself.
    ///
    /// `channel` is the voice channel the member is in, or `None` when they are
    /// not in one at all -- no cached voice state, or connected but deaf to
    /// everything.
    ///
    /// Split from [`Self::is_in_bot_channel`] so it can be exercised without a
    /// gateway: serenity's `Cache` populates itself from events and offers no
    /// way to insert one in a test.
    ///
    /// `None` is refused rather than allowed. The cache can lag a join or a
    /// move, and the cost of wrongly allowing is that anyone can stop anyone
    /// else's music; the cost of wrongly refusing is one person has to press
    /// the button again.
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
mod tests {
    use super::*;
    const BOT_CHANNEL: u64 = 4242;
    const OTHER_CHANNEL: u64 = 9;

    fn may_drive(channel: Option<u64>) -> bool {
        VoiceService::member_is_in(channel.map(ChannelId::new), ChannelId::new(BOT_CHANNEL))
    }

    /// The rule the dashboard needs: someone in the bot's channel may press
    /// Skip, and nobody else may.
    #[test]
    fn only_members_in_the_bot_channel_may_drive_playback() {
        assert!(may_drive(Some(BOT_CHANNEL)), "in the channel, so allowed");
        assert!(!may_drive(Some(OTHER_CHANNEL)), "in another channel");
        // Connected but nowhere to hear it, or not connected at all: both are a
        // member the cache has no channel for.
        assert!(!may_drive(None));
    }

    /// Someone already in the channel when the bot starts must be enough to
    /// make it join, not only somebody who moves afterwards.
    #[test]
    fn an_occupied_channel_is_joined_without_anyone_moving() {
        assert_eq!(
            VoiceService::what_to_do(1, false),
            Presence::Join,
            "one person already there and the bot outside"
        );
        assert_eq!(
            VoiceService::what_to_do(1, true),
            Presence::None,
            "already where it should be"
        );
    }

    /// Leaving is only for a channel that has emptied. Rejoining a live channel
    /// on every sync would tear down the call the music is playing in.
    #[test]
    fn only_an_emptied_channel_is_left() {
        assert_eq!(VoiceService::what_to_do(0, true), Presence::Leave);
        assert_eq!(
            VoiceService::what_to_do(0, false),
            Presence::None,
            "nothing to leave"
        );
        assert_eq!(VoiceService::what_to_do(2, true), Presence::None);
    }
}
