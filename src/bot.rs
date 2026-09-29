use crate::config::{AsrEngine, Config};
use crate::dashboard;
use crate::direct::DirectTranscriber;
use crate::parakeet::HandyTranscriber;
use crate::player::Player;
use crate::state::{ConfigKey, HttpClientKey, PlayerKey, TranscriberKey};
use crate::transcriber::Transcriber;
use crate::voice::{VoiceService, decode_config};
use serenity::async_trait;
use serenity::builder::{
    CreateCommand, CreateInteractionResponse, CreateInteractionResponseMessage,
};
use serenity::model::application::{Command, ComponentInteraction, Interaction};
use serenity::model::gateway::Ready;
use serenity::model::id::ChannelId;
use serenity::model::voice::VoiceState;
use serenity::prelude::*;
use songbird::SerenityInit;
use std::sync::Arc;

pub async fn start(config: Config) -> anyhow::Result<()> {
    let transcriber: Arc<dyn Transcriber> = match config.asr_engine {
        AsrEngine::Direct => Arc::new(DirectTranscriber::new(
            &config.parakeet_model,
            config.asr_language.as_deref(),
        )?),
        AsrEngine::Handy => Arc::new(HandyTranscriber::new(&config.parakeet_model)),
    };
    log_configuration(&config);

    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;
    let mut client = Client::builder(&config.discord_token, intents)
        .event_handler(Handler)
        .register_songbird_from_config(decode_config())
        .type_map_insert::<ConfigKey>(config)
        .type_map_insert::<HttpClientKey>(reqwest::Client::new())
        .type_map_insert::<TranscriberKey>(transcriber)
        .await?;

    client.start().await?;
    Ok(())
}

/// Prints the loaded settings so a misconfigured run is obvious at a glance.
fn log_configuration(config: &Config) {
    let mut wake_words: Vec<&str> = config.wake_words.iter().map(String::as_str).collect();
    wake_words.sort_unstable();
    println!("Shamash starting up");
    match config.asr_engine {
        AsrEngine::Direct => {
            println!("  asr engine:    transcribe.cpp (in-process, model resident)");
            println!("  parakeet model: {}", config.parakeet_model);
            if let Some(language) = &config.asr_language {
                println!("  asr language:  {language}");
            }
        }
        AsrEngine::Handy => {
            println!("  asr engine:    handy (parakeet, subprocess)");
            println!("  parakeet model: {}", config.parakeet_model);
        }
    }
    println!("  wake words:    {}", wake_words.join(", "));
    println!("  voice channel: {}", config.voice_channel_id);
    match config.alert_channel_id {
        Some(id) => println!("  text channel:  {id}"),
        None => println!("  text channel:  auto (system channel, else first text channel)"),
    }
    match std::process::Command::new("yt-dlp")
        .arg("--version")
        .output()
    {
        Ok(output) => println!(
            "  yt-dlp:        {}",
            String::from_utf8_lossy(&output.stdout).trim()
        ),
        Err(_) => println!("  yt-dlp:        NOT FOUND; playback will fail"),
    }
}

pub struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        println!("Shamash is online as {}", ready.user.name);
        let command =
            CreateCommand::new("stop").description("Stop playback and leave the voice channel");
        if let Err(e) = Command::create_global_command(&ctx.http, command).await {
            println!("failed to register /stop command: {e}");
        }

        // One player for the whole process, so the voice listener and the
        // dashboard buttons act on the same queue. Built here because that is
        // the first point the songbird manager can be reached.
        match songbird::get(&ctx).await {
            Some(manager) => {
                ctx.data
                    .write()
                    .await
                    .insert::<PlayerKey>(Arc::new(Player::new(manager)));
            }
            None => println!("songbird unavailable, playback is disabled"),
        }
    }

    async fn voice_state_update(&self, ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        let Some(guild_id) = new.guild_id else {
            return;
        };
        if new.user_id == ctx.cache.current_user().id {
            return;
        }

        let Ok(service) = VoiceService::from_ctx(&ctx).await else {
            return;
        };
        let target = ChannelId::new(service.config.voice_channel_id);
        let touches_target = new.channel_id == Some(target)
            || old.as_ref().and_then(|vs| vs.channel_id) == Some(target);
        if !touches_target {
            return;
        }

        if let Err(e) = service.sync_channel(&ctx, guild_id).await {
            println!("voice channel sync failed for {guild_id}: {e:#}");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(command) => self.slash_stop(&ctx, command).await,
            Interaction::Component(button) => self.dashboard_button(&ctx, button).await,
            // Modal submissions and autocomplete have no dashboard buttons.
            _ => {}
        }
    }
}

/// The interaction handlers, split out of the [`EventHandler`] impl because
/// they are plain methods rather than events.
impl Handler {
    /// Handles `/stop`, which leaves the voice channel as well as stopping.
    async fn slash_stop(
        &self,
        ctx: &Context,
        command: serenity::model::application::CommandInteraction,
    ) {
        if command.data.name != "stop" {
            return;
        }

        let response = match VoiceService::from_ctx(ctx).await {
            Ok(service) => match command.guild_id {
                Some(guild_id) => {
                    let was_connected = service.manager.get(guild_id).is_some();
                    match service.stop(guild_id).await {
                        Ok(()) if was_connected => "Left the voice channel.".to_string(),
                        Ok(()) => "I wasn't in a voice channel.".to_string(),
                        Err(e) => format!("Failed to leave: {e}"),
                    }
                }
                None => "This command must be used in a server.".to_string(),
            },
            Err(e) => format!("Internal error: {e:#}"),
        };

        let reply = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new().content(response),
        );
        if let Err(e) = command.create_response(&ctx.http, reply).await {
            println!("failed to respond to /stop: {e}");
        }
    }

    /// Handles a press of one of the dashboard's buttons.
    ///
    /// The press is acknowledged straight away, because Discord fails an
    /// interaction that goes unanswered within three seconds and a skip may
    /// have to download nothing but still waits on the voice lock.
    async fn dashboard_button(&self, ctx: &Context, button: ComponentInteraction) {
        let Some(action) = dashboard::action_from_id(&button.data.custom_id) else {
            // Some other component's press; not ours to answer.
            return;
        };
        let Some(guild_id) = button.guild_id else {
            return;
        };
        if let Err(e) = button.defer(&ctx.http).await {
            println!("failed to acknowledge a dashboard button: {e}");
            return;
        }

        let config = {
            let data = ctx.data.read().await;
            data.get::<ConfigKey>().cloned()
        };
        let player = {
            let data = ctx.data.read().await;
            data.get::<PlayerKey>().cloned()
        };
        let (Some(config), Some(player)) = (config, player) else {
            println!("dashboard button pressed before the bot was ready");
            return;
        };

        match action {
            dashboard::ACTION_PAUSE | dashboard::ACTION_RESUME => {
                match player.toggle_pause(guild_id).await {
                    Some(true) => println!("[{guild_id}] paused"),
                    Some(false) => println!("[{guild_id}] resumed"),
                    None => println!("[{guild_id}] nothing to pause"),
                }
                player.draw_dashboard(ctx, &config, guild_id).await;
            }
            dashboard::ACTION_SKIP => {
                player.skip(ctx, &config, guild_id).await;
            }
            dashboard::ACTION_STOP => {
                player.stop(ctx, &config, guild_id).await;
            }
            // `action_from_id` only returns the four above.
            _ => {}
        }
    }
}
