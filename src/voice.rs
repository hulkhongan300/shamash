use crate::config::Config;
use crate::listener::{VoiceTickHandler, run_listener};
use crate::parser::CommandParser;
use crate::pipeline::ListenerPipeline;
use crate::player::Player;
use crate::state::{ConfigKey, HttpClientKey, TranscriberKey};
use anyhow::Context;
use serenity::model::id::{ChannelId, GuildId};
use serenity::prelude::Context as SerenityContext;
use songbird::Songbird;
use songbird::driver::{Channels, DecodeConfig, DecodeMode, SampleRate};
use songbird::events::CoreEvent;
use std::sync::Arc;
use std::time::Duration;
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
            println!(
                "  deaf: {}, mute: {}, voice server: {}",
                handler.is_deaf(),
                handler.is_mute(),
                if connected {
                    "connected"
                } else {
                    "NOT CONNECTED after 10s"
                },
            );
        }
        if !connected {
            println!(
                "No audio can arrive while the voice server connection is missing; \
                 check RUST_LOG=debug output for the reason."
            );
        }

        let (transcriber, http) = {
            let data = ctx.data.read().await;
            let transcriber = data
                .get::<TranscriberKey>()
                .cloned()
                .context("transcriber not present in type map")?;
            let http = data
                .get::<HttpClientKey>()
                .cloned()
                .context("http client not present in type map")?;
            (transcriber, http)
        };
        let pipeline = Arc::new(ListenerPipeline::new(
            transcriber,
            CommandParser::new(self.config.wake_words.clone()),
            Arc::new(Player {
                manager: self.manager.clone(),
                http,
            }),
        ));

        let ctx = ctx.clone();
        let config = self.config.clone();
        let mut wake_words: Vec<&str> = config.wake_words.iter().map(String::as_str).collect();
        wake_words.sort_unstable();
        println!("Listening in {channel} for: {}", wake_words.join(", "));
        tokio::spawn(run_listener(rx, {
            let pipeline = pipeline.clone();
            let ctx = ctx.clone();
            let config = config.clone();
            move |utterance| {
                let pipeline = pipeline.clone();
                let ctx = ctx.clone();
                let config = config.clone();
                tokio::spawn(async move {
                    pipeline
                        .handle_utterance(&ctx, &config, guild_id, utterance)
                        .await;
                });
            }
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
            self.join_and_listen(ctx, guild_id, target).await?;
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
