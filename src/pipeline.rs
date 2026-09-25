use crate::config::Config;
use crate::parser::{CommandParser, ParseMiss, PlayRequest};
use crate::player::Player;
use crate::transcriber::Transcriber;
use serenity::model::id::GuildId;
use serenity::prelude::Context as SerenityContext;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a "bot play" with no title keeps waiting for one.
///
/// Speaking the command and the song title with a pause between them produces
/// two utterances. The first carries the wake word and "play", the second is
/// just the title, and the second is only recognisable as a command because of
/// the first.
const PENDING_QUERY_TTL: Duration = Duration::from_secs(8);

/// Remembers a "bot play" that is still waiting for a title.
#[derive(Debug, Default)]
struct PendingQuery {
    since: std::sync::Mutex<Option<Instant>>,
}

impl PendingQuery {
    /// Notes that a wake word and "play" were heard with no title after them.
    fn expect(&self) {
        if let Ok(mut slot) = self.since.lock() {
            *slot = Some(Instant::now());
        }
    }

    /// Whether a title is still being awaited, clearing the slot either way so
    /// one stray utterance cannot claim a command spoken much later.
    fn take(&self) -> bool {
        let Ok(mut slot) = self.since.lock() else {
            return false;
        };
        let pending = slot.is_some_and(|at| at.elapsed() < PENDING_QUERY_TTL);
        *slot = None;
        pending
    }

    fn clear(&self) {
        if let Ok(mut slot) = self.since.lock() {
            *slot = None;
        }
    }
}

/// Transcribes an utterance and turns it into a play request, if the grammar
/// matched. `None` means the transcript contained no play command.
pub fn transcribe_and_parse(
    transcriber: &dyn Transcriber,
    parser: &CommandParser,
    samples: &[f32],
) -> anyhow::Result<Option<PlayRequest>> {
    let transcript = transcriber.transcribe(samples)?;
    Ok(parser.parse(&transcript))
}

/// The full speech-to-music pipeline: transcribe, parse, and play.
pub struct ListenerPipeline {
    pub transcriber: Arc<dyn Transcriber>,
    pub parser: CommandParser,
    pub player: Arc<Player>,
    /// Utterances that produced no words, reported once there is something
    /// real to say. Background noise trips the voice detector often enough
    /// that printing every one of them drowns out real commands.
    muted_utterances: std::sync::atomic::AtomicUsize,
    /// Set when a wake word and "play" were heard with no title after them.
    awaiting_query: PendingQuery,
}

impl ListenerPipeline {
    pub fn new(
        transcriber: Arc<dyn Transcriber>,
        parser: CommandParser,
        player: Arc<Player>,
    ) -> Self {
        Self {
            transcriber,
            parser,
            player,
            muted_utterances: std::sync::atomic::AtomicUsize::new(0),
            awaiting_query: PendingQuery::default(),
        }
    }

    /// Transcribes `utterance` off the blocking pool, then plays any request
    /// that matches the grammar. Failures are logged, never fatal.
    pub async fn handle_utterance(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        utterance: Vec<f32>,
    ) {
        let transcriber = self.transcriber.clone();
        let transcript =
            match tokio::task::spawn_blocking(move || transcriber.transcribe(&utterance)).await {
                Ok(Ok(transcript)) => transcript,
                Ok(Err(e)) => {
                    println!("transcription failed: {e:#}");
                    return;
                }
                Err(e) => {
                    println!("transcription task failed: {e}");
                    return;
                }
            };

        if transcript.trim().is_empty() {
            self.muted_utterances
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            println!(
                "[{guild_id}] voice detected but no words recognised (too quiet, or too short?)"
            );
            return;
        }
        let skipped = self
            .muted_utterances
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        if skipped > 0 {
            println!("[{guild_id}] heard: {transcript:?} (skipped {skipped} unintelligible)");
        } else {
            println!("[{guild_id}] heard: {transcript:?}");
        }

        let request = match self.parser.parse_with_reason(&transcript) {
            Ok(request) => {
                self.awaiting_query.clear();
                request
            }
            Err(ParseMiss::EmptyRequest) => {
                self.awaiting_query.expect();
                println!("[{guild_id}] heard the wake word and \"play\", waiting for a title");
                return;
            }
            Err(miss) => {
                // A pause inside the command splits it in two, so the title can
                // arrive without any wake word of its own.
                let completed = self
                    .awaiting_query
                    .take()
                    .then(|| self.parser.request_from_query(&transcript))
                    .flatten();
                match completed {
                    Some(request) => {
                        println!(
                            "[{guild_id}] taking {transcript:?} as the title for that command"
                        );
                        request
                    }
                    None => {
                        let wake_words = self
                            .parser
                            .wake_words()
                            .iter()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ");
                        println!(
                            "[{guild_id}] not a play command: {miss} (wake words: {wake_words})"
                        );
                        return;
                    }
                }
            }
        };

        println!(
            "[{guild_id}] wake command activated: {transcript:?} -> play {}",
            request.query
        );
        if let Err(e) = self.player.acknowledge(guild_id).await {
            println!("[{guild_id}] acknowledgement tone failed: {e:#}");
        }
        if let Err(e) = self.player.play(ctx, config, guild_id, &request).await {
            println!("[{guild_id}] playback failed: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestTranscriber;

    impl Transcriber for TestTranscriber {
        fn sample_rate(&self) -> u32 {
            16_000
        }

        fn transcribe(&self, _samples: &[f32]) -> anyhow::Result<String> {
            Ok("shamash play dracula by tame impala".to_string())
        }
    }

    struct QuietTranscriber;

    impl Transcriber for QuietTranscriber {
        fn sample_rate(&self) -> u32 {
            16_000
        }

        fn transcribe(&self, _samples: &[f32]) -> anyhow::Result<String> {
            Ok("just chatting about lunch".to_string())
        }
    }

    #[test]
    fn wired_pipeline_yields_play_request() {
        let req = transcribe_and_parse(
            &TestTranscriber,
            &crate::parser::CommandParser::new(["shamash".to_string()]),
            &[0.0; 320],
        )
        .unwrap()
        .expect("should match");
        assert_eq!(req.query, "dracula by tame impala");
    }

    #[test]
    fn wired_pipeline_ignores_ordinary_speech() {
        let result = transcribe_and_parse(
            &QuietTranscriber,
            &crate::parser::CommandParser::new(["shamash".to_string()]),
            &[0.0; 320],
        )
        .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn a_title_can_only_be_claimed_once() {
        let pending = PendingQuery::default();
        assert!(
            !pending.take(),
            "nothing is pending before a command is heard"
        );

        pending.expect();
        assert!(
            pending.take(),
            "the next utterance should complete the command"
        );
        assert!(!pending.take(), "a later utterance must not claim it again");
    }

    #[test]
    fn a_pending_title_expires() {
        let pending = PendingQuery::default();
        pending.expect();
        *pending.since.lock().unwrap() =
            Some(Instant::now() - PENDING_QUERY_TTL - Duration::from_millis(1));

        assert!(
            !pending.take(),
            "a title spoken long after the command should not be played"
        );
    }

    #[test]
    fn a_complete_command_clears_the_pending_title() {
        let pending = PendingQuery::default();
        pending.expect();
        pending.clear();
        assert!(!pending.take());
    }
}
