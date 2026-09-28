use shamash::bot;
use shamash::config::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Songbird, serenity and reqwest all report through `tracing`. Without a
    // subscriber their errors are discarded, and a bot that never finishes
    // connecting to the voice server fails silently while looking healthy.
    // Set RUST_LOG=debug for songbird's per-packet detail.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Load DISCORD_TOKEN and friends from a .env file, if present. Real
    // environment variables take precedence over values in the file.
    dotenvy::dotenv().ok();
    bot::start(Config::from_env()?).await
}
