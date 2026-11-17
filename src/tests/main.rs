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

/// A typo in `DEFAULT_LOG` would panic at startup via `From<&str>`.
#[test]
fn the_default_log_filter_parses() {
    let _: EnvFilter = DEFAULT_LOG.into();
    assert!(!directives().is_empty());
}

#[test]
fn the_default_log_filter_keeps_the_pipeline_and_the_model_lines() {
    let d = directives();
    for target in [
        "shamash::pipeline",
        "shamash::direct",
        "shamash::bot",
        "shamash::voice",
    ] {
        assert!(
            d.contains(&format!("{target}=info")),
            "{target} is silenced at info; DEFAULT_LOG is {DEFAULT_LOG}"
        );
    }
}

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

/// What the bot prints for `message` warned from songbird's target.
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
    // Not `try_unwrap`: tracing may still hold a clone, which flaked empty.
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

#[test]
fn the_rtcp_decryption_warning_is_not_printed() {
    let printed = printed_for("RTCP decryption failed: Crypto(Error)");
    assert!(printed.is_empty(), "still reaching stdout: {printed}");
}

/// Same module, but these mean the bot stopped hearing.
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

#[test]
fn our_own_warnings_are_still_printed() {
    use std::sync::{Arc, Mutex};
    let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = Arc::clone(&buffer);
    tracing::subscriber::with_default(
        tracing_subscriber::registry().with(WithoutRtcpNoise::new(
            tracing_subscriber::fmt::layer().with_writer(move || MutexWriter(Arc::clone(&sink))),
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
