use shamash::bot;
use shamash::config::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    bot::start(Config::from_env()?).await
}
