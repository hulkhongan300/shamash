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
    /// The Handy app, which owns a Parakeet model and the GPU backend.
    Handy,
    /// The bundled whisper.cpp model.
    Whisper,
}

impl AsrEngine {
    fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "handy" | "parakeet" => Ok(Self::Handy),
            "whisper" => Ok(Self::Whisper),
            other => anyhow::bail!("unknown ASR_ENGINE '{other}'; use 'handy' or 'whisper'"),
        }
    }
}

/// Runtime configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    pub discord_token: String,
    pub voice_channel_id: u64,
    pub asr_engine: AsrEngine,
    pub whisper_model: String,
    pub parakeet_model: String,
    pub wake_words: HashSet<String>,
    pub alert_channel_id: Option<u64>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let discord_token = std::env::var("DISCORD_TOKEN")?;
        let voice_channel_id = std::env::var("VOICE_CHANNEL_ID")?.parse()?;
        let asr_engine = match std::env::var("ASR_ENGINE") {
            Ok(value) => AsrEngine::parse(&value)?,
            Err(_) => AsrEngine::Handy,
        };
        let whisper_model =
            std::env::var("WHISPER_MODEL").unwrap_or_else(|_| "data/model.bin".to_string());
        let parakeet_model = std::env::var("PARAKEET_MODEL")
            .unwrap_or_else(|_| crate::parakeet::DEFAULT_MODEL.to_string());
        let alert_channel_id = std::env::var("ALERT_CHANNEL_ID")
            .ok()
            .map(|value| value.parse())
            .transpose()?;
        let wake_words = std::env::var("WAKE_WORDS")
            .map(|s| split_csv(&s))
            .unwrap_or_else(|_| DEFAULT_WAKE_WORDS.iter().map(|w| w.to_string()).collect())
            .into_iter()
            .collect();
        Ok(Self {
            discord_token,
            voice_channel_id,
            asr_engine,
            whisper_model,
            parakeet_model,
            wake_words,
            alert_channel_id,
        })
    }
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
    fn asr_engine_names_are_case_insensitive() {
        assert_eq!(AsrEngine::parse("Handy").unwrap(), AsrEngine::Handy);
        assert_eq!(AsrEngine::parse(" parakeet ").unwrap(), AsrEngine::Handy);
        assert_eq!(AsrEngine::parse("WHISPER").unwrap(), AsrEngine::Whisper);
    }

    #[test]
    fn an_unknown_asr_engine_is_rejected_by_name() {
        let err = AsrEngine::parse("deepspeech").unwrap_err().to_string();
        assert!(err.contains("deepspeech"), "unhelpful error: {err}");
    }
}
