use crate::config::{AsrEngine, Config};
use crate::dashboard;
use crate::direct::DirectTranscriber;
use crate::parakeet::HandyTranscriber;
use crate::player::Player;
use crate::resolve;
use crate::state::{BigModelsKey, ConfigKey, HttpClientKey, PlayerKey, TranscriberKey};
use crate::transcriber::Transcriber;
use crate::voice::{VoiceService, decode_config};
use crate::wake::BigModels;
use serenity::async_trait;
use serenity::builder::{
    CreateCommand, CreateInteractionResponse, CreateInteractionResponseMessage, EditMessage,
};
use serenity::model::application::{Command, ComponentInteraction, Interaction};
use serenity::model::gateway::Ready;
use serenity::model::id::{ChannelId, GuildId, UserId};
use serenity::model::voice::VoiceState;
use serenity::prelude::*;
use songbird::SerenityInit;
use std::sync::Arc;

pub async fn start(config: Config) -> anyhow::Result<()> {
    // Two models, because they pay for being loaded differently. The wake model
    // is resident from startup: it has to be ready before anyone speaks, and it
    // runs on every window of speech, so a load per command would cost far more
    // than it saves. The larger one is loaded on the first wake word instead,
    // which is what lets the bot tell the speaker when it is ready for a request.
    let wake_model: Arc<dyn Transcriber> = build_transcriber(
        config.asr_engine,
        &config.wake_model,
        config.asr_language.as_deref(),
    )?;
    let big_models = Arc::new(BigModels::new(
        config.asr_engine,
        &config.parakeet_model,
        config.asr_language.as_deref(),
    ));
    log_configuration(&config, &big_models);

    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;
    let mut client = Client::builder(&config.discord_token, intents)
        .event_handler(Handler)
        .register_songbird_from_config(decode_config())
        .type_map_insert::<ConfigKey>(config)
        .type_map_insert::<HttpClientKey>(reqwest::Client::new())
        .type_map_insert::<TranscriberKey>(wake_model)
        .type_map_insert::<BigModelsKey>(big_models)
        .await?;

    client.start().await?;
    Ok(())
}

/// Builds one speech-to-text engine from a model spec.
fn build_transcriber(
    engine: AsrEngine,
    model: &str,
    language: Option<&str>,
) -> anyhow::Result<Arc<dyn Transcriber>> {
    Ok(match engine {
        AsrEngine::Direct => Arc::new(DirectTranscriber::new(model, language)?),
        AsrEngine::Handy => Arc::new(HandyTranscriber::new(model)),
    })
}

/// Prints the loaded settings so a misconfigured run is obvious at a glance.
fn log_configuration(config: &Config, big_models: &BigModels) {
    let mut wake_words: Vec<&str> = config.wake_words.iter().map(String::as_str).collect();
    wake_words.sort_unstable();
    println!("Shamash starting up");
    match config.asr_engine {
        AsrEngine::Direct => {
            println!("  asr engine:    transcribe.cpp (in-process, model resident)");
            println!("  wake model:    {}", config.wake_model);
            println!(
                "  request model: {} (loaded on the first wake word)",
                config.parakeet_model
            );
            if let Some(language) = &config.asr_language {
                println!("  asr language:  {language}");
            }
        }
        AsrEngine::Handy => {
            println!("  asr engine:    handy (parakeet, subprocess)");
            println!("  wake model:    {}", config.wake_model);
            println!(
                "  request model: {} (loaded on the first wake word)",
                config.parakeet_model
            );
        }
    }
    println!(
        "  rewrite model: {}",
        config
            .llm_endpoint
            .as_deref()
            .map(|endpoint| format!("{} ({})", config.llm_model, endpoint))
            .unwrap_or_else(|| "off, the transcript is used as heard".to_string())
    );
    println!("  wake words:    {}", wake_words.join(", "));
    println!("  voice channel: {}", config.voice_channel_id);
    match config.alert_channel_id {
        Some(id) => println!("  text channel:  {id}"),
        None => println!("  text channel:  auto (system channel, else first text channel)"),
    }
    println!(
        "  request model loaded: {}",
        if big_models.is_loaded() { "yes" } else { "no" }
    );
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
        let manager = match songbird::get(&ctx).await {
            Some(manager) => manager,
            None => {
                println!("songbird unavailable, playback is disabled");
                return;
            }
        };
        // The catalog turns a spoken request into a real song's names and
        // running time before anything is searched for. A failure here is worth
        // reporting rather than working around: without it the bot still plays
        // songs, but it will pick the wrong one much more often.
        let config = ctx.data.read().await.get::<ConfigKey>().cloned();
        let Some(config) = config else {
            println!("config unavailable, playback is disabled");
            return;
        };
        match resolve::Catalog::from_config(&config) {
            Ok(catalog) => {
                ctx.data
                    .write()
                    .await
                    .insert::<PlayerKey>(Arc::new(Player::new(manager, Arc::new(catalog))));
            }
            Err(e) => println!("could not reach the song catalog, playback is disabled: {e:#}"),
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

/// The reply a member outside the bot's voice channel gets.
///
/// Ephemeral, because it answers one person and would otherwise be a public
/// message about someone else's failed button press.
pub const NOT_IN_VOICE: &str =
    "Join the voice channel I'm in first — that's where playback is controlled.";

/// Whether `user_id` may drive playback for this guild.
///
/// Everything that acts on playback — Skip, Stop, Pause/Resume, the "which song
/// did you mean" buttons and `/stop` — is a control surface for the people in
/// the channel the music is playing in, and for nobody else. The dashboard is a
/// text-channel message, so without this anyone who can read it could stop
/// someone else's music.
async fn may_control(ctx: &Context, guild_id: GuildId, user_id: UserId) -> bool {
    match VoiceService::from_ctx(ctx).await {
        Ok(service) => service.is_in_bot_channel(ctx, guild_id, user_id),
        Err(e) => {
            tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "could not check the voice channel");
            false
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

        let response = match command.guild_id {
            Some(guild_id) if may_control(ctx, guild_id, command.user.id).await => {
                match VoiceService::from_ctx(ctx).await {
                    Ok(service) => {
                        let was_connected = service.manager.get(guild_id).is_some();
                        match service.stop(guild_id).await {
                            Ok(()) if was_connected => "Left the voice channel.".to_string(),
                            Ok(()) => "I wasn't in a voice channel.".to_string(),
                            Err(e) => format!("Failed to leave: {e}"),
                        }
                    }
                    Err(e) => format!("Internal error: {e:#}"),
                }
            }
            Some(_) => NOT_IN_VOICE.to_string(),
            None => "This command must be used in a server.".to_string(),
        };

        let reply = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new().content(response),
        );
        if let Err(e) = command.create_response(&ctx.http, reply).await {
            tracing::warn!("failed to respond to /stop: {e}");
        }
    }

    /// Handles a press of one of the dashboard's buttons.
    ///
    /// The press is acknowledged straight away, because Discord fails an
    /// interaction that goes unanswered within three seconds and a skip may
    /// have to download nothing but still waits on the voice lock.
    /// Plays the song behind a "which one did you mean" button.
    async fn choose_song(&self, ctx: &Context, button: ComponentInteraction, video_id: &str) {
        let Some(guild_id) = button.guild_id else {
            return;
        };
        if !may_control(ctx, guild_id, button.user.id).await {
            tracing::info!(
                guild_id = %guild_id,
                user = %button.user.id,
                "a song choice was pressed from outside the voice channel"
            );
            refuse(ctx, button).await;
            return;
        }
        if let Err(e) = button.defer(&ctx.http).await {
            tracing::warn!("failed to acknowledge a song choice: {e}");
            return;
        }
        let (config, player) = {
            let data = ctx.data.read().await;
            (
                data.get::<ConfigKey>().cloned(),
                data.get::<PlayerKey>().cloned(),
            )
        };
        let (Some(config), Some(player)) = (config, player) else {
            tracing::warn!("song choice pressed before the bot was ready");
            return;
        };

        match player.play_chosen(ctx, &config, guild_id, video_id).await {
            Ok(()) => tracing::info!(guild_id = %guild_id, "played the chosen song"),
            Err(e) => {
                tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "could not play the chosen song");
                // The choice is spent either way, so the buttons go rather than
                // offering a second press that cannot do anything.
                let edit = EditMessage::new().components(Vec::new());
                if let Err(e) = ctx
                    .http
                    .edit_message(button.channel_id, button.message.id, &edit, Vec::new())
                    .await
                {
                    tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "could not clear the song choice");
                }
            }
        }
    }

    async fn dashboard_button(&self, ctx: &Context, button: ComponentInteraction) {
        // Checked before the playback actions, because a choice button is not
        // one of them and would be rejected by `action_from_id`.
        if let Some(video_id) =
            dashboard::picked_video_id(&button.data.custom_id).map(str::to_owned)
        {
            return self.choose_song(ctx, button, &video_id).await;
        }
        let Some(action) = dashboard::action_from_id(&button.data.custom_id) else {
            // Some other component's press; not ours to answer.
            return;
        };
        let Some(guild_id) = button.guild_id else {
            return;
        };
        if !may_control(ctx, guild_id, button.user.id).await {
            tracing::info!(
                guild_id = %guild_id,
                user = %button.user.id,
                action = action,
                "a dashboard button was pressed from outside the voice channel"
            );
            refuse(ctx, button).await;
            return;
        }
        if let Err(e) = button.defer(&ctx.http).await {
            tracing::warn!("failed to acknowledge a dashboard button: {e}");
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
            tracing::warn!("dashboard button pressed before the bot was ready");
            return;
        };

        match action {
            dashboard::ACTION_PAUSE | dashboard::ACTION_RESUME => {
                match player.toggle_pause(guild_id).await {
                    Some(true) => tracing::info!(guild_id = %guild_id, "paused"),
                    Some(false) => tracing::info!(guild_id = %guild_id, "resumed"),
                    None => tracing::info!(guild_id = %guild_id, "nothing to pause"),
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

/// Tells one person, privately, that they cannot drive playback from here.
///
/// Ephemeral rather than a public reply, so pressing Skip from the wrong
/// channel does not announce itself to the room.
async fn refuse(ctx: &Context, button: ComponentInteraction) {
    let reply = CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content(NOT_IN_VOICE)
            .ephemeral(true),
    );
    if let Err(e) = button.create_response(&ctx.http, reply).await {
        tracing::warn!("could not refuse a button press: {e}");
    }
}
