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
/// Pause between join attempts.
const JOIN_RETRY_DELAY: Duration = Duration::from_secs(3);
/// How long a channel must stay empty before the bot leaves it.
const LEAVE_GRACE: Duration = Duration::from_secs(15);

/// Builds the songbird driver config that decodes incoming voice to mono
/// 16 kHz PCM, the format the voice detector and the models expect.
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
        // Everything the listener needs is gathered before joining. Joining
        // first and failing here used to leave the bot in the channel, deaf,
        // and a retry then joined again on top of it.
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
            // A rejoin reuses the call, and its handlers with it. Without this
            // every rejoin added another receiver of the same audio.
            handler.remove_all_global_events();
            handler.add_global_event(CoreEvent::VoiceTick.into(), VoiceTickHandler::new(tx));
            handler.add_global_event(
                CoreEvent::SpeakingStateUpdate.into(),
                SpeakingHandler(speakers.clone()),
            );
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

    /// Reconciles bot presence with the configured target channel: joins when
    /// people are present, leaves whatever channel it is in once nobody is.
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
        match self.presence(ctx, guild_id).await {
            Presence::Join => {
                self.join_with_retry(ctx, guild_id, target).await?;
                tracing::info!(guild_id = %guild_id, channel = %target, "joined the voice channel");
            }
            Presence::Leave => {
                // Someone dropping out for a second, a network blip or a
                // client restart, must not cost everyone the queue.
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

    /// What presence should be reconciled to right now.
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

    /// The people in `channel`: other bots, this one included, do not count,
    /// or a second bot in the channel would keep this one there forever.
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

    /// Leaves an empty channel, the way `/stop` would: the queue is cleared and
    /// the dashboard says nothing is playing, rather than showing a song nobody
    /// can hear.
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

    /// What presence should be reconciled to.
    ///
    /// `in_target` is how many people are in the configured channel, and
    /// `here` how many are in the channel the bot is connected to, if it is in
    /// one. That is usually the same channel, but not when someone dragged
    /// the bot elsewhere, and an emptied channel is left either way.
    ///
    /// Join whenever anyone is in the target, which is what makes a channel
    /// that was already occupied at startup count the same as one somebody
    /// just walked into. A bot that is with people is left alone rather than
    /// rejoined.
    fn what_to_do(in_target: usize, here: Option<usize>) -> Presence {
        match here {
            Some(0) => Presence::Leave,
            Some(_) => Presence::None,
            None if in_target > 0 => Presence::Join,
            None => Presence::None,
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
            VoiceService::what_to_do(1, None),
            Presence::Join,
            "one person already there and the bot outside"
        );
        assert_eq!(
            VoiceService::what_to_do(1, Some(1)),
            Presence::None,
            "already where it should be"
        );
    }

    /// Leaving is only for a channel that has emptied. Rejoining a live channel
    /// on every sync would tear down the call the music is playing in.
    #[test]
    fn only_an_emptied_channel_is_left() {
        assert_eq!(VoiceService::what_to_do(0, Some(0)), Presence::Leave);
        assert_eq!(
            VoiceService::what_to_do(0, None),
            Presence::None,
            "nothing to leave"
        );
        assert_eq!(VoiceService::what_to_do(2, Some(2)), Presence::None);
    }

    /// Dragged into another channel, the bot stays while people are with it
    /// and leaves once they are gone, whoever is in its own channel.
    #[test]
    fn a_bot_moved_elsewhere_leaves_when_that_channel_empties() {
        assert_eq!(VoiceService::what_to_do(3, Some(1)), Presence::None);
        assert_eq!(VoiceService::what_to_do(3, Some(0)), Presence::Leave);
        assert_eq!(VoiceService::what_to_do(0, Some(2)), Presence::None);
    }
}
