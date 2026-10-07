//! Resolves a spoken request against a music catalog, to get a canonical
//! spelling of the song before any video search happens.
//!
//! The problem this solves: speech-to-text mishears proper nouns, so "hoolig"
//! reaches the search layer and gets a flat YouTube query. Fixing that with more
//! string matching on YouTube titles is guessing, because those titles are
//! written by uploaders and are not a list of songs.
//!
//! A music catalog *is* a list of songs. Two keyless public APIs are wired up:
//! Deezer's search, and the iTunes Search API. Both return the artist's own
//! spelling of the title plus the track's real running time in seconds. That
//! running time is the valuable part: it is the one number nobody can lie about,
//! and it is what lets [`crate::search::score`] tell the studio recording from a
//! 10-minute remix or a 30-second clip without guessing from the title.
//!
//! Providers sit behind [`CatalogProvider`] so a keyed or local catalog can be
//! added without touching the caller.

use crate::parser::PlayRequest;
use anyhow::Context as _;
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashSet;
use std::time::Duration;
use strsim::jaro_winkler;
use tokio::time::timeout;

/// How long a catalog lookup may take before the request falls back to the raw
/// transcript.
///
/// A spoken command is a two-second interaction, and the catalog lookup sits in
/// front of the YouTube search, so it has to be a rounding error next to the
/// download it is trying to avoid. Two seconds is generous for a single HTTP GET
/// to a public API and still leaves room for the search itself.
///
/// ponytail: one global timeout for every provider. Per-provider budgets would
/// only matter once one of them is meaningfully slower than the others.
const LOOKUP_TIMEOUT: Duration = Duration::from_millis(2_000);

/// A track as the catalog knows it.
///
/// This is the corrected spelling, not a guess: whatever the catalog returned
/// for the match is what the user meant, including punctuation and capitalisation
/// that speech-to-text would never produce.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogTrack {
    pub artist: String,
    pub title: String,
    /// The track's running time, when the provider reports one.
    ///
    /// This is the most reliable signal available: a studio single is within a
    /// second or two across every source, whereas titles lie about what a video
    /// contains.
    pub duration_secs: Option<f64>,
    /// Provider-specific id, for logging and for de-duplication.
    pub id: String,
    pub provider: &'static str,
}

impl CatalogTrack {
    /// A short label for logs and for the "did you mean" embed.
    pub fn label(&self) -> String {
        format!("{} — {}", self.artist, self.title)
    }
}

/// A catalog track together with how sure we are it is what was asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    pub track: CatalogTrack,
    /// 0.0 (a guess) to 1.0 (an exact title-and-artist hit).
    pub confidence: f32,
    /// The query that produced this match, kept for logging a bad result.
    pub query: String,
    /// Whether several different artists answered to the same title.
    ///
    /// When the request named no artist and the catalog offers the same title
    /// by four different artists, no amount of title matching separates them:
    /// they all score identically. Committing to one and building the YouTube
    /// query from that name actively steers the search to a song the listener
    /// never asked for, so the ambiguity is reported and the caller asks.
    pub ambiguous: bool,
}

/// A source of canonical song metadata.
///
/// Object safe, so the provider is chosen at runtime from `CATALOG_PROVIDER`
/// and swapped without a recompile.
#[async_trait]
pub trait CatalogProvider: Send + Sync {
    /// Short name used in logs and in the `CATALOG_PROVIDER` setting.
    fn name(&self) -> &'static str;

    /// Looks up tracks matching a free-text query.
    ///
    /// Implementations return their best results in the order the provider ranks
    /// them; scoring in [`Catalog::resolve`] may reorder them.
    async fn search(&self, query: &str) -> anyhow::Result<Vec<CatalogTrack>>;
}

/// Deezer's public search API, which needs no key or account.
pub struct Deezer {
    /// Held rather than built per call: see [`catalog_http_client`].
    client: reqwest::Client,
}

impl Deezer {
    /// Builds a provider, or explains why the HTTP client could not be made.
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            client: catalog_http_client()?,
        })
    }
}

#[async_trait]
impl CatalogProvider for Deezer {
    fn name(&self) -> &'static str {
        "deezer"
    }

    async fn search(&self, query: &str) -> anyhow::Result<Vec<CatalogTrack>> {
        let response = self
            .client
            .get("https://api.deezer.com/search")
            .query(&[("q", query), ("limit", "10")])
            .send()
            .await
            .with_context(|| format!("Deezer search failed for '{query}'"))?;
        let status = response.status();
        anyhow::ensure!(
            status.is_success(),
            "Deezer search returned HTTP {status} for '{query}'"
        );

        // Deezer signals a search miss with HTTP 200 and `"error"`, not a 4xx.
        let body: DeezerSearch = response
            .json()
            .await
            .context("failed to decode the Deezer search response")?;
        if let Some(error) = body.error {
            anyhow::bail!("Deezer could not search for '{query}': {error}");
        }

        Ok(body
            .data
            .unwrap_or_default()
            .into_iter()
            .map(|hit| CatalogTrack {
                artist: hit.artist.name,
                // `title` carries the decorations ("Remastered 2011"); the short
                // form is the bare song name, which is what gets matched.
                title: match hit.title_short {
                    Some(short) if !short.trim().is_empty() => short,
                    _ => hit.title,
                },
                duration_secs: hit.duration,
                id: hit.id.to_string(),
                provider: "deezer",
            })
            .collect())
    }
}

#[derive(Deserialize)]
struct DeezerSearch {
    #[serde(default)]
    data: Option<Vec<DeezerHit>>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct DeezerHit {
    id: u64,
    title: String,
    #[serde(default)]
    title_short: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    artist: DeezerArtist,
}

#[derive(Deserialize)]
struct DeezerArtist {
    name: String,
}

/// The iTunes Search API, which needs no key or account.
///
/// The second provider because it is the one Apple ships for this: it is
/// keyless, needs no user agent negotiation, and returns the track's running
/// time in milliseconds alongside the store's own spelling of the title and
/// artist — exactly the three fields [`CatalogTrack`] is.
pub struct ITunes {
    client: reqwest::Client,
}

impl ITunes {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            client: catalog_http_client()?,
        })
    }
}

#[async_trait]
impl CatalogProvider for ITunes {
    fn name(&self) -> &'static str {
        "itunes"
    }

    async fn search(&self, query: &str) -> anyhow::Result<Vec<CatalogTrack>> {
        let response = self
            .client
            .get("https://itunes.apple.com/search")
            .query(&[
                ("term", query),
                ("media", "music"),
                ("entity", "song"),
                ("limit", "10"),
            ])
            .send()
            .await
            .with_context(|| format!("iTunes search failed for '{query}'"))?;
        let status = response.status();
        anyhow::ensure!(
            status.is_success(),
            "iTunes search returned HTTP {status} for '{query}'"
        );

        let body: ITunesSearch = response
            .json()
            .await
            .context("failed to decode the iTunes search response")?;

        Ok(body
            .results
            .into_iter()
            .filter_map(ITunesHit::into_track)
            .collect())
    }
}

#[derive(Deserialize)]
struct ITunesSearch {
    #[serde(default)]
    results: Vec<ITunesHit>,
}

#[derive(Deserialize)]
struct ITunesHit {
    #[serde(rename = "trackId")]
    track_id: Option<u64>,
    #[serde(rename = "trackName")]
    track_name: String,
    #[serde(rename = "artistName")]
    artist_name: String,
    /// Milliseconds. The store spells the duration this way rather than in
    /// seconds, which is the one field that needs converting.
    #[serde(rename = "trackTimeMillis", default)]
    track_time_millis: Option<f64>,
}

impl ITunesHit {
    /// Converts a search hit, or drops it when it reports no running time.
    ///
    /// Dropping rather than admitting an unknown length is deliberate: length
    /// is the strongest signal the ranking has, and a track that cannot be
    /// scored on it mostly contributes noise.
    fn into_track(self) -> Option<CatalogTrack> {
        let millis = self.track_time_millis.filter(|ms| *ms > 0.0)?;
        Some(CatalogTrack {
            artist: self.artist_name,
            title: self.track_name,
            duration_secs: Some(millis / 1000.0),
            id: self.track_id.unwrap_or_default().to_string(),
            provider: "itunes",
        })
    }
}

/// The shared HTTP client every provider builds from.
///
/// Held rather than built per call: `reqwest::Client` owns a connection pool,
/// and a client dropped at the end of each search throws the pool away with it
/// -- so every spoken command would pay for a fresh TCP and TLS handshake to
/// look up one song. The user agent is required rather than cosmetic, because
/// Deezer rejects requests that do not identify themselves.
fn catalog_http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(crate::config::HTTP_USER_AGENT)
        .build()
        .context("failed to build the catalog HTTP client")
}

/// What a spoken request asked for, before the catalog corrected it.
#[derive(Debug, Clone)]
pub struct Target {
    pub title: Option<String>,
    pub artist: Option<String>,
    /// The whole request text, kept so scoring can tell "asked for the remix"
    /// from "a remix was in the way".
    pub raw: String,
}

impl Target {
    pub fn from_request(request: &PlayRequest) -> Self {
        Self {
            title: request.title.clone(),
            artist: request.artist.clone(),
            raw: request.query.clone(),
        }
    }

    /// The query to send to the catalog.
    ///
    /// Prefers the structured pair the parser found, because `"<artist>
    /// <title>"` ranks far better than the raw phrase, and falls back to
    /// whatever was heard when the grammar produced no "by" clause.
    pub fn query(&self) -> String {
        match (&self.artist, &self.title) {
            (Some(artist), Some(title)) => format!("{artist} {title}"),
            (None, Some(title)) => title.clone(),
            _ => self.raw.clone(),
        }
    }
}

/// Strips the decorations that stop two spellings of one song from matching.
///
/// A catalog writes "Song (feat. Someone) (Remastered 2011)" where speech writes
/// "song featuring someone", and the two must compare equal. Drops bracketed
/// asides, a leading "feat.", trailing version markers, and everything that is
/// not a letter or digit.
pub fn normalize(text: &str) -> String {
    let lower = text.to_lowercase();
    let mut kept = String::with_capacity(lower.len());
    let mut in_parenthetical = false;

    for c in lower.chars() {
        match c {
            // Everything inside "(...)" is an aside: a feature credit, a
            // remaster year, a "Part 2" marker.
            '(' => in_parenthetical = true,
            ')' => in_parenthetical = false,
            _ if in_parenthetical => {}
            // A bracketed version marker on its own: "- Single Version".
            '[' => in_parenthetical = true,
            ']' => in_parenthetical = false,
            c if c.is_alphanumeric() => kept.push(c),
            _ => kept.push(' '),
        }
    }

    let words: Vec<&str> = kept
        .split_whitespace()
        // A feature credit is a suffix on the title as often as a parenthetical,
        // and dropping it keeps "song" matching "song feat. someone".
        .filter(|w| !is_noise_word(w))
        .collect();
    words.join(" ")
}

/// Decorations that carry no identity for matching purposes.
fn is_noise_word(word: &str) -> bool {
    matches!(
        word,
        "feat"
            | "ft"
            | "featuring"
            | "remaster"
            | "remastered"
            | "version"
            | "single"
            | "album"
            | "explicit"
            | "edit"
            | "mono"
            | "stereo"
            | "bonus"
            | "deluxe"
            | "anniversary"
    )
}

/// How similar two normalized strings are, 0.0 to 1.0.
///
/// Jaro-Winkler over the whole string, then a floor for a token that appears in
/// both. The floor matters because speech-to-text drops or adds whole words
/// ("the dark side of the moon" → "dark side moon") and that is a match, while
/// Winkler alone scores the pair well below any usable threshold.
pub fn similarity(left: &str, right: &str) -> f32 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    if left == right {
        return 1.0;
    }
    let base = jaro_winkler(left, right) as f32;
    let shared = left
        .split_whitespace()
        .any(|l| right.split_whitespace().any(|r| l == r));
    if shared { base.max(0.72) } else { base }
}

/// Resolves requests to canonical tracks, with a confidence for each.
pub struct Catalog {
    provider: Box<dyn CatalogProvider>,
    /// Below this, a match is not believed and the caller falls back.
    threshold: f32,
}

impl Catalog {
    pub fn new(provider: Box<dyn CatalogProvider>, threshold: f32) -> Self {
        Self {
            provider,
            threshold,
        }
    }

    /// Builds the catalog described by [`crate::config::Config`].
    ///
    /// Reading the provider from the validated [`crate::config::Config`] keeps
    /// the setting, its accepted values and its threshold in one place instead
    /// of being parsed twice from the environment.
    pub fn from_config(config: &crate::config::Config) -> anyhow::Result<Self> {
        Ok(Self::new(
            Self::provider(&config.catalog_provider)?,
            config.catalog_match_threshold,
        ))
    }

    /// Builds the named provider.
    ///
    /// The only place the provider names are matched, so `CATALOG_PROVIDER` and
    /// a direct call cannot disagree about what exists.
    pub fn provider(name: &str) -> anyhow::Result<Box<dyn CatalogProvider>> {
        match name {
            "deezer" => Ok(Box::new(Deezer::new()?)),
            "itunes" => Ok(Box::new(ITunes::new()?)),
            other => anyhow::bail!("unknown catalog provider '{other}'"),
        }
    }

    pub fn provider_name(&self) -> &'static str {
        self.provider.name()
    }

    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    /// Finds the catalog track a request is asking for.
    ///
    /// Returns `None` when nothing scores above [`Self::threshold`], which is
    /// the caller's signal to fall back to the raw transcript: a wrong
    /// confident answer is worse than a fuzzy one, because it plays the wrong
    /// song without complaint.
    ///
    /// Bounded by [`LOOKUP_TIMEOUT`] however the provider behaves. A lookup
    /// that hangs is reported as no match rather than as an error, because the
    /// catalog is an improvement on the raw transcript and must never be the
    /// reason a command goes unanswered: the caller searches on the words it
    /// heard instead.
    pub async fn resolve(&self, target: &Target) -> anyhow::Result<Option<Resolution>> {
        let query = target.query();
        if query.trim().is_empty() {
            return Ok(None);
        }

        let hits = match timeout(LOOKUP_TIMEOUT, self.provider.search(&query)).await {
            Ok(hits) => hits?,
            Err(_) => {
                tracing::debug!(
                    provider = self.provider.name(),
                    query = %query,
                    budget = ?LOOKUP_TIMEOUT,
                    "catalog lookup timed out; searching on the transcript alone"
                );
                return Ok(None);
            }
        };
        let Some(tied) = tied_hits(target, &hits) else {
            return Ok(None);
        };
        let (confidence, track) = tied.best[0].clone();
        tracing::debug!(
            provider = self.provider.name(),
            query = %query,
            artist = %track.artist,
            title = %track.title,
            duration = ?track.duration_secs,
            confidence = confidence,
            threshold = self.threshold,
            ambiguous = tied.ambiguous,
            "catalog match"
        );

        if confidence < self.threshold {
            return Ok(None);
        }
        Ok(Some(Resolution {
            track,
            confidence,
            query,
            ambiguous: tied.ambiguous,
        }))
    }
}

/// The hits sharing the top score, and whether they name different artists.
struct Tied {
    /// Best first; the catalog's own order decides among equals.
    best: Vec<(f32, CatalogTrack)>,
    ambiguous: bool,
}

/// The hits sharing the top score, and whether they name different artists.
///
/// A request naming no artist and a title many people have recorded scores
/// every one of those recordings identically, so this reports the tie instead of
/// picking a winner: choosing one silently is how "play look at me" ends up
/// playing whichever artist happened to have the shortest name.
fn tied_hits(target: &Target, hits: &[CatalogTrack]) -> Option<Tied> {
    let mut best: Vec<(f32, CatalogTrack)> = Vec::new();
    let mut top = 0.0f32;
    for track in hits {
        let score = confidence(target, track);
        if score <= 0.0 {
            continue;
        }
        if score > top {
            top = score;
            best.clear();
            best.push((score, track.clone()));
        } else if score == top {
            best.push((score, track.clone()));
        }
    }
    if best.is_empty() {
        return None;
    }
    // Only an unstated artist can make two records ambiguous: naming one has
    // already separated them.
    let ambiguous = target.artist.is_none()
        && best
            .iter()
            .map(|(_, track)| normalize(&track.artist))
            .collect::<HashSet<_>>()
            .len()
            > 1;
    Some(Tied { best, ambiguous })
}

fn confidence(target: &Target, track: &CatalogTrack) -> f32 {
    let got_title = normalize(&track.title);
    if got_title.is_empty() {
        return 0.0;
    }

    // The best evidence available. Most requests have no "by" clause -- the
    // parser only fills the title when it sees one -- so the common case is a
    // bare phrase that has to be matched without a title to compare.
    let title_score = match target
        .title
        .as_deref()
        .map(normalize)
        .filter(|title| !title.is_empty())
    {
        Some(wanted) => similarity(&wanted, &got_title),
        None => phrase_title_score(&normalize(&target.raw), track),
    };

    let score = match target
        .artist
        .as_deref()
        .map(normalize)
        .filter(|artist| !artist.is_empty())
    {
        Some(wanted_artist) => {
            let artist_score = similarity(&wanted_artist, &normalize(&track.artist));
            // 0.7 title + 0.3 artist. A perfect title with a badly wrong artist
            // still clears the bar, which is the common "... by <misheard
            // artist>" case that the catalog exists to fix.
            0.7 * title_score + 0.3 * artist_score
        }
        None => title_score,
    };

    score.clamp(0.0, 1.0)
}

/// How well a request with no "by" clause names a track.
///
/// Scoring the whole phrase against the title does not work, because the
/// request usually carries more than a title: "drakula tame impala" is a
/// misheard song followed by its artist, and comparing that as one string
/// penalises the artist for being mentioned. So the best-matching word carries
/// the title score, and a request that also names the track's artist gets the
/// full score rather than a discount -- which is what separates the requested
/// track from another artist's song with a similar name.
fn phrase_title_score(phrase: &str, track: &CatalogTrack) -> f32 {
    if phrase.is_empty() {
        return 0.0;
    }
    let got_title = normalize(&track.title);
    let score = phrase
        .split_whitespace()
        .map(|word| similarity(word, &got_title))
        .fold(0.0f32, f32::max);

    let got_artist = normalize(&track.artist);
    if got_artist.is_empty() {
        return score;
    }
    let names_artist = got_artist
        .split_whitespace()
        .all(|word| phrase.split_whitespace().any(|said| said == word));
    if names_artist {
        score
    } else {
        score * UNNAMED_ARTIST_DISCOUNT
    }
}

/// What a request that names no artist is worth against a track whose artist it
/// did not mention.
///
/// A discount rather than a rejection, because asking for a song without saying
/// who recorded it is ordinary. It is enough to separate "Hooligan" from
/// "Hooligans" by someone else without rejecting a genuine match for failing to
/// say "BTS".
const UNNAMED_ARTIST_DISCOUNT: f32 = 0.8;

#[cfg(test)]
mod tests {
    use super::*;

    fn track(artist: &str, title: &str, duration: Option<f64>) -> CatalogTrack {
        CatalogTrack {
            artist: artist.to_string(),
            title: title.to_string(),
            duration_secs: duration,
            id: "1".to_string(),
            provider: "test",
        }
    }

    fn target(title: Option<&str>, artist: Option<&str>) -> Target {
        Target {
            title: title.map(str::to_string),
            artist: artist.map(str::to_string),
            raw: [title, artist]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" "),
        }
    }

    #[test]
    fn normalisation_drops_parenthesised_asides() {
        assert_eq!(normalize("Dracula (feat. The Weeknd)"), "dracula");
        assert_eq!(normalize("Song (Remastered 2011)"), "song");
        assert_eq!(normalize("Song - Single Version"), "song");
        assert_eq!(normalize("Song [Explicit]"), "song");
    }

    #[test]
    fn normalisation_drops_a_trailing_feature_credit() {
        assert_eq!(normalize("Song feat. Someone"), "song someone");
        assert_eq!(normalize("Song ft. Someone"), "song someone");
    }

    #[test]
    fn normalisation_strips_punctuation_and_case() {
        assert_eq!(normalize("BTS!"), "bts");
        assert_eq!(normalize("Don't Stop Me Now"), "don t stop me now");
        assert_eq!(normalize("  spaced   out  "), "spaced out");
    }

    /// The Hangul sits inside the brackets on YouTube, where it is a
    /// parenthetical, but a catalog may spell the title without them, so
    /// non-Latin scripts have to survive when they are not an aside.
    #[test]
    fn normalisation_keeps_non_latin_scripts() {
        assert_eq!(normalize("방탄소년단 Dynamite"), "방탄소년단 dynamite");
        assert_eq!(normalize("BTS (방탄소년단) Dynamite"), "bts dynamite");
    }

    #[test]
    fn normalisation_keeps_the_words_that_identify_a_song() {
        assert_eq!(
            normalize("The Dark Side of the Moon"),
            "the dark side of the moon"
        );
    }

    #[test]
    fn a_decorated_catalog_title_matches_a_plain_request() {
        let t = target(Some("dracula"), None);
        let got = track("Tame Impala", "Dracula (Remastered 2020)", Some(205.0));
        assert!(confidence(&t, &got) > 0.95, "got {}", confidence(&t, &got));
    }

    #[test]
    fn a_misheard_title_fuzzy_matches_the_real_one() {
        // What speech-to-text tends to return for these.
        for heard in ["hoolig", "hoolegan", "hooligan"] {
            let t = target(Some(heard), Some("bts"));
            let got = track("BTS", "Hooligan", Some(182.0));
            assert!(
                confidence(&t, &got) > 0.62,
                "{heard:?} scored {}",
                confidence(&t, &got)
            );
        }
    }

    #[test]
    fn a_shared_token_keeps_a_reordered_title_matching() {
        let t = target(Some("dark side of the moon"), None);
        let got = track("Pink Floyd", "The Dark Side of the Moon", Some(413.0));
        assert!(confidence(&t, &got) > 0.72);
    }

    #[test]
    fn a_different_song_scores_too_low_to_trust() {
        let t = target(Some("dracula"), Some("tame impala"));
        let got = track("BTS", "2.0", Some(235.0));
        assert!(confidence(&t, &got) < 0.62);
    }

    #[test]
    fn a_right_title_with_a_wrong_artist_still_clears_the_bar() {
        let t = target(Some("shape of you"), Some("ed sheeran"));
        let got = track("Ed Sheeran", "Shape of You", Some(234.0));
        assert!(confidence(&t, &got) > 0.9);
    }

    /// The parser only fills a title when it hears "by", so most requests
    /// arrive with no title at all. Treating that as unmatchable would mean the
    /// catalog did nothing for the most common request there is.
    #[test]
    fn a_request_with_no_title_is_still_matched() {
        let t = Target {
            title: None,
            artist: None,
            raw: "drakula tame impala".to_string(),
        };
        let score = confidence(&t, &track("Tame Impala", "Dracula", Some(205.0)));
        assert!(score > 0.62, "a misheard title should still match: {score}");
    }

    /// A title on its own is enough; the request need not name the artist.
    #[test]
    fn a_bare_title_matches_without_naming_the_artist() {
        let t = Target {
            title: None,
            artist: None,
            raw: "dracula".to_string(),
        };
        let score = confidence(&t, &track("Tame Impala", "Dracula", Some(205.0)));
        assert!(score > 0.62, "{score}");
    }

    /// The artist being unmentioned is a discount, not a rejection, so a
    /// request that only says the title still resolves.
    #[test]
    fn an_unnamed_artist_is_discounted_rather_than_rejected() {
        let named = confidence(
            &Target {
                title: None,
                artist: None,
                raw: "dracula tame impala".to_string(),
            },
            &track("Tame Impala", "Dracula", Some(205.0)),
        );
        let unnamed = confidence(
            &Target {
                title: None,
                artist: None,
                raw: "dracula".to_string(),
            },
            &track("Tame Impala", "Dracula", Some(205.0)),
        );
        assert!(unnamed > 0.62, "should still resolve: {unnamed}");
        assert!(
            named > unnamed,
            "naming the artist should help: {named} vs {unnamed}"
        );
    }

    /// The best-matching word carries the score, so a phrase that also names a
    /// different artist is not disqualified for containing it.
    #[test]
    fn a_phrase_containing_a_wrong_artist_still_matches_the_title() {
        let t = Target {
            title: None,
            artist: None,
            raw: "drakula tye digger".to_string(),
        };
        let score = confidence(&t, &track("Tame Impala", "Dracula", Some(205.0)));
        assert!(score > 0.62, "the title should carry it: {score}");
    }

    /// A phrase with nothing like the title in it must not be forced to match.
    #[test]
    fn an_unrelated_phrase_is_not_confident() {
        let t = Target {
            title: None,
            artist: None,
            raw: "that one about the weather".to_string(),
        };
        let score = confidence(&t, &track("Tame Impala", "Dracula", Some(205.0)));
        assert!(score < 0.62, "{score}");
    }

    /// Several versions by one artist are still one song, so the tie is not
    /// ambiguous and the catalog's order stands: the original comes first, which
    /// is what stops a remix displacing it.
    #[test]
    fn a_tie_between_versions_by_one_artist_is_not_ambiguous() {
        let t = Target {
            title: None,
            artist: None,
            raw: "drakula tame impala".to_string(),
        };
        // As Deezer returns it: the original once, the remix three times.
        let hits = [
            track("Tame Impala", "Dracula", Some(205.0)),
            track("Tame Impala", "Dracula (with JENNIE)", Some(209.0)),
            track("Tame Impala", "Dracula (with JENNIE)", Some(241.0)),
            track("Tame Impala", "Dracula (with JENNIE)", Some(209.0)),
        ];
        let tied = tied_hits(&t, &hits).expect("a hit");
        assert!(!tied.ambiguous, "one artist is one song");
        let (_, best) = &tied.best[0];
        assert_eq!(best.duration_secs, Some(205.0), "picked {best:?}");
    }

    /// The reported bug: "play look at me" with no "by" is answered by several
    /// artists, and they all score the same because only the title was given.
    /// Picking one of them is a coin toss, so the tie has to be reported rather
    /// than resolved -- the caller asks instead.
    #[test]
    fn a_title_claimed_by_several_artists_is_ambiguous() {
        let t = Target {
            title: None,
            artist: None,
            raw: "look at me".to_string(),
        };
        let hits = [
            track("Naughty Boy", "Look at Me", Some(200.0)),
            track("Tems", "Look at Me", Some(191.0)),
            track("The Neighbourhood", "Look at Me", Some(180.0)),
        ];
        let tied = tied_hits(&t, &hits).expect("a hit");
        assert!(tied.ambiguous, "three artists answered to one title");
        assert_eq!(tied.best.len(), 3);
    }

    /// Naming the artist is what removes the ambiguity, so a request that does
    /// name one must still resolve to that artist.
    #[test]
    fn naming_the_artist_makes_the_same_title_unambiguous() {
        let t = target(Some("look at me"), Some("naughty boy"));
        let hits = [
            track("Naughty Boy", "Look at Me", Some(200.0)),
            track("Tems", "Look at Me", Some(191.0)),
        ];
        let tied = tied_hits(&t, &hits).expect("a hit");
        assert!(!tied.ambiguous);
        assert_eq!(tied.best[0].1.artist, "Naughty Boy");
    }

    /// A better match still wins outright; the tie only collects equals.
    #[test]
    fn a_clearer_match_beats_a_tied_one() {
        let t = target(Some("hooligan"), Some("bts"));
        let hits = [
            track("BTS", "OK", Some(200.0)),
            track("BTS", "Hooligan", Some(182.0)),
        ];
        let tied = tied_hits(&t, &hits).expect("a hit");
        let (score, best) = &tied.best[0];
        assert_eq!(best.title, "Hooligan");
        assert_eq!(tied.best.len(), 1, "only one hit tied");
        assert!(*score > 0.62, "{score}");
    }

    /// A track with no usable title cannot be matched against anything.
    #[test]
    fn a_track_with_no_title_is_never_confident() {
        let t = target(Some("dracula"), None);
        assert_eq!(
            confidence(&t, &track("Tame Impala", "  ", Some(205.0))),
            0.0
        );
    }

    /// The bug this whole module exists for: the flat search handed back a
    /// different song with more views, because nothing canonicalised the name.
    #[test]
    fn the_corrected_artist_steers_the_query() {
        let t = target(Some("hoolig"), Some("bts"));
        assert_eq!(t.query(), "bts hoolig");
    }

    /// A request with no "by" clause still reaches the catalog.
    #[test]
    fn the_query_falls_back_to_whatever_was_heard() {
        let t = target(Some("dracula"), None);
        assert_eq!(t.query(), "dracula");
        assert_eq!(target(None, None).query(), "");
    }

    /// Both built providers have to answer to their configured name, since
    /// `CATALOG_PROVIDER` is matched by string.
    #[test]
    fn the_built_in_providers_name_themselves() {
        for name in crate::config::SUPPORTED_CATALOG_PROVIDERS {
            assert_eq!(Catalog::provider(name).unwrap().name(), *name);
        }
        let unknown = Catalog::provider("spotify")
            .err()
            .expect("an unknown provider is refused")
            .to_string();
        assert_eq!(unknown, "unknown catalog provider 'spotify'");
    }

    /// A provider that never answers must not hold a spoken command open
    /// forever: the caller falls back to searching the words it heard.
    ///
    /// The wait is bounded by the real [`LOOKUP_TIMEOUT`], which would make this
    /// a two-second test, so the budget is shortened to keep the suite quick
    /// while still exercising the timeout path itself.
    #[tokio::test(start_paused = true)]
    async fn a_provider_that_never_answers_times_out_instead_of_hanging() {
        struct Stalls;

        #[async_trait]
        impl CatalogProvider for Stalls {
            fn name(&self) -> &'static str {
                "stalls"
            }
            async fn search(&self, _query: &str) -> anyhow::Result<Vec<CatalogTrack>> {
                // Never resolves: the only way out is the timeout.
                std::future::pending().await
            }
        }

        let catalog = Catalog::new(Box::new(Stalls), 0.62);
        let resolved = tokio::time::timeout(
            LOOKUP_TIMEOUT * 2,
            catalog.resolve(&target(Some("dracula"), Some("tame impala"))),
        )
        .await
        .expect("resolve must return rather than hang")
        .expect("a timeout is a fallback, not an error");
        assert!(resolved.is_none(), "a stalled lookup yields no match");
    }

    /// A provider that fails outright, and one that is merely slow, must both
    /// leave the caller able to search on the transcript.
    #[tokio::test]
    async fn a_failing_provider_leaves_the_caller_with_no_match() {
        struct Broken;

        #[async_trait]
        impl CatalogProvider for Broken {
            fn name(&self) -> &'static str {
                "broken"
            }
            async fn search(&self, _query: &str) -> anyhow::Result<Vec<CatalogTrack>> {
                anyhow::bail!("upstream is down")
            }
        }

        let catalog = Catalog::new(Box::new(Broken), 0.62);
        assert!(
            catalog
                .resolve(&target(Some("dracula"), Some("tame impala")))
                .await
                .is_err(),
            "an outright failure is still reported; the caller handles it"
        );
    }

    /// iTunes reports the running time in milliseconds. Reading it as seconds
    /// would turn every track into a song longer than any mix, so the
    /// conversion is the whole reason this provider can be scored on length.
    #[test]
    fn itunes_milliseconds_become_seconds() {
        let hit: ITunesHit = serde_json::from_str(
            r#"{"trackId":1440857781,"trackName":"Dynamite","artistName":"BTS",
                "trackTimeMillis":173937}"#,
        )
        .expect("the documented shape decodes");
        let track = hit.into_track().expect("a track has a duration");
        assert_eq!(track.duration_secs, Some(173.937));
        assert_eq!(track.artist, "BTS");
        assert_eq!(track.provider, "itunes");
    }

    /// The store decorates titles the way catalogs do, and the shared
    /// normaliser is what makes them comparable with a spoken request.
    #[test]
    fn a_decorated_itunes_title_still_matches_a_plain_request() {
        let hit: ITunesHit = serde_json::from_str(
            r#"{"trackName":"Dracula (feat. The Weeknd)","artistName":"Tame Impala",
                "trackTimeMillis":205000}"#,
        )
        .unwrap();
        let t = target(Some("dracula"), Some("tame impala"));
        assert!(
            confidence(&t, &hit.into_track().unwrap()) > 0.95,
            "the parenthetical should be ignored"
        );
    }

    /// A result with no running time cannot be scored on length, which removes
    /// the strongest ranking signal, so it is dropped rather than admitted as a
    /// track with an unknown length.
    #[test]
    fn an_itunes_result_with_no_duration_is_dropped() {
        let hit: ITunesHit = serde_json::from_str(
            r#"{"trackId":1,"trackName":"Dracula","artistName":"Tame Impala"}"#,
        )
        .unwrap();
        assert_eq!(hit.into_track(), None);
    }

    #[tokio::test]
    #[ignore = "hits the iTunes Search API"]
    async fn resolves_a_real_song_against_itunes() {
        let catalog = Catalog::new(Catalog::provider("itunes").unwrap(), 0.62);
        let resolved = catalog
            .resolve(&target(Some("hoolig"), Some("bts")))
            .await
            .unwrap()
            .expect("BTS Hooligan is a real song");
        assert_eq!(resolved.track.artist, "BTS");
        assert!(resolved.track.duration_secs.unwrap_or(0.0) > 100.0);
        assert!(resolved.confidence > 0.62);
    }

    #[tokio::test]
    async fn a_provider_that_finds_nothing_resolves_to_none() {
        struct Silent;
        #[async_trait]
        impl CatalogProvider for Silent {
            fn name(&self) -> &'static str {
                "silent"
            }
            async fn search(&self, _query: &str) -> anyhow::Result<Vec<CatalogTrack>> {
                Ok(Vec::new())
            }
        }
        let catalog = Catalog::new(Box::new(Silent), 0.62);
        let resolved = catalog
            .resolve(&target(Some("dracula"), Some("tame impala")))
            .await
            .unwrap();
        assert!(resolved.is_none());
    }

    #[tokio::test]
    async fn a_below_threshold_match_is_reported_as_no_match() {
        struct Wrong;
        #[async_trait]
        impl CatalogProvider for Wrong {
            fn name(&self) -> &'static str {
                "wrong"
            }
            async fn search(&self, _query: &str) -> anyhow::Result<Vec<CatalogTrack>> {
                Ok(vec![track("BTS", "Butter", Some(164.0))])
            }
        }
        let catalog = Catalog::new(Box::new(Wrong), 0.62);
        let resolved = catalog
            .resolve(&target(Some("dracula"), Some("tame impala")))
            .await
            .unwrap();
        assert!(resolved.is_none(), "a wrong song must not be trusted");
    }

    #[tokio::test]
    #[ignore = "hits the Deezer API"]
    async fn resolves_a_real_song_against_deezer() {
        let catalog = Catalog::new(Box::new(Deezer::new().unwrap()), 0.62);
        let resolved = catalog
            .resolve(&target(Some("hoolig"), Some("bts")))
            .await
            .unwrap()
            .expect("BTS Hooligan is a real song");
        assert_eq!(resolved.track.artist, "BTS");
        assert!(resolved.track.duration_secs.unwrap_or(0.0) > 100.0);
        assert!(resolved.confidence > 0.62);
    }
}
