use crate::config::{AsrEngine, Config};
use crate::dashboard;
use crate::direct::DirectTranscriber;
use crate::parakeet::HandyTranscriber;
use crate::player::Player;
use crate::resolve;
use crate::search;
use crate::state::{BigModelsKey, ConfigKey, HttpClientKey, PlayerKey, TranscriberKey};
use crate::transcriber::Transcriber;
use crate::voice::{VoiceService, decode_config};
use crate::wake::BigModels;
use serenity::all::AutocompleteChoice;
use serenity::async_trait;
use serenity::builder::{
    CreateAutocompleteResponse, CreateCommand, CreateCommandOption, CreateInteractionResponse,
    CreateInteractionResponseMessage, EditInteractionResponse,
};
use serenity::model::application::{
    CommandInteraction, CommandOptionType, ComponentInteraction, Interaction, ResolvedValue,
};
use serenity::model::gateway::Ready;
use serenity::model::id::{ChannelId, GuildId, MessageId, UserId};
use serenity::model::voice::VoiceState;
use serenity::prelude::*;
use songbird::SerenityInit;
use std::sync::Arc;
use std::time::Duration;

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

/// Logs the loaded settings so a misconfigured run is obvious at a glance.
///
/// One `info!` line per setting rather than a block of prints: `RUST_LOG` then
/// controls this along with everything else, and the settings can be filtered
/// out of a busy log.
fn log_configuration(config: &Config, big_models: &BigModels) {
    let mut wake_words: Vec<&str> = config.wake_words.iter().map(String::as_str).collect();
    wake_words.sort_unstable();
    let asr = match config.asr_engine {
        AsrEngine::Direct => "transcribe.cpp (in-process, model resident)",
        AsrEngine::Handy => "handy (parakeet, subprocess)",
    };
    let rewrite = config
        .llm_endpoint
        .as_deref()
        .map(|endpoint| format!("{} ({})", config.llm_model, endpoint))
        .unwrap_or_else(|| "off, the transcript is used as heard".to_string());
    let text_channel = match config.alert_channel_id {
        Some(id) => id.to_string(),
        None => "auto (system channel, else first text channel)".to_string(),
    };
    let yt_dlp = match std::process::Command::new("yt-dlp")
        .arg("--version")
        .output()
    {
        Ok(output) => String::from_utf8_lossy(&output.stdout).trim().to_string(),
        Err(_) => "NOT FOUND; playback will fail".to_string(),
    };

    tracing::info!(
        asr_engine = asr,
        wake_model = %config.wake_model,
        request_model = %config.parakeet_model,
        request_model_loaded = big_models.is_loaded(),
        asr_language = config.asr_language.as_deref().unwrap_or("auto"),
        rewrite_model = %rewrite,
        wake_words = %wake_words.join(", "),
        voice_channel = config.voice_channel_id,
        text_channel = %text_channel,
        resolve_budget_ms = config.resolve_budget.as_millis() as u64,
        yt_dlp = %yt_dlp,
        "Shamash starting up"
    );
}

pub struct Handler;

/// The slash commands, in one place so both registrations cannot disagree.
fn commands() -> Vec<CreateCommand> {
    vec![
        CreateCommand::new("stop").description("Stop playback and leave the voice channel"),
        CreateCommand::new("play")
            .description("Search the catalog for a song and choose from the results")
            .add_option(
                CreateCommandOption::new(CommandOptionType::String, "song", "What to search for")
                    .required(true)
                    .set_autocomplete(true),
            ),
    ]
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        tracing::info!(user = %ready.user.name, "Shamash is online");
        // Deliberately no global registration. `guild_create` registers per
        // guild, which is both instant -- a global command takes up to an hour
        // to reach a client -- and shadowed by the guild copy, so having both
        // only ever showed the same command twice. A guild copy works in any
        // guild the bot is invited to later, because GUILD_CREATE fires for
        // each of them on connect.

        // One player for the whole process, so the voice listener and the
        // dashboard buttons act on the same queue. Built here because that is
        // the first point the songbird manager can be reached.
        let manager = match songbird::get(&ctx).await {
            Some(manager) => manager,
            None => {
                tracing::error!("songbird unavailable, playback is disabled");
                return;
            }
        };
        // The catalog turns a spoken request into a real song's names and
        // running time before anything is searched for. A failure here is worth
        // reporting rather than working around: without it the bot still plays
        // songs, but it will pick the wrong one much more often.
        let config = ctx.data.read().await.get::<ConfigKey>().cloned();
        let Some(config) = config else {
            tracing::error!("config unavailable, playback is disabled");
            return;
        };
        match resolve::Catalog::from_config(&config) {
            Ok(catalog) => {
                ctx.data
                    .write()
                    .await
                    .insert::<PlayerKey>(Arc::new(Player::new(manager, Arc::new(catalog))));
            }
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "could not reach the song catalog, playback is disabled")
            }
        }
    }

    /// Reconciles presence as each guild arrives.
    ///
    /// Without this the bot only ever joins in reaction to somebody moving, so
    /// a channel that already had people in it when the bot started -- or a
    /// guild it was already in -- is never noticed until someone else joins or
    /// leaves. `GUILD_CREATE` is the first point at which that guild's voice
    /// states are in the cache, so it is where the count can be trusted.
    async fn guild_create(
        &self,
        ctx: Context,
        guild: serenity::model::guild::Guild,
        _is_new: Option<bool>,
    ) {
        // Global commands take up to an hour to reach a client, and a bot that
        // has just registered `/play` looks broken for all of it. A guild copy
        // appears at once, so anyone in the server can see what the bot offers
        // the moment it starts. This is a PUT, so it replaces rather than
        // duplicating.
        if let Err(e) = ctx.http.create_guild_commands(guild.id, &commands()).await {
            tracing::warn!(guild_id = %guild.id, error = %format!("{e:#}"), "failed to register the guild commands");
        }

        let Ok(service) = VoiceService::from_ctx(&ctx).await else {
            return;
        };
        if let Err(e) = service.sync_channel(&ctx, guild.id).await {
            tracing::warn!(guild_id = %guild.id, error = %format!("{e:#}"), "voice channel sync failed");
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
            tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "voice channel sync failed");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(command) => match command.data.name.as_str() {
                "play" => self.slash_play(&ctx, command).await,
                "stop" => self.slash_stop(&ctx, command).await,
                _ => {}
            },
            Interaction::Autocomplete(command) => {
                if command.data.name.as_str() == "play" {
                    self.slash_autocomplete_play(&ctx, command).await;
                }
            }
            Interaction::Component(button) => self.component(&ctx, button).await,
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

/// The reply to `/play` with no words in it.
const NOT_A_SEARCH: &str = "Tell me what to search for: `/play dracula`.";

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
    /// Handles a press of a `/play` chooser button, or of the dashboard's.
    ///
    /// The two are different lists with different owners and must not reach
    /// each other: a `/play` chooser belongs to whoever typed it, while the
    /// dashboard belongs to everyone in the voice channel.
    async fn component(&self, ctx: &Context, button: ComponentInteraction) {
        let id = button.data.custom_id.as_str();
        let action = dashboard::action_from_id(id).unwrap_or_default();
        if let Some(index) = dashboard::asked_index(id) {
            return self.asked_button(ctx, button, Asked::Song(index)).await;
        }
        if action == dashboard::ACTION_NEXT {
            return self.asked_button(ctx, button, Asked::Next).await;
        }
        if action == dashboard::ACTION_NOTHING {
            return self.asked_button(ctx, button, Asked::Dismiss).await;
        }
        self.dashboard_button(ctx, button).await
    }

    /// A press on the `/play` chooser: a song, the next page, or nothing.
    async fn asked_button(&self, ctx: &Context, button: ComponentInteraction, asked: Asked) {
        let Some(guild_id) = button.guild_id else {
            return;
        };
        if let Err(e) = button.defer(&ctx.http).await {
            tracing::warn!("failed to acknowledge a /play button: {e}");
            return;
        }
        let Some((config, player)) = ready(ctx).await else {
            tracing::warn!("a /play button was pressed before the bot was ready");
            return;
        };

        // A chooser raised by speech belongs to the voice channel, so it keeps the
        // channel's rule rather than a named person. `/play` is the other way
        // round and is checked by `ask_belongs_to` below.
        let spoken = player.ask_is_spoken(guild_id);
        let may_answer = if spoken {
            may_control(ctx, guild_id, button.user.id).await
        } else {
            player.ask_belongs_to(guild_id, button.user.id)
        };
        if !may_answer {
            tracing::info!(
                guild_id = %guild_id,
                user = %button.user.id,
                spoken,
                "a song was chosen by someone who may not answer this question"
            );
            refuse(ctx, button).await;
            return;
        }

        match asked {
            Asked::Song(index) => {
                if let Err(e) = player.play_asked(ctx, &config, guild_id, index).await {
                    tracing::warn!(guild_id = %guild_id, index, error = %format!("{e:#}"), "could not play that song");
                    refuse(ctx, button).await;
                } else {
                    drop_question(ctx, button.channel_id, button.message.id, guild_id).await;
                }
            }
            Asked::Next => {
                if !player.next_page(ctx, guild_id).await {
                    refuse(ctx, button).await;
                }
            }
            Asked::Dismiss => {
                player.dismiss_ask(guild_id);
                drop_question(ctx, button.channel_id, button.message.id, guild_id).await;
            }
        }
    }
    /// Handles `/play <song>`: searches the catalog and posts the results as a
    /// paged chooser for the asker to pick from.
    async fn slash_play(
        &self,
        ctx: &Context,
        command: serenity::model::application::CommandInteraction,
    ) {
        let Some(query) = typed_song(&command) else {
            reply(ctx, command, NOT_A_SEARCH).await;
            return;
        };
        let Some(guild_id) = command.guild_id else {
            reply(ctx, command, "This command only works in a server.").await;
            return;
        };
        let Some((config, player)) = ready(ctx).await else {
            reply(
                ctx,
                command,
                "I'm still starting up — try again in a moment.",
            )
            .await;
            return;
        };

        // The bot has to be in the voice channel to play anything. Saying so
        // before the search saves an API round trip and a wall of buttons that
        // could not do anything.
        if !may_control(ctx, guild_id, command.user.id).await {
            reply(ctx, command, NOT_IN_VOICE).await;
            return;
        }

        // If the query is a specific video selected from autocomplete (or a direct URL),
        // play it directly without needing another chooser.
        if let Some(video_id) = search::url_video_id(&query) {
            let _ = command.defer_ephemeral(&ctx.http).await;
            match player
                .play_direct(ctx, &config, guild_id, &query, video_id)
                .await
            {
                Ok((title, starting)) => {
                    let text = if starting {
                        format!("Playing **{title}**")
                    } else {
                        format!("Queued **{title}**")
                    };
                    let _ = command
                        .edit_response(&ctx.http, EditInteractionResponse::new().content(text))
                        .await;
                }
                Err(e) => {
                    tracing::warn!(guild_id = %guild_id, query = %query, error = %format!("{e:#}"), "could not play direct video");
                    let _ = command
                        .edit_response(
                            &ctx.http,
                            EditInteractionResponse::new()
                                .content(format!("Could not play that song: {e:#}")),
                        )
                        .await;
                }
            }
            return;
        }

        match player
            .search_and_ask(ctx, &config, guild_id, &query, command.user.id)
            .await
        {
            Ok(found) => {
                reply(
                    ctx,
                    command,
                    &format!(
                        "Found {found} song{} for `{query}`. Pick one below.",
                        if found == 1 { "" } else { "s" }
                    ),
                )
                .await;
            }
            Err(e) => {
                tracing::info!(guild_id = %guild_id, query = %query, error = %format!("{e:#}"), "/play could not answer");
                // The catalog may well have found something and the post may
                // have been refused, which is what this used to report as
                // "nothing in the catalog" -- true of the second, false of the
                // first, and the two look identical from the outside.
                reply(
                    ctx,
                    command,
                    &format!("I couldn't send those results: {e:#}"),
                )
                .await;
            }
        }
    }

    /// Answers slash command autocomplete queries for `/play song:<query>`,
    /// displaying catalog suggestions in the top popup menu for arrow-key selection.
    async fn slash_autocomplete_play(&self, ctx: &Context, command: CommandInteraction) {
        let query = command
            .data
            .options
            .iter()
            .find(|opt| opt.name == "song")
            .and_then(|opt| opt.value.as_str())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        if query.is_empty() {
            let response = CreateInteractionResponse::Autocomplete(
                CreateAutocompleteResponse::new().set_choices(Vec::new()),
            );
            let _ = command.create_response(&ctx.http, response).await;
            return;
        }

        let Some((_config, player)) = ready(ctx).await else {
            let response = CreateInteractionResponse::Autocomplete(
                CreateAutocompleteResponse::new().set_choices(Vec::new()),
            );
            let _ = command.create_response(&ctx.http, response).await;
            return;
        };

        let tracks = tokio::time::timeout(Duration::from_millis(2500), player.autocomplete(&query))
            .await
            .unwrap_or_default();

        let choices: Vec<AutocompleteChoice> = tracks
            .into_iter()
            .take(25)
            .map(|track| {
                let name = format_autocomplete_name(&track);
                let value = if let Some(video_id) = &track.video_id {
                    format!("https://youtu.be/{video_id}")
                } else {
                    format!("{} by {}", track.title, track.artist)
                };
                AutocompleteChoice::new(name, value)
            })
            .collect();

        let response = CreateInteractionResponse::Autocomplete(
            CreateAutocompleteResponse::new().set_choices(choices),
        );
        if let Err(e) = command.create_response(&ctx.http, response).await {
            tracing::debug!(error = %format!("{e:#}"), "failed to respond to autocomplete");
        }
    }

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
    async fn dashboard_button(&self, ctx: &Context, button: ComponentInteraction) {
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
            // Viewing pages. Neither plays anything, so neither needs the
            // playback lock that Skip and Stop take.
            dashboard::ACTION_QUEUE => player.show_queue(ctx, &config, guild_id).await,
            dashboard::ACTION_BACK => player.show_now_playing(ctx, &config, guild_id).await,
            dashboard::ACTION_REPEAT => {
                let repeat = player.toggle_repeat(ctx, &config, guild_id).await;
                tracing::info!(guild_id = %guild_id, repeat, "toggled repeat");
            }
            // `action_from_id` covers every live action above.
            _ => {}
        }
    }
}

/// Takes the bot's own "which one did you mean" question out of the channel
/// once it has been answered.
///
/// Only the question is ever passed here. The dashboard is edited in place and
/// is meant to stay. A bot may delete its own messages without any extra
/// permission, so nothing else is needed to clean up after itself.
async fn drop_question(
    ctx: &Context,
    channel_id: ChannelId,
    message_id: MessageId,
    guild_id: GuildId,
) {
    if let Err(e) = ctx.http.delete_message(channel_id, message_id, None).await {
        tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "could not clear the song choice");
    }
}

/// What a press on a `/play` chooser means.
#[derive(Debug, Clone, Copy)]
enum Asked {
    Song(usize),
    Next,
    Dismiss,
}

/// The settings and the player, once both are in the type map.
async fn ready(ctx: &Context) -> Option<(crate::config::Config, Arc<crate::player::Player>)> {
    let data = ctx.data.read().await;
    match (
        data.get::<ConfigKey>().cloned(),
        data.get::<PlayerKey>().cloned(),
    ) {
        (Some(config), Some(player)) => Some((config, player)),
        _ => None,
    }
}

/// The words after `/play`, if the option carried any.
fn typed_song(command: &CommandInteraction) -> Option<String> {
    let words = command
        .data
        .options()
        .iter()
        .find_map(|option| match option.value {
            ResolvedValue::String(text) => Some(text.to_string()),
            _ => None,
        })?;
    let query = words.trim().to_string();
    (!query.is_empty()).then_some(query)
}

/// Answers a slash command privately.
async fn reply(ctx: &Context, command: CommandInteraction, text: &str) {
    let response = CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content(text)
            .ephemeral(true),
    );
    if let Err(e) = command.create_response(&ctx.http, response).await {
        tracing::warn!(error = %format!("{e:#}"), "could not answer a command");
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

/// Formats a catalog track for a Discord autocomplete choice label (max 100 characters).
fn format_autocomplete_name(track: &resolve::CatalogTrack) -> String {
    let mut name = format!("{} — {}", track.artist, track.title);
    if let Some(duration) = track.duration_secs {
        let total = duration.max(0.0) as u64;
        let dur = format!(" ({}:{:02})", total / 60, total % 60);
        if name.chars().count() + dur.chars().count() <= 100 {
            name.push_str(&dur);
        }
    }
    if name.chars().count() > 100 {
        name = name.chars().take(97).collect::<String>() + "...";
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autocomplete_name_includes_artist_title_and_duration() {
        let track = resolve::CatalogTrack {
            artist: "Queen".to_string(),
            title: "Bohemian Rhapsody".to_string(),
            duration_secs: Some(355.0),
            popularity: None,
            video_id: Some("fJ9rUzIMcZQ".to_string()),
        };
        assert_eq!(
            format_autocomplete_name(&track),
            "Queen — Bohemian Rhapsody (5:55)"
        );
    }

    #[test]
    fn autocomplete_name_clamps_to_100_chars() {
        let track = resolve::CatalogTrack {
            artist: "A".repeat(60),
            title: "B".repeat(60),
            duration_secs: Some(120.0),
            popularity: None,
            video_id: Some("123".to_string()),
        };
        let formatted = format_autocomplete_name(&track);
        assert!(formatted.chars().count() <= 100);
        assert!(formatted.ends_with("..."));
    }
}
