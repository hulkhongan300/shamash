use crate::config::Config;
use crate::listener::{VoiceTickHandler, run_listener};
use crate::state::ConfigKey;
use anyhow::Context;
use serenity::model::id::{ChannelId, GuildId};
use serenity::prelude::Context as SerenityContext;
use songbird::Songbird;
use songbird::driver::{Channels, DecodeConfig, DecodeMode, SampleRate};
use songbird::events::CoreEvent;
use std::sync::Arc;
use tokio::sync::mpsc;

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

    /// Joins `channel` and starts listening for speech. A fresh listener is
    /// spawned per join; it exits automatically when the call is removed.
    pub async fn join_and_listen(
        &self,
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

        // TODO(step 4): transcribe, parse, and play instead of just logging.
        tokio::spawn(run_listener(rx, |utterance| {
            println!("captured utterance of {} samples", utterance.len());
        }));
        Ok(())
    }

    /// Reconciles bot presence with the configured target channel: joins when
    /// members are present, leaves when it empties.
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

        if member_count == 0 {
            if connected_to_target {
                println!("Voice channel {target} is empty; leaving");
                self.manager
                    .remove(guild_id)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to leave voice channel: {e}"))?;
            }
        } else if !connected_to_target {
            self.join_and_listen(guild_id, target).await?;
            println!("Joined voice channel {target}");
        }
        Ok(())
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
