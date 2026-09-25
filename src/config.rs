use std::collections::HashSet;

/// Runtime configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    pub discord_token: String,
    pub voice_channel_id: u64,
    pub whisper_model: String,
    pub wake_words: HashSet<String>,
    pub alert_channel_id: Option<u64>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let discord_token = std::env::var("DISCORD_TOKEN")?;
        let voice_channel_id = std::env::var("VOICE_CHANNEL_ID")?.parse()?;
        let whisper_model =
            std::env::var("WHISPER_MODEL").unwrap_or_else(|_| "data/model.bin".to_string());
        let alert_channel_id = std::env::var("ALERT_CHANNEL_ID")
            .ok()
            .map(|value| value.parse())
            .transpose()?;
        let wake_words = std::env::var("WAKE_WORDS")
            .map(|s| split_csv(&s))
            .unwrap_or_else(|_| vec!["shamash".to_string(), "bot".to_string()])
            .into_iter()
            .collect();
        Ok(Self {
            discord_token,
            voice_channel_id,
            whisper_model,
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
    fn empty_csv_yields_no_words() {
        assert!(split_csv(" , , ").is_empty());
    }
}
