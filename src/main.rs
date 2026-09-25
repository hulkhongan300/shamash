use shamash::bot;
use shamash::config::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load DISCORD_TOKEN and friends from a .env file, if present. Real
    // environment variables take precedence over values in the file.
    dotenvy::dotenv().ok();
    bot::start(Config::from_env()?).await
}
