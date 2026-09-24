use serenity::async_trait;
use serenity::model::gateway::Ready;
use serenity::prelude::*;
use shamash::config::Config;

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        println!("Shamash is online as {}", ready.user.name);
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    let intents = GatewayIntents::GUILD_VOICE_STATES;
    let mut client = Client::builder(&config.discord_token, intents)
        .event_handler(Handler)
        .await?;

    client.start().await?;
    Ok(())
}
