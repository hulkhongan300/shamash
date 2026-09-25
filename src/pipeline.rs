use crate::config::Config;
use crate::parser::{CommandParser, PlayRequest};
use crate::player::Player;
use crate::transcriber::Transcriber;
use serenity::model::id::GuildId;
use serenity::prelude::Context as SerenityContext;
use std::sync::Arc;

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
            Ok(request) => request,
            Err(miss) => {
                let wake_words = self
                    .parser
                    .wake_words()
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("[{guild_id}] not a play command: {miss} (wake words: {wake_words})");
                return;
            }
        };
        println!("[{guild_id}] play: {}", request.query);
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
}
