use crate::config::Config;
use crate::parser::PlayRequest;
use crate::search;
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
    /// Searches YouTube for the most popular song matching the request and
    /// plays it, replacing whatever is currently playing. Confirmation is
    /// posted to the alert channel when one is available.
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
        let match_ = search::most_popular_song(&request.query, search::DEFAULT_CANDIDATES)
            .await
            .with_context(|| format!("no match for '{}'", request.query))?;
        let views = match match_.view_count {
            Some(views) => format!("{views} views"),
            None => "unknown views".to_string(),
        };
        let summary = format!(
            "Now playing \u{201c}{}\u{201d} by {} ({views})",
            match_.title(),
            match_.channel()
        );
        println!("[{guild_id}] {summary}");

        let input = Input::Lazy(Box::new(YoutubeDl::new(self.http.clone(), match_.url)));
        {
            let mut handler = call.lock().await;
            handler.stop();
            handler.play_input(input);
        }

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
