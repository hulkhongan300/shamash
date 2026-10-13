use anyhow::Context as _;
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

/// Identifies the bot to Deezer, which rejects requests without one.
pub const HTTP_USER_AGENT: &str = concat!("shamash/", env!("CARGO_PKG_VERSION"));

/// The catalogs to resolve requests against when `CATALOG_PROVIDERS` is unset.
///
/// Two by default, and they are not redundant: Deezer answers artist-only
/// queries that rescue a misheard title, and iTunes catches a Deezer quota. The
/// MusicBrainz fuzzy search is deliberately *not* in the default list -- it is
/// rate-limited to one request a second and is only worth the wait when the
/// faster providers found nothing.
pub const DEFAULT_CATALOG_PROVIDERS: &str = "deezer,itunes";

/// Catalogs this build can resolve against.
///
/// All need no API key, which is why they are here at all; the provider trait
/// in [`crate::resolve`] exists so a keyed or local catalog can be added without
/// touching the selection code. [`crate::resolve::Catalog::provider`] is the
/// only place these names are matched.
pub const SUPPORTED_CATALOG_PROVIDERS: &[&str] = &["deezer", "itunes", "musicbrainz"];

/// How long the whole catalog fan-out may take, in milliseconds.
///
/// One budget for every query, not one per query: this sits in front of the
/// YouTube search, and the point of asking several providers at once is that
/// the slowest of them does not set the wait. Two and a half seconds is
/// generous for a handful of keyless HTTP GETs and still leaves the search and
/// the download to follow.
///
/// ponytail: one global budget. Per-provider budgets would only matter once one
/// provider is meaningfully slower than the rest.
pub const DEFAULT_RESOLVE_BUDGET_MS: u64 = 2_500;

/// How confident a catalog match must be before its names override the
/// transcript.
///
/// Now on the matcher's 0-1 scale, where the title must clear 0.65 on its own
/// and the combination must clear 0.72. The old default of 0.62 was on the
/// pre-catalog scorer's scale, where a shared word was worth 0.72 on its own, so
/// it cannot be compared with this one.
pub const DEFAULT_CATALOG_MATCH_THRESHOLD: f32 = crate::matcher::ACCEPT;

/// Whether a video found *without* a catalog match may be played on its own.
///
/// Default `false`, and it should stay that way. Without a catalog there is no
/// verified running time, which removes the only signal that tells the studio
/// recording from a remix of it; the scorer then has nothing but a title
/// similarity and a view count, and `UNRESOLVED_MATCH_THRESHOLD` was low enough
/// that an unrelated title sharing one word cleared it. Offering the results as
/// a choice costs the listener one button press and never plays a wrong song
/// unannounced; this setting only exists for someone who would rather have the
/// wrong song than the extra click.
pub const DEFAULT_ALLOW_UNVERIFIED_AUTOPLAY: bool = false;

/// Whether unverified results are offered as a choice when nothing matched.
///
/// Default `true`, because it is a question rather than an answer: the options
/// are labelled unverified and nothing is played until the listener presses one.
/// With it off, a request that matched nothing produces only the miss tone and a
/// message.
pub const DEFAULT_UNVERIFIED_CHOOSER: bool = true;

/// Reads a boolean from the environment, falling back to `default`.
///
/// A malformed value is an error rather than a silent default, for the same
/// reason [`env_f32`] refuses one: a typo in `ALLOW_UNVERIFIED_AUTOPLAY` that
/// quietly became `false` would be invisible, and one that quietly became
/// `true` would turn the bot back into playing whatever YouTube ranked first.
pub fn env_bool(name: &str, default: bool) -> anyhow::Result<bool> {
    match std::env::var(name) {
        Err(_) => Ok(default),
        Ok(raw) => parse_bool(name, &raw),
    }
}

/// One setting's value, without the environment, so the accepted spellings can
/// be tested without `std::env::set_var` (unsafe in edition 2024, and the test
/// threads run in parallel so a shared variable would be a race).
fn parse_bool(name: &str, raw: &str) -> anyhow::Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => anyhow::bail!("{name} must be true or false, got {other:?}"),
    }
}

/// How many YouTube results a request is scored against.
///
/// The scorer's inputs are a title, an artist and a running time, so a short
/// list is usually enough; the extra candidates only widen the pool the three
/// offered choices are drawn from.
pub const DEFAULT_SEARCH_CANDIDATES: usize = 8;

/// The lowest score a candidate may reach and still be played without asking.
///
/// 90 is measured, not guessed: with a catalog match a candidate can earn about
/// 190 points, and the Dracula case in the README plays at 124 while the lyric
/// video it must not play sits at 101. A bar under that would pick the wrong
/// song; a bar far above it would post choices for requests that were already
/// confident.
pub const DEFAULT_MATCH_THRESHOLD: f64 = 90.0;

/// The model that listens for the wake word, when `WAKE_MODEL` is unset.
///
/// The small Parakeet, deliberately: it runs on every window of speech in the
/// channel, so its cost is paid continuously rather than once per command, and
/// it only has to hear one short word. `PARAKEET_MODEL` is the opposite
/// trade -- loaded once and run only after a wake word, so it can afford to be
/// the stronger, much larger model.
pub const DEFAULT_WAKE_MODEL: &str = crate::model::SMALL_MODEL_ID;

/// The Ollama model used to clean up a transcript, when `LLM_MODEL` is unset.
pub const DEFAULT_LLM_MODEL: &str = "llama3.2";

/// Reads a float from the environment, falling back to `default`.
///
/// A malformed value is an error rather than a silent default: a typo in
/// `MATCH_THRESHOLD` that quietly became the default would be very hard to
/// notice and very confusing to tune. `range` is checked for the same reason,
/// and is passed in rather than inferred from the name so each setting's
/// legitimate range is stated where the setting is defined.
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

/// Reads a count from the environment, falling back to `default`.
///
/// Same reasoning as [`env_f32`]: a typo in a count is a startup error, not a
/// silent default.
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
    /// Which music catalogs resolve spoken requests, in the order they are asked.
    pub catalog_providers: Vec<String>,
    /// Contact details for MusicBrainz, or `None` to leave that provider off.
    ///
    /// MusicBrainz's terms require a `User-Agent` that identifies the
    /// application and gives someone a way to be contacted, and it asks for
    /// exactly one request a second. A bot that cannot identify itself is not
    /// welcome there, so this being unset disables the provider rather than
    /// sending an anonymous request.
    pub musicbrainz_contact: Option<String>,
    /// How long the whole catalog fan-out may take.
    pub resolve_budget: std::time::Duration,
    /// How confident a catalog match must be to override the transcript.
    pub catalog_match_threshold: f32,
    /// How many YouTube results a request is scored against.
    pub search_candidates: usize,
    /// The lowest score a candidate may reach and still be played unasked.
    pub match_threshold: f64,
    /// Whether a video found without a catalog match may be played unasked.
    pub allow_unverified_autoplay: bool,
    /// Whether unverified results are offered as a choice when nothing matched.
    pub unverified_chooser: bool,
    /// The small model that listens for the wake word at all times.
    pub wake_model: String,
    /// An Ollama-compatible endpoint that cleans up a transcript, or `None`.
    ///
    /// `None` leaves the raw transcript to the parser, which is the pre-LLM
    /// behaviour and stays the default: the catalog already corrects most
    /// mishearing, so this has to earn its place before it is switched on.
    pub llm_endpoint: Option<String>,
    pub llm_model: String,
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
        let catalog_providers = catalog_provider_names(
            std::env::var("CATALOG_PROVIDERS").ok().as_deref(),
            std::env::var("CATALOG_PROVIDER").ok().as_deref(),
        )?;
        Ok(Self {
            catalog_providers,
            musicbrainz_contact: std::env::var("MUSICBRAINZ_CONTACT")
                .ok()
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty()),
            resolve_budget: std::time::Duration::from_millis(env_usize(
                "RESOLVE_BUDGET_MS",
                DEFAULT_RESOLVE_BUDGET_MS as usize,
            )? as u64),
            catalog_match_threshold: env_f32(
                "CATALOG_MATCH_THRESHOLD",
                DEFAULT_CATALOG_MATCH_THRESHOLD,
                0.5..=0.95,
            )?,
            discord_token,
            voice_channel_id,
            asr_engine,
            parakeet_model,
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
            allow_unverified_autoplay: env_bool(
                "ALLOW_UNVERIFIED_AUTOPLAY",
                DEFAULT_ALLOW_UNVERIFIED_AUTOPLAY,
            )?,
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
        })
    }
}

/// Validates `CATALOG_PROVIDERS`, treating unset as the default.
///
/// `legacy` is `CATALOG_PROVIDER`, the single-provider setting this replaced.
/// It is still accepted, as one item in the list, so an existing `.env` keeps
/// working -- the caller logs the deprecation.
///
/// Split out from [`Config::from_env`] so the accepted names are testable
/// without also having to supply a Discord token and a voice channel id.
pub fn catalog_provider_names(
    value: Option<&str>,
    legacy: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let raw = match (value, legacy) {
        (Some(list), _) if !list.trim().is_empty() => list.to_string(),
        (_, Some(single)) => single.to_string(),
        _ => DEFAULT_CATALOG_PROVIDERS.to_string(),
    };

    let mut names: Vec<String> = Vec::new();
    for entry in split_csv(&raw) {
        anyhow::ensure!(
            SUPPORTED_CATALOG_PROVIDERS.contains(&entry.as_str()),
            "unknown catalog provider '{entry}' in CATALOG_PROVIDERS; supported: {}",
            SUPPORTED_CATALOG_PROVIDERS.join(", ")
        );
        // A repeat costs a duplicate fan-out for nothing.
        if !names.contains(&entry) {
            names.push(entry);
        }
    }
    anyhow::ensure!(
        !names.is_empty(),
        "CATALOG_PROVIDERS must name at least one provider"
    );
    Ok(names)
}

/// How several artists recording one title are settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmbiguousPolicy {
    /// Offer the choices.
    Ask,
    /// Play whichever is far more popular.
    Popular,
}

/// Reads `AMBIGUOUS_POLICY`, treating unset as `ask`.
pub fn ambiguous_policy() -> AmbiguousPolicy {
    match std::env::var("AMBIGUOUS_POLICY")
        .ok()
        .as_deref()
        .map(str::trim)
    {
        Some(value) if value.eq_ignore_ascii_case("popular") => AmbiguousPolicy::Popular,
        // A typo falls back to asking, which is the safe direction: the wrong
        // reading of this setting is a wrong song rather than an extra click.
        _ => AmbiguousPolicy::Ask,
    }
}

/// Whether `CATALOG_PROVIDER` is set, which is how startup decides to warn.
pub fn uses_legacy_catalog_provider(providers: Option<&str>, legacy: Option<&str>) -> bool {
    match providers {
        Some(list) if !list.trim().is_empty() => false,
        _ => legacy.is_some_and(|single| !single.trim().is_empty()),
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

    /// An unknown catalog must be named in the error, or a typo looks like the
    /// bot ignoring the setting.
    #[test]
    fn an_unknown_catalog_provider_is_rejected_by_name() {
        let err = catalog_provider_names(Some("spotify"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("spotify"), "unhelpful error: {err}");
        assert!(
            err.contains("deezer"),
            "should list what is supported: {err}"
        );
    }

    #[test]
    fn the_catalog_providers_default_and_are_case_insensitive() {
        assert_eq!(
            catalog_provider_names(None, None).unwrap(),
            ["deezer", "itunes"]
        );
        assert_eq!(
            catalog_provider_names(Some("  "), None).unwrap(),
            ["deezer", "itunes"]
        );
        assert_eq!(
            catalog_provider_names(Some(" Itunes , Deezer "), None).unwrap(),
            ["itunes", "deezer"],
            "the order asked for is the order asked in"
        );
    }

    /// `CATALOG_PROVIDER` is still accepted as a one-item list, so an existing
    /// `.env` keeps working, and the caller is told to move on.
    #[test]
    fn the_single_provider_setting_still_works_and_is_flagged() {
        assert_eq!(
            catalog_provider_names(None, Some(" Deezer ")).unwrap(),
            ["deezer"]
        );
        assert!(
            uses_legacy_catalog_provider(None, Some("deezer")),
            "startup should warn about the old name"
        );
        assert!(
            !uses_legacy_catalog_provider(Some("deezer"), Some("deezer")),
            "but not when the new one is set too"
        );
        assert!(!uses_legacy_catalog_provider(Some("deezer"), None));
    }

    /// A repeat costs a duplicate fan-out for nothing.
    #[test]
    fn a_repeated_provider_is_asked_once() {
        assert_eq!(
            catalog_provider_names(Some("deezer,deezer"), None).unwrap(),
            ["deezer"]
        );
    }

    /// An empty list would leave the bot with no catalog at all, and that is a
    /// mistake worth refusing at startup rather than at the first request.
    #[test]
    fn an_empty_provider_list_is_refused() {
        assert!(
            catalog_provider_names(Some(" , , "), None)
                .expect_err("nothing usable")
                .to_string()
                .contains("at least one")
        );
    }

    /// Unverified autoplay is off unless the operator says otherwise, because the
    /// whole point is not to play a song no catalog confirmed. Pinned at compile
    /// time so it cannot be flipped by accident.
    const _: () = assert!(!DEFAULT_ALLOW_UNVERIFIED_AUTOPLAY);

    /// A typo must not become a silent `false` here: this flag decides whether
    /// the bot plays something it could not verify.
    #[test]
    fn a_malformed_boolean_is_an_error_rather_than_a_default() {
        for (raw, expected) in [
            ("1", true),
            ("true", true),
            (" YES ", true),
            ("on", true),
            ("0", false),
            ("false", false),
            ("No", false),
            ("off", false),
        ] {
            assert_eq!(
                parse_bool("ALLOW_UNVERIFIED_AUTOPLAY", raw).unwrap(),
                expected
            );
        }
        let err = parse_bool("ALLOW_UNVERIFIED_AUTOPLAY", "ture")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("ALLOW_UNVERIFIED_AUTOPLAY"),
            "unhelpful: {err}"
        );
        assert!(err.contains("true or false"), "unhelpful: {err}");
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
