use shamash::bot;
use shamash::config::Config;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// What to log when `RUST_LOG` says nothing.
///
/// `warn` is the floor, so every dependency's failures stay visible; the `info`
/// lines on top are only the ones worth reading while the bot runs: the models
/// it loaded and the requests it handled. Serenity and songbird log a line per
/// connection and per packet at info, which drowns those out.
const DEFAULT_LOG: &str = "warn,shamash::pipeline=info,shamash::direct=info,shamash::bot=info";

/// Whether an event is the RTCP warning that cannot be acted on.
///
/// Songbird logs every RTCP packet it cannot decrypt at warn. Discord sends
/// RTCP reports for streams the bot has no key for, so this fires continuously
/// on a working connection, and there is no configuration to turn it off: it is
/// a bare `warn!` inside the driver's receive task.
///
/// Matching the message rather than the target is deliberate.
#[derive(Clone)]
struct WithoutRtcpNoise<S> {
    inner: S,
}

impl<S> WithoutRtcpNoise<S> {
    fn new(inner: S) -> Self {
        Self { inner }
    }
}

fn is_rtcp_noise(event: &tracing::Event<'_>) -> bool {
    use tracing::field::Visit;
    // The message lives in the event's fields, not in the call site metadata
    // `Layer::enabled` sees, which is why this cannot be a filter.
    struct Message(String);
    impl Visit for Message {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "message" {
                self.0 = value.to_string();
            }
        }
    }
    let mut message = Message(String::new());
    event.record(&mut message);
    event.metadata().target() == "songbird::driver::tasks::udp_rx"
        && message.0.starts_with("RTCP decryption failed")
}

impl<S: tracing::Subscriber, I: tracing_subscriber::Layer<S>> tracing_subscriber::Layer<S>
    for WithoutRtcpNoise<I>
{
    /// Drops the noise and hands everything else on untouched.
    ///
    /// Everything else here is forwarding. `RTP decryption failed` and
    /// `Decode error for SSRC` are logged from this same module and both mean
    /// the bot has stopped hearing, which is exactly the failure that must stay
    /// loud -- so this cannot be done by filtering the module away.
    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        if !is_rtcp_noise(event) {
            self.inner.on_event(event, ctx);
        }
    }
    fn enabled(
        &self,
        meta: &tracing::Metadata<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) -> bool {
        tracing_subscriber::Layer::enabled(&self.inner, meta, ctx)
    }
    fn on_new_span(
        &self,
        span: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        self.inner.on_new_span(span, id, ctx)
    }
    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        self.inner.on_record(id, values, ctx)
    }
    fn on_enter(&self, id: &tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        self.inner.on_enter(id, ctx)
    }
    fn on_exit(&self, id: &tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        self.inner.on_exit(id, ctx)
    }
    fn on_close(&self, id: tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        self.inner.on_close(id, ctx)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Songbird, serenity and reqwest all report through `tracing`. Without a
    // subscriber their errors are discarded, and a bot that never finishes
    // connecting to the voice server fails silently while looking healthy.
    //
    // RUST_LOG overrides the default: `RUST_LOG=info` for everything, or
    // `RUST_LOG=debug` for songbird's per-packet detail.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| DEFAULT_LOG.into()),
        )
        .with(WithoutRtcpNoise::new(tracing_subscriber::fmt::layer()))
        .init();

    // Load DISCORD_TOKEN and friends from a .env file, if present. Real
    // environment variables take precedence over values in the file.
    dotenvy::dotenv().ok();
    bot::start(Config::from_env()?).await
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_LOG, WithoutRtcpNoise};
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;

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

    /// What the bot would actually print for `message` at warn, from the target
    /// songbird uses.
    fn printed_for(message: &str) -> String {
        use std::sync::{Arc, Mutex};
        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = Arc::clone(&buffer);
        let writer = move || {
            let sink = Arc::clone(&sink);
            MutexWriter(sink)
        };
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(WithoutRtcpNoise::new(
                tracing_subscriber::fmt::layer().with_writer(writer),
            )),
            || {
                tracing::warn!(target: "songbird::driver::tasks::udp_rx", "{message}");
            },
        );
        // Not `try_unwrap`: tracing may still hold a clone of the buffer when
        // this runs, and that made the read come back empty at random.
        let out = buffer.lock().unwrap().clone();
        String::from_utf8_lossy(&out).into_owned()
    }

    struct MutexWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for MutexWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The RTCP warning is noise: Discord sends RTCP for streams the bot has no
    /// key for, so it fires continuously on a working connection.
    #[test]
    fn the_rtcp_decryption_warning_is_not_printed() {
        let printed = printed_for("RTCP decryption failed: Crypto(Error)");
        assert!(printed.is_empty(), "still reaching stdout: {printed}");
    }

    /// The same module logs failures that mean the bot has stopped hearing.
    /// Those must stay loud, or a silent bot looks healthy.
    #[test]
    fn the_driver_warnings_that_matter_are_still_printed() {
        for message in [
            "RTP decryption failed: Crypto(Error)",
            "Decode error for SSRC 1234: Opus(Other)",
            "Illegal UDP packet from voice server.",
            "Failed to parse message of type Rtp(Unknown).",
        ] {
            let printed = printed_for(message);
            assert!(
                printed.contains(message),
                "{message} was dropped; it means the bot may have stopped hearing"
            );
        }
    }

    /// A warning of ours that happens to begin the same way is not songbird's,
    /// and must not be dropped.
    #[test]
    fn our_own_warnings_are_still_printed() {
        use std::sync::{Arc, Mutex};
        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = Arc::clone(&buffer);
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(WithoutRtcpNoise::new(
                tracing_subscriber::fmt::layer()
                    .with_writer(move || MutexWriter(Arc::clone(&sink))),
            )),
            || {
                tracing::warn!(target: "shamash::pipeline", "RTCP decryption failed: ours");
            },
        );
        let out = buffer.lock().unwrap().clone();
        assert!(
            String::from_utf8_lossy(&out).contains("ours"),
            "our own event was dropped by songbird's filter"
        );
    }
}
