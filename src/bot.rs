use crate::config::{AsrEngine, Config};
use crate::dashboard;
use crate::direct::DirectTranscriber;
use crate::parakeet::HandyTranscriber;
use crate::player::Player;
use crate::resolve;
use crate::state::{
    BigModelsKey, ConfigKey, HttpClientKey, ListenLock, ListenLockKey, PlayerKey, TranscriberKey,
};
use crate::transcriber::Transcriber;
use crate::voice::{VoiceService, decode_config};
use crate::wake::BigModels;
use serenity::all::AutocompleteChoice;
use serenity::async_trait;
use serenity::builder::{
    CreateAutocompleteResponse, CreateCommand, CreateCommandOption, CreateInteractionResponse,
    CreateInteractionResponseFollowup, CreateInteractionResponseMessage, EditInteractionResponse,
};
use serenity::model::application::{
    CommandInteraction, CommandOptionType, ComponentInteraction, ComponentInteractionDataKind,
    Interaction, ResolvedValue,
};
use serenity::model::gateway::Ready;
use serenity::model::id::{ChannelId, GuildId, MessageId, UserId};
use serenity::model::voice::VoiceState;
use serenity::prelude::*;
use songbird::SerenityInit;
use std::sync::Arc;
use std::time::Duration;

pub async fn start(config: Config) -> anyhow::Result<()> {
    // The wake model runs on every utterance so stays resident; the request model
    // loads on the first wake word. Both are downloaded now, not on first use.
    if config.asr_engine == AsrEngine::Direct {
        for spec in [&config.wake_model, &config.request_model] {
            crate::model::ensure(spec).await?;
        }
    }
    let wake_model: Arc<dyn Transcriber> = build_transcriber(
        config.asr_engine,
        &config.wake_model,
        config.asr_language.as_deref(),
    )?;
    let big_models = Arc::new(BigModels::new(
        config.asr_engine,
        &config.request_model,
        config.asr_language.as_deref(),
        config.asr_prompt.as_deref(),
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
        .type_map_insert::<ListenLockKey>(ListenLock::default())
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
        AsrEngine::Direct => Arc::new(DirectTranscriber::new(model, language, None)?),
        AsrEngine::Handy => Arc::new(HandyTranscriber::new(model)),
    })
}

/// Logs the loaded settings, one `info!` per setting, so a misconfigured run is obvious.
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
        request_model = %config.request_model,
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
        CreateCommand::new("lock")
            .description("Stop listening for voice commands (music keeps playing)"),
        CreateCommand::new("unlock").description("Start listening for voice commands again"),
        CreateCommand::new("play")
            .description("Play a song, or a whole YouTube playlist or album from its link")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "song",
                    "Start typing to pick a song, or paste a YouTube video, playlist or album link",
                )
                .required(true)
                .set_autocomplete(true),
            ),
    ]
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        tracing::info!(user = %ready.user.name, "Shamash is online");
        // No global registration: per-guild in `guild_create` is instant, and both would show twice.

        // One player shared by voice and dashboard; built here, where songbird is first reachable.
        let manager = match songbird::get(&ctx).await {
            Some(manager) => manager,
            None => {
                tracing::error!("songbird unavailable, playback is disabled");
                return;
            }
        };
        // Without the catalog the bot still plays, but picks the wrong song more often.
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
                    .insert::<PlayerKey>(Arc::new(player_with_memory(
                        Player::new(manager, Arc::new(catalog)),
                        &config,
                    )));
            }
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "could not reach the song catalog, playback is disabled")
            }
        }
    }

    /// Reconciles presence as each guild arrives, so already-occupied channels are joined.
    async fn guild_create(
        &self,
        ctx: Context,
        guild: serenity::model::guild::Guild,
        _is_new: Option<bool>,
    ) {
        // Guild commands appear at once (global ones take up to an hour); a PUT, so no duplicates.
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
        let Ok(service) = VoiceService::from_ctx(&ctx).await else {
            return;
        };
        let target = ChannelId::new(service.config.voice_channel_id);
        let me = ctx.cache.current_user().id;
        if new.user_id == me {
            // Only being dragged elsewhere needs handling: that channel may be empty.
            if new.channel_id.is_none() || new.channel_id == Some(target) {
                return;
            }
        } else {
            let touches = |channel: ChannelId| {
                new.channel_id == Some(channel)
                    || old.as_ref().and_then(|vs| vs.channel_id) == Some(channel)
            };
            let bot_channel = ctx.cache.guild(guild_id).and_then(|guild| {
                guild
                    .voice_states
                    .get(&me)
                    .and_then(|state| state.channel_id)
            });
            if !touches(target) && !bot_channel.is_some_and(touches) {
                return;
            }
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
                "lock" => self.slash_lock(&ctx, command, true).await,
                "unlock" => self.slash_lock(&ctx, command, false).await,
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

/// The (ephemeral) reply a member outside the bot's voice channel gets.
pub const NOT_IN_VOICE: &str =
    "Join the voice channel I'm in first — that's where playback is controlled.";

/// The reply to a press on a `/play` list someone else asked for.
const NOT_YOURS: &str =
    "That list belongs to whoever ran `/play`. Run `/play` yourself to pick a song.";

/// The reply to a press on a list the bot is no longer holding.
const NOT_ON_OFFER: &str = "That search is no longer on offer; run `/play` again.";

/// The reply to "Next page" when the page could not be turned.
const NO_NEXT_PAGE: &str = "There is no next page to show; run `/play` again.";

/// The reply to anything pressed before the bot has finished starting.
const STARTING_UP: &str = "I'm still starting up — try again in a moment.";

/// The reply to `/play` with no words in it.
const NOT_A_SEARCH: &str = "Tell me what to search for: `/play dracula`.";

/// Whether `user_id` may drive playback: only people in the bot's voice channel.
async fn may_control(ctx: &Context, guild_id: GuildId, user_id: UserId) -> bool {
    match VoiceService::from_ctx(ctx).await {
        Ok(service) => service.is_in_bot_channel(ctx, guild_id, user_id),
        Err(e) => {
            tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "could not check the voice channel");
            false
        }
    }
}

/// The interaction handlers, kept out of the [`EventHandler`] impl.
impl Handler {
    /// Handles a press of a `/play` chooser button, or of the dashboard's; they have different owners.
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
    ///
    /// Refusals are the one response; later errors go in a follow-up (Discord rejects a second response).
    async fn asked_button(&self, ctx: &Context, button: ComponentInteraction, asked: Asked) {
        let Some(guild_id) = button.guild_id else {
            return;
        };
        let Some((config, player)) = ready(ctx).await else {
            tracing::warn!("a /play button was pressed before the bot was ready");
            answer_privately(ctx, button, STARTING_UP).await;
            return;
        };

        // A spoken chooser belongs to the voice channel; `/play` is checked by `ask_belongs_to`.
        let on_offer = player.has_ask(guild_id);
        let spoken = player.ask_is_spoken(guild_id);
        let may_answer = if spoken {
            may_control(ctx, guild_id, button.user.id).await
        } else {
            player.ask_belongs_to(guild_id, button.user.id)
        };
        if let Some(text) = chooser_refusal(on_offer, spoken, may_answer) {
            tracing::info!(
                guild_id = %guild_id,
                user = %button.user.id,
                on_offer,
                spoken,
                "a song was chosen by someone who may not answer this question"
            );
            answer_privately(ctx, button, text).await;
            return;
        }
        if let Err(e) = button.defer(&ctx.http).await {
            tracing::warn!("failed to acknowledge a /play button: {e}");
            return;
        }

        match asked {
            Asked::Song(index) => {
                if let Err(e) = player.play_asked(ctx, &config, guild_id, index).await {
                    tracing::warn!(guild_id = %guild_id, index, error = %format!("{e:#}"), "could not play that song");
                    follow_up(ctx, &button, &format!("I couldn't play that song: {e:#}")).await;
                } else {
                    drop_question(ctx, button.channel_id, button.message.id, guild_id).await;
                }
            }
            Asked::Next => {
                if !player.next_page(ctx, guild_id).await {
                    follow_up(ctx, &button, NO_NEXT_PAGE).await;
                }
            }
            Asked::Dismiss => {
                player.dismiss_ask(guild_id);
                drop_question(ctx, button.channel_id, button.message.id, guild_id).await;
            }
        }
    }
    /// Handles `/play <song>`: searches the catalog and posts a paged chooser.
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

        // Check voice before searching, to skip a useless search.
        if !may_control(ctx, guild_id, command.user.id).await {
            reply(ctx, command, NOT_IN_VOICE).await;
            return;
        }

        // A playlist or album link queues the whole thing.
        if let Some(url) = crate::playlist::playlist_url(&query) {
            let _ = command.defer_ephemeral(&ctx.http).await;
            let text = match player.play_playlist(ctx, &config, guild_id, &url).await {
                Ok(queued) => queued.summary(),
                Err(e) => {
                    tracing::warn!(guild_id = %guild_id, url = %url, error = %format!("{e:#}"), "could not queue a playlist");
                    format!("I couldn't play that playlist: {e:#}")
                }
            };
            finish(ctx, &command, &text).await;
            return;
        }

        // A single-video link plays directly; typed words are a search.
        if let Some(video_id) = crate::playlist::video_id(&query) {
            let _ = command.defer_ephemeral(&ctx.http).await;
            match player
                .play_direct(ctx, &config, guild_id, &query, &video_id)
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

        // Defer first: the search can exceed Discord's 3 s answer deadline.
        if let Err(e) = command.defer_ephemeral(&ctx.http).await {
            tracing::warn!(error = %format!("{e:#}"), "could not acknowledge /play");
            return;
        }
        let started = std::time::Instant::now();
        let searched = player
            .search_and_ask(ctx, &config, guild_id, &query, command.user.id)
            .await;
        tracing::info!(
            guild_id = %guild_id,
            query = %query,
            found = searched.as_ref().ok(),
            took_ms = started.elapsed().as_millis() as u64,
            "/play"
        );
        match searched {
            Ok(found) => {
                finish(
                    ctx,
                    &command,
                    &format!(
                        "Found {found} song{} for `{query}`. Pick one below.",
                        if found == 1 { "" } else { "s" }
                    ),
                )
                .await;
            }
            Err(e) => {
                tracing::info!(guild_id = %guild_id, query = %query, error = %format!("{e:#}"), "/play could not answer");
                // The post failed, not necessarily the search; don't report "nothing found".
                finish(
                    ctx,
                    &command,
                    &format!("I couldn't send those results: {e:#}"),
                )
                .await;
            }
        }
    }

    /// Answers autocomplete for `/play song:<query>` with catalog suggestions.
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

    /// `/lock` and `/unlock`: switch voice listening off and on; gated and replied publicly.
    async fn slash_lock(&self, ctx: &Context, command: CommandInteraction, locked: bool) {
        let Some(guild_id) = command.guild_id else {
            reply(ctx, command, "This command only works in a server.").await;
            return;
        };
        if !may_control(ctx, guild_id, command.user.id).await {
            reply(ctx, command, NOT_IN_VOICE).await;
            return;
        }
        let lock = ctx.data.read().await.get::<ListenLockKey>().cloned();
        let Some(lock) = lock else {
            reply(
                ctx,
                command,
                "I'm still starting up — try again in a moment.",
            )
            .await;
            return;
        };

        let changed = lock.set(guild_id, locked);
        if locked && let Some(player) = ctx.data.read().await.get::<PlayerKey>().cloned() {
            // Anyone mid-request is not going to finish it now.
            player.release_duck(guild_id);
        }
        tracing::info!(guild_id = %guild_id, user = %command.user.id, locked, changed, "listening lock");
        let text = match (locked, changed) {
            (true, true) => {
                "🔒 Voice commands are off. I'm not listening to anything said in the channel. Use `/unlock` to turn them back on."
            }
            (true, false) => {
                "🔒 Voice commands were already off. Use `/unlock` to turn them back on."
            }
            (false, true) => "🔓 Voice commands are on. Say my name to ask for a song.",
            (false, false) => "🔓 I was already listening.",
        };
        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new().content(text),
        );
        if let Err(e) = command.create_response(&ctx.http, response).await {
            tracing::warn!(error = %format!("{e:#}"), "could not answer /lock");
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

    /// Handles a dashboard button press, acknowledged at once to beat Discord's 3 s deadline.
    async fn dashboard_button(&self, ctx: &Context, button: ComponentInteraction) {
        let id = button.data.custom_id.as_str();
        let press = if let Some(action) = dashboard::action_from_id(id) {
            Press::Action(action)
        } else if dashboard::is_pick(id) {
            Press::Pick(match &button.data.kind {
                ComponentInteractionDataKind::StringSelect { values } => dashboard::picked(values),
                _ => None,
            })
        } else if let Some((edit, song)) = dashboard::edit_from_id(id) {
            Press::Edit(edit, song)
        } else {
            // Some other component's press; not ours to answer.
            return;
        };
        let action = id.to_string();
        let Some(guild_id) = button.guild_id else {
            return;
        };
        if !may_control(ctx, guild_id, button.user.id).await {
            tracing::info!(
                guild_id = %guild_id,
                user = %button.user.id,
                action = %action,
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
        player.adopt_dashboard(guild_id, button.channel_id, button.message.id);

        let action = match press {
            Press::Action(action) => action,
            Press::Pick(selected) => {
                return player.pick_in_queue(ctx, &config, guild_id, selected).await;
            }
            Press::Edit(edit, song) => {
                return player.edit_queue(ctx, &config, guild_id, edit, song).await;
            }
        };
        match action {
            dashboard::ACTION_PAUSE | dashboard::ACTION_RESUME => {
                match player
                    .set_paused(guild_id, action == dashboard::ACTION_PAUSE)
                    .await
                {
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
            // Viewing pages needs no playback lock.
            dashboard::ACTION_QUEUE => player.show_queue(ctx, &config, guild_id).await,
            dashboard::ACTION_BACK => player.show_now_playing(ctx, &config, guild_id).await,
            dashboard::ACTION_REPEAT => {
                let repeat = player.toggle_repeat(ctx, &config, guild_id).await;
                tracing::info!(guild_id = %guild_id, repeat, "toggled repeat");
            }
            dashboard::ACTION_WRONG if !player.wrong_song(ctx, &config, guild_id).await => {
                tracing::info!(guild_id = %guild_id, "wrong song pressed, but nothing remembered is playing");
            }
            // `action_from_id` covers every live action above.
            _ => {}
        }
    }
}

/// Deletes the bot's answered "which one did you mean" question (never the dashboard).
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

/// A press on the dashboard: a button, a queue-menu pick, or a queue edit.
enum Press<'a> {
    Action(&'a str),
    Pick(Option<u64>),
    Edit(dashboard::Edit, u64),
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

/// Replaces the "thinking…" of a deferred command with its answer.
async fn finish(ctx: &Context, command: &CommandInteraction, text: &str) {
    let edit = EditInteractionResponse::new().content(text);
    if let Err(e) = command.edit_response(&ctx.http, edit).await {
        tracing::warn!(error = %format!("{e:#}"), "could not answer a deferred command");
    }
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
async fn refuse(ctx: &Context, button: ComponentInteraction) {
    answer_privately(ctx, button, NOT_IN_VOICE).await;
}

/// Why a chooser press may not be answered, or `None` when it may.
fn chooser_refusal(on_offer: bool, spoken: bool, may_answer: bool) -> Option<&'static str> {
    if !on_offer {
        Some(NOT_ON_OFFER)
    } else if may_answer {
        None
    } else if spoken {
        Some(NOT_IN_VOICE)
    } else {
        Some(NOT_YOURS)
    }
}

/// Answers an unacknowledged press privately; after `defer`, use [`follow_up`].
async fn answer_privately(ctx: &Context, button: ComponentInteraction, text: &str) {
    let reply = CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content(text)
            .ephemeral(true),
    );
    if let Err(e) = button.create_response(&ctx.http, reply).await {
        tracing::warn!("could not answer a button press: {e}");
    }
}

/// The private message sent after a press was already acknowledged.
fn private_follow_up(text: &str) -> CreateInteractionResponseFollowup {
    CreateInteractionResponseFollowup::new()
        .content(text)
        .ephemeral(true)
}

/// Tells the presser, privately, what happened to a press already acknowledged.
async fn follow_up(ctx: &Context, button: &ComponentInteraction, text: &str) {
    if let Err(e) = button
        .create_followup(&ctx.http, private_follow_up(text))
        .await
    {
        tracing::warn!("could not follow up a button press: {e}");
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

/// Gives the player its song memory, read from `LIBRARY_PATH`.
fn player_with_memory(mut player: Player, config: &Config) -> Player {
    match config.library_path.as_deref() {
        Some(path) => {
            player.library = Some(Arc::new(crate::library::Library::load(Some(path))));
        }
        None => tracing::info!("LIBRARY_PATH is off; picked songs are not remembered"),
    }
    player
}

#[cfg(test)]
#[path = "tests/bot.rs"]
mod tests;
