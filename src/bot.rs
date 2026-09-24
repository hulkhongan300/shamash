use crate::config::Config;
use crate::voice::{VoiceService, decode_config};
use serenity::async_trait;
use serenity::builder::{
    CreateCommand, CreateInteractionResponse, CreateInteractionResponseMessage,
};
use serenity::model::application::{Command, Interaction};
use serenity::model::gateway::Ready;
use serenity::model::id::ChannelId;
use serenity::model::voice::VoiceState;
use serenity::prelude::*;
use songbird::SerenityInit;

pub async fn start(config: Config) -> anyhow::Result<()> {
    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;
    let mut client = Client::builder(&config.discord_token, intents)
        .event_handler(Handler)
        .register_songbird_from_config(decode_config())
        .type_map_insert::<crate::state::ConfigKey>(config)
        .await?;
    client.start().await?;
    Ok(())
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
        let Interaction::Command(command) = interaction else {
            return;
        };
        if command.data.name != "stop" {
            return;
        }

        let response = match VoiceService::from_ctx(&ctx).await {
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
}
