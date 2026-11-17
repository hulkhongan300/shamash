use shamash::bot;
use shamash::config::Config;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Default filter: `warn` for everything, `info` only for our own modules
/// (serenity and songbird log per packet at info).
const DEFAULT_LOG: &str =
    "warn,shamash::pipeline=info,shamash::direct=info,shamash::bot=info,shamash::voice=info";

/// Drops songbird's "RTCP decryption failed" warning, which fires constantly on
/// a healthy connection and has no config switch.
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
    // The message is in event fields, invisible to `Layer::enabled`, so no filter.
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
    /// Not a module filter: the same module logs RTP/decode failures that must stay loud.
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
    // Load .env before building the log filter so a RUST_LOG set there is honoured.
    let dotenv = dotenvy::dotenv();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| DEFAULT_LOG.into()),
        )
        .with(WithoutRtcpNoise::new(tracing_subscriber::fmt::layer()))
        .init();

    if let Err(e) = dotenv
        && !e.not_found()
    {
        tracing::warn!(error = %e, "could not read .env");
    }
    bot::start(Config::from_env()?).await
}

#[cfg(test)]
#[path = "tests/main.rs"]
mod tests;
