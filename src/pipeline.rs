use crate::parser::{CommandParser, PlayRequest};
use crate::transcriber::Transcriber;

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
