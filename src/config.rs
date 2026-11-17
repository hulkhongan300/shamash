use anyhow::Context as _;
use std::collections::HashSet;

/// Wake words used when `WAKE_WORDS` is unset; a command is "groovy play <request>".
pub const DEFAULT_WAKE_WORDS: &[&str] = &["groovy"];

/// Identifies the bot to the services it calls.
pub const HTTP_USER_AGENT: &str = concat!("shamash/", env!("CARGO_PKG_VERSION"));

/// Total budget for the whole catalog lookup (all queries), in milliseconds.
pub const DEFAULT_RESOLVE_BUDGET_MS: u64 = 2_500;

/// Offer unverified results as a choice when nothing matched; safe since nothing plays unpicked.
pub const DEFAULT_UNVERIFIED_CHOOSER: bool = true;

/// Reads a boolean from the environment; a malformed value is an error, not the default.
pub fn env_bool(name: &str, default: bool) -> anyhow::Result<bool> {
    match std::env::var(name) {
        Err(_) => Ok(default),
        Ok(raw) => parse_bool(name, &raw),
    }
}

/// Split out so tests avoid `std::env::set_var` (unsafe, and racy across test threads).
fn parse_bool(name: &str, raw: &str) -> anyhow::Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => anyhow::bail!("{name} must be true or false, got {other:?}"),
    }
}

/// How many YouTube results a request is scored against.
pub const DEFAULT_SEARCH_CANDIDATES: usize = 8;

/// Lowest score played without asking; measured (README's Dracula: right 124, wrong 101).
pub const DEFAULT_MATCH_THRESHOLD: f64 = 90.0;

/// Wake-word model when `WAKE_MODEL` is unset; small because it runs continuously.
pub const DEFAULT_WAKE_MODEL: &str = crate::model::SMALL_MODEL_ID;

/// The Ollama model used to clean up a transcript, when `LLM_MODEL` is unset.
pub const DEFAULT_LLM_MODEL: &str = "llama3.2";

/// Reads a float from the environment; malformed or out-of-`range` values are errors.
pub fn env_f32(
    name: &str,
    default: f32,
    range: std::ops::RangeInclusive<f32>,
) -> anyhow::Result<f32> {
    match std::env::var(name) {
        Err(_) => Ok(default),
        Ok(raw) => {
            let value: f32 = raw
                .trim()
                .parse()
                .with_context(|| format!("{name} must be a number, got {raw:?}"))?;
            anyhow::ensure!(
                range.contains(&value),
                "{name} must be between {} and {}, got {value}",
                range.start(),
                range.end()
            );
            Ok(value)
        }
    }
}

/// Reads a count from the environment; a malformed value is an error.
pub fn env_usize(name: &str, default: usize) -> anyhow::Result<usize> {
    match std::env::var(name) {
        Err(_) => Ok(default),
        Ok(raw) => raw
            .trim()
            .parse()
            .with_context(|| format!("{name} must be a whole number, got {raw:?}")),
    }
}

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
            "whisper" => anyhow::bail!(
                "ASR_ENGINE=whisper is gone: the bundled whisper-rs engine cannot be \
                 linked alongside transcribe.cpp, which replaced it. Use \
                 ASR_ENGINE=direct and set REQUEST_MODEL to a Whisper GGUF."
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
    /// `REQUEST_MODEL`, legacy `PARAKEET_MODEL`, or [`crate::model::DEFAULT_MODEL_ID`].
    pub request_model: String,
    /// `ASR_PROMPT` for models that accept one; `off` disables it.
    pub asr_prompt: Option<String>,
    pub asr_language: Option<String>,
    pub wake_words: HashSet<String>,
    pub alert_channel_id: Option<u64>,
    pub resolve_budget: std::time::Duration,
    pub search_candidates: usize,
    pub match_threshold: f64,
    pub unverified_chooser: bool,
    pub wake_model: String,
    /// Ollama-compatible transcript cleanup; off by default, the catalog fixes most mishearing.
    pub llm_endpoint: Option<String>,
    pub llm_model: String,
    /// Song memory file (`LIBRARY_PATH`); `off` keeps nothing.
    pub library_path: Option<std::path::PathBuf>,
}

/// Song memory path unless `LIBRARY_PATH` is set; the image uses `/data/library.json`.
pub const DEFAULT_LIBRARY_PATH: &str = "data/library.json";

/// Reads `LIBRARY_PATH`; `off`/`none`/`0`/`false` keeps no memory.
fn library_path(value: Option<String>) -> Option<std::path::PathBuf> {
    match value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        Some(off) if ["off", "none", "0", "false"].contains(&off.to_ascii_lowercase().as_str()) => {
            None
        }
        Some(path) => Some(path.into()),
        None => Some(DEFAULT_LIBRARY_PATH.into()),
    }
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let discord_token = required("DISCORD_TOKEN")?;
        let voice_channel_id = required("VOICE_CHANNEL_ID")?
            .trim()
            .parse()
            .context("VOICE_CHANNEL_ID must be a channel id (a number)")?;
        let asr_engine = match std::env::var("ASR_ENGINE") {
            Ok(value) => AsrEngine::parse(&value)?,
            Err(_) => AsrEngine::Direct,
        };
        let request_model = non_empty("REQUEST_MODEL")
            .or_else(|| non_empty("PARAKEET_MODEL"))
            .unwrap_or_else(|| crate::model::DEFAULT_MODEL_ID.to_string());
        let asr_prompt = match non_empty("ASR_PROMPT") {
            Some(off)
                if ["off", "none", "0", "false"].contains(&off.to_ascii_lowercase().as_str()) =>
            {
                None
            }
            Some(prompt) => Some(prompt),
            None => Some(crate::model::REQUEST_PROMPT.to_string()),
        };
        // Canary and Nemotron return empty text without it.
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
            resolve_budget: std::time::Duration::from_millis(env_usize(
                "RESOLVE_BUDGET_MS",
                DEFAULT_RESOLVE_BUDGET_MS as usize,
            )? as u64),
            discord_token,
            voice_channel_id,
            asr_engine,
            request_model,
            asr_prompt,
            asr_language,
            wake_words,
            alert_channel_id,
            search_candidates: env_usize("SEARCH_CANDIDATES", DEFAULT_SEARCH_CANDIDATES)?
                .clamp(1, 50),
            match_threshold: env_f32(
                "MATCH_THRESHOLD",
                DEFAULT_MATCH_THRESHOLD as f32,
                0.0..=1000.0,
            )? as f64,
            unverified_chooser: env_bool("UNVERIFIED_CHOOSER", DEFAULT_UNVERIFIED_CHOOSER)?,
            wake_model: std::env::var("WAKE_MODEL")
                .ok()
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| DEFAULT_WAKE_MODEL.to_string()),
            llm_endpoint: std::env::var("LLM_ENDPOINT")
                .ok()
                .map(|e| e.trim().to_string())
                .filter(|e| !e.is_empty()),
            llm_model: std::env::var("LLM_MODEL")
                .ok()
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| DEFAULT_LLM_MODEL.to_string()),
            library_path: library_path(std::env::var("LIBRARY_PATH").ok()),
        })
    }
}

/// How several artists recording one title are settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmbiguousPolicy {
    /// Play the best-scoring candidate without asking.
    Auto,
    /// Offer the choices.
    Ask,
    /// Play whichever is far more popular.
    Popular,
}

impl AmbiguousPolicy {
    fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some(value) if value.eq_ignore_ascii_case("auto") => Self::Auto,
            Some(value) if value.eq_ignore_ascii_case("popular") => Self::Popular,
            // A typo asks: an extra click beats a wrong song.
            _ => Self::Ask,
        }
    }
}

/// Reads `AMBIGUOUS_POLICY`, treating unset as `ask`.
pub fn ambiguous_policy() -> AmbiguousPolicy {
    AmbiguousPolicy::parse(std::env::var("AMBIGUOUS_POLICY").ok().as_deref())
}

/// Parses an optional channel id; blank (an `.env` placeholder) means unset.
fn parse_optional_id(value: Option<String>) -> anyhow::Result<Option<u64>> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(Into::into)
}

fn required(name: &str) -> anyhow::Result<String> {
    non_empty(name).with_context(|| format!("{name} is not set; add it to .env"))
}

fn non_empty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|w| w.trim().to_ascii_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

#[cfg(test)]
#[path = "tests/config.rs"]
mod tests;
