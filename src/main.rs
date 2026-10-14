use shamash::bot;
use shamash::config::Config;

/// What to log when `RUST_LOG` says nothing.
///
/// `warn` is the floor, so every dependency's failures stay visible; the `info`
/// lines on top are only the ones worth reading while the bot runs: the models
/// it loaded and the requests it handled. Serenity and songbird log a line per
/// connection and per packet at info, which drowns those out.
const DEFAULT_LOG: &str = "warn,shamash::pipeline=info,shamash::direct=info,shamash::bot=info";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Songbird, serenity and reqwest all report through `tracing`. Without a
    // subscriber their errors are discarded, and a bot that never finishes
    // connecting to the voice server fails silently while looking healthy.
    //
    // RUST_LOG overrides the default: `RUST_LOG=info` for everything, or
    // `RUST_LOG=debug` for songbird's per-packet detail.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| DEFAULT_LOG.into()),
        )
        .init();

    // Load DISCORD_TOKEN and friends from a .env file, if present. Real
    // environment variables take precedence over values in the file.
    dotenvy::dotenv().ok();
    bot::start(Config::from_env()?).await
}

#[cfg(test)]
mod tests {
    use super::DEFAULT_LOG;
    use tracing_subscriber::EnvFilter;

    /// The directives `DEFAULT_LOG` names, lowercased.
    fn directives() -> Vec<String> {
        DEFAULT_LOG
            .split(',')
            .map(|d| d.trim().to_lowercase())
            .collect()
    }

    /// A typo in `DEFAULT_LOG` panics at startup, because the fallback goes
    /// through `From<&str>`, which cannot fail. Parse it here instead.
    #[test]
    fn the_default_log_filter_parses() {
        let _: EnvFilter = DEFAULT_LOG.into();
        assert!(!directives().is_empty());
    }

    /// What was heard, and which models loaded.
    #[test]
    fn the_default_log_filter_keeps_the_pipeline_and_the_model_lines() {
        let d = directives();
        for target in ["shamash::pipeline", "shamash::direct", "shamash::bot"] {
            assert!(
                d.contains(&format!("{target}=info")),
                "{target} is silenced at info; DEFAULT_LOG is {DEFAULT_LOG}"
            );
        }
    }

    /// A dependency's chatter must not get through at info: it is a line per
    /// connection and per packet. Warnings still do, so a failure is never
    /// silent -- which is what the bare `warn` floor is for.
    #[test]
    fn the_default_log_filter_silences_dependency_chatter_but_keeps_warnings() {
        let d = directives();
        assert!(
            d.iter().any(|directive| directive == "warn"),
            "warnings are not the floor; DEFAULT_LOG is {DEFAULT_LOG}"
        );
        for target in ["serenity", "songbird", "reqwest", "hyper"] {
            assert!(
                !d.iter().any(|directive| directive.starts_with(target)),
                "{target} is not silenced at info; DEFAULT_LOG is {DEFAULT_LOG}"
            );
        }
    }
}
