use crate::config::Config;
use crate::parser::PlayRequest;
use anyhow::Context;
use serenity::model::channel::ChannelType;
use serenity::model::id::{ChannelId, GuildId};
use serenity::prelude::Context as SerenityContext;
use songbird::Songbird;
use songbird::input::{Input, YoutubeDl};
use std::sync::Arc;

/// Plays music for a request and reports the outcome to a text channel.
pub struct Player {
    pub manager: Arc<Songbird>,
    pub http: reqwest::Client,
}

impl Player {
    /// Looks the query up on YouTube and plays the best audio match, replacing
    /// whatever is currently playing. Confirmation is posted to the alert
    /// channel when one is available.
    pub async fn play(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        request: &PlayRequest,
    ) -> anyhow::Result<()> {
        let call = self
            .manager
            .get(guild_id)
            .context("bot is not in a voice channel")?;
        let input = Input::Lazy(Box::new(YoutubeDl::new_search(
            self.http.clone(),
            request.query.clone(),
        )));
        {
            let mut handler = call.lock().await;
            handler.stop();
            handler.play_input(input);
        }

        let summary = match (request.title.as_deref(), request.artist.as_deref()) {
            (Some(title), Some(artist)) => format!("Playing {title} by {artist}."),
            _ => format!("\u{201c}{}\u{201d} searching on YouTube.", request.query),
        };
        println!("{summary}");

        if let Some(channel) = resolve_alert_channel(ctx, config, guild_id)
            && let Err(e) = channel.say(&ctx.http, &summary).await
        {
            println!("failed to post play confirmation: {e}");
        }
        Ok(())
    }
}

/// Picks the text channel used for feedback: the configured alert channel,
/// else the guild's system channel, else the first text channel.
fn resolve_alert_channel(
    ctx: &SerenityContext,
    config: &Config,
    guild_id: GuildId,
) -> Option<ChannelId> {
    if let Some(id) = config.alert_channel_id {
        return Some(ChannelId::new(id));
    }
    let guild = ctx.cache.guild(guild_id)?;
    guild.system_channel_id.or_else(|| {
        guild
            .channels
            .values()
            .find(|channel| channel.kind == ChannelType::Text)
            .map(|channel| channel.id)
    })
}
