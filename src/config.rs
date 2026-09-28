use std::collections::HashSet;

/// Wake words used when `WAKE_WORDS` is unset.
///
/// "play" is included because the parser lets it stand in for the wake word,
/// which makes the bare "play <query>" the shortest thing worth speaking.
///
/// "ut" and "ot" are there because speech-to-text often drops the leading
/// consonant of a short wake word, turning "bot" into "ut" or "ot". Matching
/// only "bot" left the bot deaf to the word people actually shout at it.
pub const DEFAULT_WAKE_WORDS: &[&str] = &["bot", "play", "ut", "ot"];

/// Which speech-to-text engine transcribes speech.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrEngine {
    /// transcribe.cpp linked directly into this process, model held resident.
    Direct,
    /// The Handy app, kept as a fallback; loads a model per utterance.
    Handy,
}

impl AsrEngine {
    fn parse(value: &str) -> anyhow::Result<Self> {
        let value = value.trim().to_ascii_lowercase();
        match value.as_str() {
            "direct" | "transcribe-cpp" | "tcpp" => Ok(Self::Direct),
            "handy" | "parakeet" => Ok(Self::Handy),
            // Whisper used to be a separate engine here. It now runs through
            // transcribe.cpp like every other model.
            "whisper" => anyhow::bail!(
                "ASR_ENGINE=whisper is gone: the bundled whisper-rs engine cannot be \
                 linked alongside transcribe.cpp, which replaced it. Use \
                 ASR_ENGINE=direct and set PARAKEET_MODEL to a Whisper GGUF."
            ),
            other => anyhow::bail!("unknown ASR_ENGINE '{other}'; use 'direct' or 'handy'"),
        }
    }
}

/// Runtime configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    pub discord_token: String,
    pub voice_channel_id: u64,
    pub asr_engine: AsrEngine,
    pub parakeet_model: String,
    pub asr_language: Option<String>,
    pub wake_words: HashSet<String>,
    pub alert_channel_id: Option<u64>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let discord_token = std::env::var("DISCORD_TOKEN")?;
        let voice_channel_id = std::env::var("VOICE_CHANNEL_ID")?.parse()?;
        let asr_engine = match std::env::var("ASR_ENGINE") {
            Ok(value) => AsrEngine::parse(&value)?,
            Err(_) => AsrEngine::Direct,
        };
        let parakeet_model = std::env::var("PARAKEET_MODEL")
            .unwrap_or_else(|_| crate::model::DEFAULT_MODEL_ID.into());
        // Only the models that cannot detect a language for themselves need
        // this; Canary and Nemotron return empty text without it.
        let asr_language = std::env::var("ASR_LANGUAGE")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let alert_channel_id = parse_optional_id(std::env::var("ALERT_CHANNEL_ID").ok())?;
        let wake_words = std::env::var("WAKE_WORDS")
            .map(|s| split_csv(&s))
            .unwrap_or_else(|_| DEFAULT_WAKE_WORDS.iter().map(|w| w.to_string()).collect())
            .into_iter()
            .collect();
        Ok(Self {
            discord_token,
            voice_channel_id,
            asr_engine,
            parakeet_model,
            asr_language,
            wake_words,
            alert_channel_id,
        })
    }
}

/// Parses an optional channel id, treating blank as unset.
///
/// Blank is treated as unset so a placeholder line left in `.env` falls back
/// to the default channel rather than refusing to start on a parse error.
fn parse_optional_id(value: Option<String>) -> anyhow::Result<Option<u64>> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(Into::into)
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|w| w.trim().to_ascii_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_csv_wake_words() {
        let words = split_csv("Shamash, BOT,  ,tveir");
        assert_eq!(words, vec!["shamash", "bot", "tveir"]);
    }

    #[test]
    fn empty_csv_yields_no_wake_words() {
        assert!(split_csv(" , , ").is_empty());
    }

    #[test]
    fn a_blank_optional_channel_id_means_unset() {
        assert_eq!(parse_optional_id(Some(String::new())).unwrap(), None);
        assert_eq!(parse_optional_id(Some("   ".to_string())).unwrap(), None);
        assert_eq!(parse_optional_id(None).unwrap(), None);
    }

    #[test]
    fn an_optional_channel_id_is_read_when_present() {
        assert_eq!(
            parse_optional_id(Some(" 12345 ".to_string())).unwrap(),
            Some(12345)
        );
    }

    #[test]
    fn a_non_numeric_optional_channel_id_is_rejected() {
        assert!(parse_optional_id(Some("not-a-channel".to_string())).is_err());
    }

    #[test]
    fn asr_engine_names_are_case_insensitive() {
        assert_eq!(AsrEngine::parse("Handy").unwrap(), AsrEngine::Handy);
        assert_eq!(AsrEngine::parse(" parakeet ").unwrap(), AsrEngine::Handy);
        assert_eq!(AsrEngine::parse("DIRECT").unwrap(), AsrEngine::Direct);
        assert_eq!(
            AsrEngine::parse(" transcribe-cpp ").unwrap(),
            AsrEngine::Direct
        );
    }

    /// The removed engine's name should point at the replacement rather than
    /// be reported as unknown.
    #[test]
    fn the_removed_whisper_engine_says_where_to_go() {
        let err = AsrEngine::parse("whisper").unwrap_err().to_string();
        assert!(err.contains("ASR_ENGINE=direct"), "unhelpful error: {err}");
    }

    #[test]
    fn an_unknown_asr_engine_is_rejected_by_name() {
        let err = AsrEngine::parse("deepspeech").unwrap_err().to_string();
        assert!(err.contains("deepspeech"), "unhelpful error: {err}");
    }
}
