//! Resolves a spoken request against a music catalog, to get a canonical
//! spelling of the song before any video search happens.
//!
//! The problem this solves: speech-to-text mishears proper nouns, so "hoolig"
//! reaches the search layer and gets a flat YouTube query. Fixing that with more
//! string matching on YouTube titles is guessing, because those titles are
//! written by uploaders and are not a list of songs.
//!
//! A music catalog *is* a list of songs. Three keyless public APIs are wired up:
//! Deezer's search, the iTunes Search API, and MusicBrainz's Lucene search.
//! Each returns the artist's own spelling of the title plus the track's real
//! running time. That running time is the valuable part: it is the one number
//! nobody can lie about, and it is what lets [`crate::search::score`] tell the
//! studio recording from a 10-minute remix or a 30-second clip without guessing
//! from the title.
//!
//! Three properties hold no matter what the providers do, and they are the
//! reason this is a fan-out rather than a loop:
//!
//! - **No provider can fail a request.** An HTTP error, a quota, a stall or a
//!   timeout is "that provider had nothing", and the other providers carry on.
//! - **The slowest provider does not set the wait.** Every query runs
//!   concurrently under one deadline; whatever has arrived by then is used.
//! - **A misheard word is not a syntax error.** A strict `artist:"..." track:"..."`
//!   query returns nothing for a misspelt artist, which is why the plan also
//!   sends free text for discovery and asks for an artist's whole catalogue when
//!   the artist was heard correctly and the title was not.
//!
//! Providers sit behind [`CatalogProvider`] so a keyed or local catalog can be
//! added without touching the caller.

use crate::matcher;
use crate::parser::PlayRequest;
use anyhow::Context as _;
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::{Instant as TokioInstant, timeout_at};

/// A track as one provider knows it.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogTrack {
    pub artist: String,
    pub title: String,
    /// The track's running time, when this provider reports one.
    ///
    /// This is the most reliable signal available: a studio single is within a
    /// second or two across every source, whereas titles lie about what a video
    /// contains.
    pub duration_secs: Option<f64>,
    /// Provider-specific id, for logging and for de-duplication.
    pub id: String,
    pub provider: &'static str,
    /// The provider's own popularity, when it has one.
    ///
    /// Deezer reports a `rank`; iTunes and MusicBrainz do not. Used only to
    /// settle `AMBIGUOUS_POLICY=popular`, never to choose a song outright.
    pub popularity: Option<u32>,
    /// The YouTube video this song *is*, when a provider knows it.
    ///
    /// Not the same as a search result: a video id found by searching YouTube
    /// is one upload somebody put up, which is why the search has to rank
    /// several of them. A video id a provider reports is the song itself, so
    /// the rest of the pipeline can use it and never search for it.
    pub video_id: Option<String>,
}

impl CatalogTrack {
    /// A short label for logs and for the "did you mean" embed.
    pub fn label(&self) -> String {
        format!("{} — {}", self.artist, self.title)
    }
}

/// One way of asking a catalog about a request.
///
/// The shapes exist because the providers' own query languages differ and
/// because they fail in different ways. A fielded query is *strict*: Deezer
/// returns nothing for `artist:"tam impala"`, so it can confirm but never
/// discover. Free text is the opposite, and the plan sends both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogQuery {
    /// Free text, for discovery.
    Loose(String),
    /// Fielded, for confirmation.
    Strict { artist: String, title: String },
    /// One artist's catalogue, for a misheard title with a correctly heard artist.
    ArtistTracks(String),
    /// A title with no artist named.
    Title(String),
}

impl CatalogQuery {
    /// The single string form, for the parts of a provider that only take one.
    pub fn text(&self) -> String {
        match self {
            Self::Loose(text) | Self::Title(text) | Self::ArtistTracks(text) => text.clone(),
            Self::Strict { artist, title } => format!("{artist} {title}"),
        }
    }

    /// The same query phrased as free text, for providers with no field syntax.
    pub fn as_loose(&self) -> Self {
        Self::Loose(self.text())
    }

    /// Whether this is the free-text form, which every provider understands.
    pub fn is_loose(&self) -> bool {
        matches!(self, Self::Loose(_))
    }

    /// A short name for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Loose(_) => "loose",
            Self::Strict { .. } => "strict",
            Self::ArtistTracks(_) => "artist",
            Self::Title(_) => "title",
        }
    }
}

/// Why a provider could not answer.
///
/// Typed rather than an [`anyhow::Error`] because the caller does not want the
/// message -- it wants to know whether to open the breaker, whether to count a
/// rate limit against the provider's budget, or simply to carry on. A provider
/// that fails is not an error for the request; it is one less opinion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderError {
    /// The provider could not be reached, or answered with something unusable.
    Unavailable,
    /// The provider is refusing requests for now. Opens the breaker.
    RateLimited,
    /// The provider answered with something this code could not read.
    BadResponse,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let said = match self {
            Self::Unavailable => "unavailable",
            Self::RateLimited => "rate limited",
            Self::BadResponse => "sent something unreadable",
        };
        f.write_str(said)
    }
}

/// How often a provider may be asked.
///
/// Sized to each provider's published limit, because exceeding a public API's
/// rate is how you get blocked from it for everyone behind you:
/// Deezer ~50 requests per 5 s, iTunes ~20 per minute, MusicBrainz 1 per
/// second.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rate {
    /// How many requests may arrive at once.
    pub burst: f64,
    /// The steady rate, in requests per second.
    pub per_second: f64,
}

impl Rate {
    pub const fn new(burst: f64, per_second: f64) -> Self {
        Self { burst, per_second }
    }
}

/// A provider's rate limiter and circuit breaker.
///
/// Both live here because both are about *not asking too often*: the limiter
/// spreads requests out, and the breaker stops asking a provider that is
/// refusing. Neither is about correctness, which is why a request that runs out
/// of budget waiting for a token is answered with "no hits" rather than an
/// error.
#[derive(Debug)]
pub struct Guard {
    rate: Rate,
    bucket: Mutex<Bucket>,
    breaker: Mutex<Breaker>,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: tokio::time::Instant,
}

#[derive(Debug)]
struct Breaker {
    /// Consecutive failures; the breaker opens at [`BREAKER_FAILURES`].
    failures: u32,
    /// When the breaker last opened.
    opened: Option<tokio::time::Instant>,
}

/// Consecutive failures before the breaker opens.
///
/// Three rather than one: a single failed request is a dropped packet, and
/// opening on it would stop a provider that was working a moment ago.
const BREAKER_FAILURES: u32 = 3;

/// How long the breaker stays open before letting one request through.
///
/// Jittered by [`BreAKER_JITTER`], because the providers here fail together --
/// a network partition takes out all of them at the same moment, and a breaker
/// set that all reopens on the same second is a small stampede against a service
/// that is only just well again.
const BREAKER_OPEN: Duration = Duration::from_secs(30);
const BREAKER_JITTER: Duration = Duration::from_secs(30);

impl Guard {
    pub fn new(rate: Rate) -> Self {
        Self {
            rate,
            bucket: Mutex::new(Bucket {
                tokens: rate.burst,
                last: TokioInstant::now(),
            }),
            breaker: Mutex::new(Breaker {
                failures: 0,
                opened: None,
            }),
        }
    }

    /// Whether the breaker is currently refusing requests.
    pub async fn is_open(&self) -> bool {
        let mut breaker = self.breaker.lock().await;
        match breaker.opened {
            Some(at) if at.elapsed() < self.open_for() => true,
            Some(_) => {
                // Half-open: let exactly one request through and wait to see.
                breaker.opened = None;
                breaker.failures = BREAKER_FAILURES - 1;
                false
            }
            None => false,
        }
    }

    /// How long this breaker stays shut, including its share of the jitter.
    ///
    /// Derived from the guard's address rather than stored, because a stored
    /// value would have to be generated with an RNG and the point of the jitter
    /// is only that two breakers for different providers do not agree.
    fn open_for(&self) -> Duration {
        let skew = (self as *const Guard as usize % 7) as u32;
        BREAKER_OPEN + BREAKER_JITTER * skew / 6
    }

    /// Waits until a token is available, or until `deadline` passes.
    ///
    /// Returns false when the budget ran out, which the caller reports as "this
    /// provider had nothing" -- never as an error.
    pub async fn take(&self, deadline: tokio::time::Instant) -> bool {
        loop {
            let wait = {
                let mut bucket = self.bucket.lock().await;
                let now = TokioInstant::now();
                let elapsed = now.duration_since(bucket.last).as_secs_f64();
                bucket.tokens =
                    (bucket.tokens + elapsed * self.rate.per_second).min(self.rate.burst);
                bucket.last = now;
                if bucket.tokens >= 1.0 {
                    bucket.tokens -= 1.0;
                    return true;
                }
                Duration::from_secs_f64((1.0 - bucket.tokens) / self.rate.per_second)
            };
            if now_or_before(deadline, wait) {
                return false;
            }
            tokio::time::sleep(wait).await;
        }
    }

    /// Records how a request went.
    pub async fn record(&self, outcome: Result<(), ProviderError>) {
        let mut breaker = self.breaker.lock().await;
        match outcome {
            Ok(()) => breaker.failures = 0,
            Err(_) => {
                breaker.failures = breaker.failures.saturating_add(1);
                if breaker.failures >= BREAKER_FAILURES && breaker.opened.is_none() {
                    breaker.opened = Some(TokioInstant::now());
                    tracing::warn!(
                        "the catalog provider is failing repeatedly; not asking again for a while"
                    );
                }
            }
        }
    }
}

/// Whether there is still time to wait `wait` and still make `deadline`.
fn now_or_before(deadline: tokio::time::Instant, wait: Duration) -> bool {
    TokioInstant::now() + wait >= deadline
}

/// A source of canonical song metadata.
///
/// Object safe, so the set of providers is chosen at runtime from
/// `CATALOG_PROVIDERS` and swapped without a recompile.
#[async_trait]
pub trait CatalogProvider: Send + Sync {
    /// Short name used in logs and in the `CATALOG_PROVIDERS` setting.
    fn name(&self) -> &'static str;

    /// How often this provider may be asked.
    fn rate(&self) -> Rate {
        Rate::new(2.0, 2.0)
    }

    /// Whether the provider's published rate makes it a slow path.
    ///
    /// MusicBrainz is one request a second, so asking it inside the main
    /// fan-out would spend the whole budget waiting for its limiter. It is
    /// asked only when the fast providers found nothing decisive and there is
    /// still time.
    fn is_slow_path(&self) -> bool {
        false
    }

    /// Whether this provider is only asked when nothing better answered.
    ///
    /// False for the provider whose hits *are* the answer -- see [`YtMusic`].
    /// A fallback is asked after the source of truth, and only when the source
    /// of truth had nothing confident to say, so it can never displace a hit the
    /// source of truth stood behind.
    fn is_fallback(&self) -> bool {
        true
    }

    /// Asks about one query.
    ///
    /// Implementations return their best results in the order the provider ranks
    /// them; [`crate::matcher::decide`] reorders them by how well they explain
    /// what was heard.
    async fn search(&self, query: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError>;
}

/// Deezer's public search API, which needs no key or account.
///
/// ~50 requests per 5 s per IP, and a `User-Agent` is required rather than
/// cosmetic: Deezer rejects requests that do not identify themselves.
///
/// Quota and errors arrive as **HTTP 200 with an `error` body**, so there is no
/// status line to catch and the decode step is the only place that can notice.
pub struct Deezer {
    /// Held rather than built per call: see [`catalog_http_client`].
    client: reqwest::Client,
}

/// How many hits to ask for.
///
/// Ten is what a free-text search returns and is enough for `matcher::decide` to
/// have a choice. The artist-only query asks for more, because it is the query
/// that has to find one specific song among everything that artist recorded.
const DEEZER_HITS: &str = "10";
const DEEZER_ARTIST_HITS: &str = "25";

impl Deezer {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            client: catalog_http_client()?,
        })
    }

    /// Deezer's fielded syntax, as `q`.
    ///
    /// Quoted so a multi-word value is one term. Strict, which is the point of
    /// keeping it separate from the free-text query: `artist:"tam impala"`
    /// returns nothing, and that is how a confirmation query is told apart from
    /// a discovery one.
    fn q(query: &CatalogQuery) -> String {
        match query {
            CatalogQuery::Strict { artist, title } => {
                format!("artist:\"{artist}\" track:\"{title}\"")
            }
            CatalogQuery::ArtistTracks(artist) => format!("artist:\"{artist}\""),
            other => other.text(),
        }
    }

    fn limit(query: &CatalogQuery) -> &'static str {
        match query {
            CatalogQuery::ArtistTracks(_) => DEEZER_ARTIST_HITS,
            _ => DEEZER_HITS,
        }
    }
}

#[async_trait]
impl CatalogProvider for Deezer {
    fn name(&self) -> &'static str {
        "deezer"
    }

    fn rate(&self) -> Rate {
        // ~50 per 5 s, with headroom: the bucket is per process, and several
        // guilds can be resolving at once.
        Rate::new(10.0, 8.0)
    }

    async fn search(&self, query: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
        let response = self
            .client
            .get("https://api.deezer.com/search")
            .query(&[
                ("q", Self::q(query)),
                ("limit", Self::limit(query).to_string()),
            ])
            .send()
            .await
            .map_err(|_| ProviderError::Unavailable)?;
        let status = response.status();
        if status.as_u16() == 429 || status.as_u16() == 403 {
            return Err(ProviderError::RateLimited);
        }
        if !status.is_success() {
            return Err(ProviderError::Unavailable);
        }

        // Deezer signals a miss and a spent quota alike with HTTP 200 and an
        // `error` object, so the body is the only place either can show up.
        let body: DeezerSearch = response
            .json()
            .await
            .map_err(|_| ProviderError::BadResponse)?;
        parse_search(body, &query.text()).map_err(|e| {
            if e.to_string().contains("quota") {
                ProviderError::RateLimited
            } else {
                ProviderError::Unavailable
            }
        })
    }
}

#[derive(Deserialize)]
pub(crate) struct DeezerSearch {
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
    #[serde(default)]
    rank: Option<u32>,
    artist: DeezerArtist,
}

#[derive(Deserialize)]
struct DeezerArtist {
    name: String,
}

/// Turns a decoded Deezer body into tracks, or fails the way Deezer asks.
///
/// Split out from the request so the quota-in-a-200 shape can be tested without
/// a network call: it is a body Deezer returns with HTTP 200, so nothing about
/// the response line says anything is wrong.
pub(crate) fn parse_search(body: DeezerSearch, query: &str) -> anyhow::Result<Vec<CatalogTrack>> {
    if let Some(error) = body.error {
        // Code 4 is Deezer's "quota spent"; the rest are genuine no-data
        // replies, which are a miss rather than a failure.
        let code = error.get("code").and_then(serde_json::Value::as_i64);
        if code == Some(4) {
            anyhow::bail!("Deezer refused '{query}': quota spent");
        }
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
            popularity: hit.rank,
            video_id: None,
        })
        .collect())
}

/// The iTunes Search API, which needs no key or account.
///
/// The secondary provider because it is the one Apple ships for this: keyless,
/// no user-agent negotiation, and it returns the running time in milliseconds
/// alongside the store's spelling of the title and artist.
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

    fn rate(&self) -> Rate {
        // Apple documents roughly 20 calls a minute, so this is under half that
        // and the fan-out only ever sends two queries per request.
        Rate::new(5.0, 1.0 / 3.0)
    }

    async fn search(&self, query: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
        // iTunes has no fielded syntax, so a confirmation query is the same
        // free-text query it would have been anyway.
        let response = self
            .client
            .get("https://itunes.apple.com/search")
            .query(&[
                ("term", query.text()),
                ("media", "music".to_string()),
                ("entity", "song".to_string()),
                ("limit", "10".to_string()),
            ])
            .send()
            .await
            .map_err(|_| ProviderError::Unavailable)?;
        let status = response.status();
        if status.as_u16() == 429 || status.as_u16() == 403 {
            return Err(ProviderError::RateLimited);
        }
        if !status.is_success() {
            return Err(ProviderError::Unavailable);
        }

        let body: ITunesSearch = response
            .json()
            .await
            .map_err(|_| ProviderError::BadResponse)?;
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
    /// Converts a search hit, keeping it even with no running time.
    ///
    /// Dropping a duration-less result used to be deliberate -- length is the
    /// strongest signal the ranking has -- but it threw away the title and
    /// artist, which are exactly what the matcher needs and what a YouTube
    /// search is built from. A track of unknown length still helps; a track
    /// that is not in the list does not.
    fn into_track(self) -> Option<CatalogTrack> {
        Some(CatalogTrack {
            artist: self.artist_name,
            title: self.track_name,
            duration_secs: self
                .track_time_millis
                .filter(|ms| *ms > 0.0)
                .map(|ms| ms / 1000.0),
            id: self.track_id.unwrap_or_default().to_string(),
            provider: "itunes",
            popularity: None,
            video_id: None,
        })
    }
}

/// MusicBrainz's search, which needs no key and understands fuzzy terms.
///
/// The slow path. It is rate-limited to one request a second and requires a
/// `User-Agent` identifying the application with contact information, so it is
/// disabled entirely unless `MUSICBRAINZ_CONTACT` is set -- MusicBrainz's terms
/// are not optional, and an anonymous poller is the thing they ask people not to
/// be.
pub struct MusicBrainz {
    client: reqwest::Client,
    contact: String,
}

impl MusicBrainz {
    /// Fails without contact information, because MusicBrainz's terms require it.
    pub fn new(contact: &str) -> anyhow::Result<Self> {
        Ok(Self {
            client: catalog_http_client()?,
            contact: contact.to_string(),
        })
    }

    /// The Lucene query, with a fuzzy term per word.
    ///
    /// `~` is Lucene's edit-distance term, which is how "drakula~" reaches
    /// *Dracula* where an exact term would not. Applied per word rather than to
    /// the phrase, because the words come from speech-to-text in whatever order
    /// they were spoken and the phrase must not have to match literally.
    fn lucene(query: &CatalogQuery) -> String {
        let field = |value: &str| {
            let words: Vec<String> = value
                .split_whitespace()
                .map(|word| format!("{word}~"))
                .collect();
            format!("({})", words.join(" "))
        };
        match query {
            CatalogQuery::Strict { artist, title } => {
                format!("recording:{} AND artist:{}", field(title), field(artist))
            }
            CatalogQuery::ArtistTracks(artist) => format!("artist:{}", field(artist)),
            CatalogQuery::Title(title) => format!("recording:{}", field(title)),
            CatalogQuery::Loose(text) => match text.split_whitespace().next() {
                Some(first) if text.split_whitespace().count() == 1 => {
                    format!("recording:{}", field(first))
                }
                _ => text
                    .split_whitespace()
                    .map(|word| format!("recording:{word}~"))
                    .collect::<Vec<_>>()
                    .join(" AND "),
            },
        }
    }
}

#[async_trait]
impl CatalogProvider for MusicBrainz {
    fn name(&self) -> &'static str {
        "musicbrainz"
    }

    fn rate(&self) -> Rate {
        // One a second, with a burst of one: the documented limit is a
        // minimum interval, not a quota.
        Rate::new(1.0, 1.0)
    }

    fn is_slow_path(&self) -> bool {
        true
    }

    async fn search(&self, query: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
        let response = self
            .client
            .get("https://musicbrainz.org/ws/2/recording")
            .query(&[
                ("query", Self::lucene(query)),
                ("fmt", "json".to_string()),
                ("limit", "5".to_string()),
            ])
            .header(
                "User-Agent",
                format!("shamash/{} ({})", env!("CARGO_PKG_VERSION"), self.contact),
            )
            .send()
            .await
            .map_err(|_| ProviderError::Unavailable)?;
        let status = response.status();
        if status.as_u16() == 429 || status.as_u16() == 503 {
            return Err(ProviderError::RateLimited);
        }
        if !status.is_success() {
            return Err(ProviderError::Unavailable);
        }

        let body: MusicBrainzSearch = response
            .json()
            .await
            .map_err(|_| ProviderError::BadResponse)?;
        Ok(body
            .recordings
            .into_iter()
            .filter_map(|hit| hit.into_track())
            .collect())
    }
}

#[derive(Deserialize)]
struct MusicBrainzSearch {
    #[serde(default)]
    recordings: Vec<MusicBrainzHit>,
}

#[derive(Deserialize)]
struct MusicBrainzHit {
    #[serde(default)]
    title: String,
    /// Milliseconds, and often absent: MusicBrainz has no length for a lot of
    /// recordings, which is one of the reasons it is the slow path.
    #[serde(default)]
    length: Option<f64>,
    #[serde(default)]
    score: Option<u32>,
    #[serde(default, rename = "artist-credit")]
    artist_credit: Vec<MusicBrainzCredit>,
}

#[derive(Deserialize)]
struct MusicBrainzCredit {
    #[serde(default)]
    name: String,
}

impl MusicBrainzHit {
    fn into_track(self) -> Option<CatalogTrack> {
        if self.title.trim().is_empty() {
            return None;
        }
        let artist = self
            .artist_credit
            .iter()
            .map(|credit| credit.name.trim())
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        Some(CatalogTrack {
            artist,
            title: self.title,
            duration_secs: self.length.filter(|ms| *ms > 0.0).map(|ms| ms / 1000.0),
            id: String::new(),
            provider: "musicbrainz",
            popularity: self.score,
            video_id: None,
        })
    }
}

/// YouTube Music's own songs search, the one catalog here that is both a
/// *catalog* and the *source* of the audio.
///
/// Every other provider in this file is a list of songs someone else maintains.
/// YouTube Music is the thing the bot plays from: a songs-filtered result is an
/// official track on an artist's own release, with the video id that carries its
/// audio and the running time of that recording. So a hit from here needs
/// nothing further -- no upload search, no duration scoring to work out which
/// upload is the song (see [`crate::search::find_video`]) -- and, unlike Deezer
/// or iTunes, it corrects a misheard name itself, because it is the same
/// spell-correcting search the listener would have used by hand.
///
/// Keyless in the sense that matters: no account and no per-user credential. The
/// `key` below is the public InnerTube client key that YouTube ships inside every
/// web page, the same one `music.youtube.com` sends; it identifies the web client
/// and grants nothing.
pub struct YtMusic {
    client: reqwest::Client,
}

/// The InnerTube search endpoint for music.youtube.com.
const YTMUSIC_ENDPOINT: &str = "https://music.youtube.com/youtubei/v1/search";

/// The web client key, which is public and grants nothing.
///
/// shortcut: a hardcoded key and client version. Both go stale when YouTube
/// ships, and a stale pair fails as a 400, which this provider reports as
/// `Unavailable` and the fan-out treats as "no opinion". Bump both, or scrape a
/// fresh pair from music.youtube.com, when songs start coming back empty.
const YTMUSIC_KEY: &str = "AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30";
const YTMUSIC_CLIENT: &str = "WEB_REMIX";
const YTMUSIC_CLIENT_VERSION: &str = "1.20240101.00.00";

/// The `params` blob that asks for songs rather than videos, albums or
/// playlists.
///
/// Read out of the filter chips in an ordinary search response rather than
/// written from memory: the well-known `EgWKAQIYAWoK...` value that most of the
/// web still quotes now selects *albums*, which is a list of albums and exactly
/// the thing this provider exists to avoid.
const YTMUSIC_SONGS_FILTER: &str = "EgWKAQIIAWoSEAQQAxAKEAUQCRAREBAQFRAO";

impl YtMusic {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            client: catalog_http_client()?,
        })
    }
}

#[async_trait]
impl CatalogProvider for YtMusic {
    fn name(&self) -> &'static str {
        "ytmusic"
    }

    fn rate(&self) -> Rate {
        // InnerTube publishes no limit. Two a second with a burst of five is a
        // self-imposed ceiling: it keeps a burst of whispers from looking like
        // scraping, and the whole fan-out only ever asks a handful of queries.
        Rate::new(5.0, 2.0)
    }

    /// Not a fallback: this is the one provider whose answer stands on its own.
    fn is_fallback(&self) -> bool {
        false
    }

    async fn search(&self, query: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
        // Free text only. YouTube Music has one field for a query and no
        // artist-only mode worth spending a request on, so the fielded shapes
        // the other providers use would be the same string sent twice.
        let body = serde_json::json!({
            "context": { "client": {
                "clientName": YTMUSIC_CLIENT,
                "clientVersion": YTMUSIC_CLIENT_VERSION,
                "hl": "en",
            }},
            "query": query.text(),
            "params": YTMUSIC_SONGS_FILTER,
        });
        let response = self
            .client
            .post(format!("{YTMUSIC_ENDPOINT}?key={YTMUSIC_KEY}"))
            .json(&body)
            .header("Origin", "https://music.youtube.com")
            .header("X-Goog-Api-Format-Version", "1")
            .header("X-YouTube-Client-Name", "67")
            .header("X-YouTube-Client-Version", YTMUSIC_CLIENT_VERSION)
            .send()
            .await
            .map_err(|_| ProviderError::Unavailable)?;
        let status = response.status();
        if status.as_u16() == 429 || status.as_u16() == 403 {
            return Err(ProviderError::RateLimited);
        }
        if !status.is_success() {
            return Err(ProviderError::Unavailable);
        }
        // Decoded as a tree rather than into a typed body: InnerTube is a
        // renderer tree whose shape changes with the site's experiments, and a
        // hand-written struct would reject the whole response the first time a
        // field moved rather than reading the songs still in there.
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|_| ProviderError::BadResponse)?;
        Ok(parse_songs(&body))
    }
}

/// Every song in an InnerTube response, best first, in the order YouTube ranked
/// them.
///
/// The song rows live at different depths in different responses -- a
/// `musicShelfRenderer` one end of a songs-filtered search, an
/// `itemSectionRenderer` in between when a card is inserted -- so the tree is
/// walked rather than addressed. The walk is linear in a ~600 kB response, which
/// is nothing next to the HTTP round trip it replaces.
pub(crate) fn parse_songs(body: &serde_json::Value) -> Vec<CatalogTrack> {
    fn walk(node: &serde_json::Value, out: &mut Vec<CatalogTrack>) {
        let Some(fields) = node.as_object() else {
            return;
        };
        for (key, value) in fields {
            if key == "musicResponsiveListItemRenderer" {
                if let Some(track) = song(value) {
                    out.push(track);
                }
                continue;
            }
            walk(value, out);
        }
    }
    let mut out = Vec::new();
    walk(body, &mut out);
    out
}

/// One `musicResponsiveListItemRenderer` as a track, when it is a song.
fn song(row: &serde_json::Value) -> Option<CatalogTrack> {
    let title = column(row, 0)?;
    // The subtitle is runs, not one string: "Ben E. King • Don't Play That Song
    // • 2:56". The artist is the first run and the running time the last one
    // that looks like a time, because the album name in between can be
    // anything at all and sometimes is not a name but a count.
    let subtitle = runs(row, 1)?;
    let artist = subtitle.first()?.trim().to_string();
    let duration = subtitle
        .iter()
        .rev()
        .find_map(|run| run.trim().parse_clock().ok())
        .map(|clock| clock.as_secs_f64());
    // A row with no video id is an album or a playlist wearing a song row's
    // shape; there is no audio to play and nothing to hand the fetch layer.
    let video_id = row.pointer("/playlistItemData/videoId")?.as_str()?;
    if artist.is_empty() {
        return None;
    }
    Some(CatalogTrack {
        artist,
        title,
        duration_secs: duration,
        id: video_id.to_string(),
        provider: "ytmusic",
        // YouTube Music reports no popularity, and `AMBIGUOUS_POLICY=popular`
        // is a guess anyway; leaving it unset makes that policy fall back to
        // asking rather than pick a song on a number nobody published.
        popularity: None,
        video_id: Some(video_id.to_string()),
    })
}

/// Every run of one `flexColumns` entry, in order, with the empty ones dropped.
///
/// Kept as a list rather than a joined string because a row's subtitle is
/// several runs carrying different facts -- artist, album, running time -- and
/// the caller has to tell them apart.
fn runs(row: &serde_json::Value, index: usize) -> Option<Vec<String>> {
    let text: Vec<String> = row
        .pointer(&format!(
            "/flexColumns/{index}/musicResponsiveListItemFlexColumnRenderer/text/runs"
        ))?
        .as_array()?
        .iter()
        .filter_map(|run| run.get("text").and_then(serde_json::Value::as_str))
        .map(|run| run.trim().to_string())
        .filter(|run| !run.is_empty())
        .collect();
    (!text.is_empty()).then_some(text)
}

/// One `flexColumns` entry as the single string it reads as.
fn column(row: &serde_json::Value, index: usize) -> Option<String> {
    let text = runs(row, index)?.join("");
    (!text.is_empty()).then_some(text)
}

/// A `m:ss` or `h:mm:ss` running time, as seconds.
///
/// YouTube writes every one of these with at least a minutes field, so
/// "183" is never a duration here and a single bare number is refused rather
/// than read as seconds -- a wrong duration is worse than none, because the
/// upload scorer trusts it completely.
trait Clock {
    fn parse_clock(&self) -> std::result::Result<Duration, ()>;
}

impl Clock for str {
    fn parse_clock(&self) -> std::result::Result<Duration, ()> {
        let mut seconds = 0u64;
        let mut fields = 0;
        for part in self.trim().split(':') {
            let value: u64 = part.trim().parse().map_err(|_| ())?;
            seconds = seconds
                .checked_mul(60)
                .and_then(|s| s.checked_add(value))
                .ok_or(())?;
            fields += 1;
        }
        if !(1..=3).contains(&fields) || seconds == 0 {
            return Err(());
        }
        Ok(Duration::from_secs(seconds))
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
///
/// The request is kept as a *list* of readings rather than one title and one
/// artist, because " by " belongs to more song titles than it separates
/// artists: *Stand By Me*, *Take Me By The Hand*, *By The Way*. Each reading
/// is tried in turn, so a phrase the last-" by "-split reads badly still has
/// the right reading available behind it.
#[derive(Debug, Clone)]
pub struct Target {
    /// Plausible readings, most likely first, never empty for a non-empty
    /// request.
    pub readings: Vec<Reading>,
    /// The whole request text.
    pub raw: String,
}

/// One way of reading a request as "<title> by <artist>".
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// What the speaker said before the " by ", if the reading has one.
    pub title: Option<String>,
    /// What the speaker said after it.
    pub artist: Option<String>,
    /// What to ask the catalog for this reading.
    pub query: String,
}

/// How many readings one request may ask the catalog about.
///
/// Three is generous: a phrase with more than three " by " in it is being read
/// as a conversation, not a song title, and the readings past this point are
/// increasingly absurd.
const MAX_READINGS: usize = 3;

impl Reading {
    fn new(title: Option<String>, artist: Option<String>, raw: &str) -> Self {
        let query = match (title.as_deref(), artist.as_deref()) {
            (Some(artist), Some(title)) => format!("{title} {artist}"),
            (_, Some(artist)) => artist.to_string(),
            _ => raw.to_string(),
        };
        Self {
            title,
            artist,
            query,
        }
    }
}

impl Target {
    /// Every reading of `request`, most likely first.
    pub fn from_request(request: &PlayRequest) -> Self {
        let raw = request.query.clone();
        let guessed = (request.title.clone(), request.artist.clone());
        let mut readings: Vec<Reading> = Vec::new();
        for (title, artist) in std::iter::once(guessed.clone()).chain(
            request
                .splits
                .iter()
                .map(|(title, artist)| (Some(title.clone()), Some(artist.clone()))),
        ) {
            push_reading(&mut readings, Reading::new(title, artist, &raw));
        }
        push_reading(&mut readings, Reading::new(None, None, &raw));
        Self { readings, raw }
    }

    /// The reading to build a YouTube query from: the most likely one.
    pub fn primary(&self) -> &Reading {
        self.readings
            .first()
            .expect("a target always has at least one reading")
    }
}

/// Adds a reading unless it says the same thing as one already there.
///
/// Duplicates matter more than they look: "stand by me" splits identically at
/// either " by ", and asking Deezer the same question twice costs a request
/// from its quota for nothing.
fn push_reading(out: &mut Vec<Reading>, reading: Reading) {
    if out.len() >= MAX_READINGS {
        return;
    }
    if !out.iter().any(|kept| kept.query == reading.query) {
        out.push(reading);
    }
}

/// A song after merging what every provider said about it.
///
/// One provider can be wrong about a name and right about the rest; two that
/// agree on a song are worth more than either alone, and two that disagree about
/// its length are worth flagging rather than averaging.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub artist: String,
    pub title: String,
    /// Every running time reported for it. The shortest is the recording: a
    /// provider that reports a longer one is counting the outro.
    pub durations: Vec<f64>,
    /// Whether providers disagreed about the length by more than
    /// [`search::DURATION_TOLERANCE_SECS`], in which case neither is to be
    /// trusted for ranking uploads.
    pub duration_uncertain: bool,
    /// How many providers named this song.
    pub agreement: usize,
    /// The highest popularity any provider reported.
    pub popularity: Option<u32>,
    /// The YouTube video this song is on, when a provider knew it.
    ///
    /// Carried all the way to the fetch so the song is downloaded directly
    /// rather than searched for first. See [`CatalogTrack::video_id`].
    pub video_id: Option<String>,
    /// Which provider [`Self::video_id`] came from.
    ///
    /// Kept because providers disagree: YouTube Music's id is the album track,
    /// which is what was asked for, while anything else can be an upload of the
    /// song rather than the song.
    pub video_provider: &'static str,
}

impl Hit {
    pub fn label(&self) -> String {
        format!("{} — {}", self.artist, self.title)
    }

    /// The running time to rank YouTube uploads against, when there is one to
    /// trust.
    pub fn duration_secs(&self) -> Option<f64> {
        if self.duration_uncertain {
            return None;
        }
        self.durations
            .iter()
            .copied()
            .reduce(f64::min)
            .filter(|seconds| *seconds > 0.0)
    }

    /// The catalog track to hand the rest of the pipeline.
    ///
    /// Keeps the video id, and names the provider it came from, so a song that
    /// a provider could place on YouTube is downloaded from that video instead
    /// of being searched for.
    pub fn to_track(&self) -> CatalogTrack {
        CatalogTrack {
            artist: self.artist.clone(),
            title: self.title.clone(),
            duration_secs: self.duration_secs(),
            id: String::new(),
            provider: if self.video_id.is_some() {
                self.video_provider
            } else {
                "catalog"
            },
            popularity: self.popularity,
            video_id: self.video_id.clone(),
        }
    }
}

/// A provider and the guard that decides how often it may be asked.
struct Guarded {
    provider: Box<dyn CatalogProvider>,
    guard: Arc<Guard>,
}

/// Resolves requests to catalog hits.
pub struct Catalog {
    providers: Vec<Guarded>,
    /// Below this, nothing is believed and the caller falls back.
    threshold: f32,
    /// How long the whole fan-out may take.
    budget: Duration,
    /// How long the slow path is waited for once the fast providers are done.
    slow_path_reserve: Duration,
}

impl Catalog {
    pub fn new(provider: Box<dyn CatalogProvider>, threshold: f32) -> Self {
        Self::with_budget(
            vec![provider],
            threshold,
            crate::config::DEFAULT_RESOLVE_BUDGET_MS,
        )
    }

    /// A catalog over several providers, with an explicit budget in
    /// milliseconds.
    pub fn with_budget(
        providers: Vec<Box<dyn CatalogProvider>>,
        threshold: f32,
        budget_ms: u64,
    ) -> Self {
        Self {
            providers: providers
                .into_iter()
                .map(|provider| {
                    let guard = Arc::new(Guard::new(provider.rate()));
                    Guarded { provider, guard }
                })
                .collect(),
            threshold,
            budget: Duration::from_millis(budget_ms),
            slow_path_reserve: Duration::from_millis(1000),
        }
    }

    /// Builds the catalog described by [`crate::config::Config`].
    pub fn from_config(config: &crate::config::Config) -> anyhow::Result<Self> {
        let musicbrainz = match config.musicbrainz_contact.as_deref() {
            Some(contact) => Some(contact.to_string()),
            None => {
                if config.catalog_providers.iter().any(|n| n == "musicbrainz") {
                    tracing::info!(
                        "MUSICBRAINZ_CONTACT is unset, so the MusicBrainz fuzzy search is off; \
                         its terms require a user agent with contact details"
                    );
                }
                None
            }
        };
        let mut providers: Vec<Box<dyn CatalogProvider>> = Vec::new();
        for name in &config.catalog_providers {
            if name == "musicbrainz" {
                match &musicbrainz {
                    Some(contact) => providers.push(Box::new(MusicBrainz::new(contact)?)),
                    None => continue,
                }
                continue;
            }
            providers.push(Self::provider(name)?);
        }
        anyhow::ensure!(
            !providers.is_empty(),
            "no usable catalog provider is configured"
        );
        Ok(Self::with_budget(
            providers,
            config.catalog_match_threshold,
            config.resolve_budget.as_millis() as u64,
        ))
    }

    /// Builds the named provider.
    ///
    /// The only place the provider names are matched, so `CATALOG_PROVIDERS` and
    /// a direct call cannot disagree about what exists.
    pub fn provider(name: &str) -> anyhow::Result<Box<dyn CatalogProvider>> {
        match name {
            "deezer" => Ok(Box::new(Deezer::new()?)),
            "itunes" => Ok(Box::new(ITunes::new()?)),
            // Reached only through `from_config`, which checks the contact
            // first; constructing it without one would break MusicBrainz's terms.
            "musicbrainz" => anyhow::bail!(
                "the musicbrainz provider needs MUSICBRAINZ_CONTACT; build it through Config"
            ),
            other => anyhow::bail!("unknown catalog provider '{other}'"),
        }
    }

    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    pub fn provider_names(&self) -> Vec<&'static str> {
        self.providers
            .iter()
            .map(|guarded| guarded.provider.name())
            .collect()
    }

    /// The queries to ask one provider, in the order they are worth asking.
    ///
    /// Four at most, because each one costs a request from a public quota:
    ///
    /// 1. the whole phrase, loose -- the only query that can find a song nobody
    ///    has heard of yet, and the only one that survives a mangled artist;
    /// 2. `"<artist> <title>"` for each of up to two readings -- " by " is
    ///    inside more song titles than it is between an artist and one, so the
    ///    guess is often wrong and another reading may be right;
    /// 3. the artist's own catalogue, when one was heard at all -- this is the
    ///    query that rescues a correct artist with a misheard title, because it
    ///    does not need the title to be right;
    /// 4. the title alone, for a request that named no artist.
    ///
    /// Free text throughout: the fielded [`CatalogQuery::Strict`] form is for
    /// *confirming* a candidate found by one of these, not for discovering one,
    /// because a misspelt field returns nothing at all.
    pub fn plan(&self, target: &Target, provider: &str) -> Vec<CatalogQuery> {
        let mut queries: Vec<CatalogQuery> = Vec::new();
        let push = |query: CatalogQuery, queries: &mut Vec<CatalogQuery>| {
            if queries.len() < MAX_QUERIES_PER_PROVIDER
                && !queries.iter().any(|kept| kept.text() == query.text())
            {
                queries.push(query);
            }
        };

        // iTunes documents roughly 20 calls a minute, which is about one every
        // three seconds, and its searches are the slowest of the fast providers.
        // It gets the first two and nothing else.
        let fast_only = provider == "itunes";
        let splits: Vec<&Reading> = target.readings.iter().collect();

        push(CatalogQuery::Loose(target.raw.clone()), &mut queries);
        for reading in splits.iter().take(2) {
            let text = match (&reading.artist, &reading.title) {
                (Some(artist), Some(title)) => format!("{title} {artist}"),
                _ => continue,
            };
            push(CatalogQuery::Loose(text), &mut queries);
        }
        if fast_only {
            return queries;
        }

        for reading in splits.iter().take(2) {
            let Some(artist) = reading.artist.as_deref() else {
                continue;
            };
            // `ArtistTracks` is Deezer's and MusicBrainz's *fielded* artist
            // query. YouTube Music has one field, so the same request goes out
            // as free text -- which still rescues a correct artist with a
            // misheard title, because it does not need the title to be right.
            push(
                if provider == "ytmusic" {
                    CatalogQuery::Loose(artist.to_string())
                } else {
                    CatalogQuery::ArtistTracks(artist.to_string())
                },
                &mut queries,
            );
        }
        if let Some(title) = target
            .readings
            .iter()
            .find_map(|reading| reading.title.clone())
            .filter(|title| !title.trim().is_empty())
        {
            push(CatalogQuery::Title(title), &mut queries);
        }
        queries
    }

    /// Every catalog hit the providers know about, merged and de-duplicated.
    ///
    /// Never fails. Whatever has arrived by [`Self::budget`] is used, and a
    /// provider that errors, stalls or answers with a quota notice is simply
    /// one fewer opinion.
    ///
    /// In two stages. The source of truth ([`YtMusic`]) goes first, alone: its
    /// results are official tracks with their own running time and their own
    /// video, so mixing a fallback's guesses into the same ranking is how a
    /// confidently wrong song gets played. The fallback providers only run when
    /// that stage produced nothing, nothing confident, or an error -- and when
    /// they do run they are the *whole* answer, not a blend, so there is no
    /// ranking for them to win.
    pub async fn hits(&self, target: &Target) -> Vec<Hit> {
        let deadline = TokioInstant::now() + self.budget;
        let mut found = Vec::new();

        let (primary, fallback): (Vec<&Guarded>, Vec<&Guarded>) = self
            .providers
            .iter()
            .filter(|guarded| !guarded.provider.is_slow_path())
            .partition(|guarded| !guarded.provider.is_fallback());

        for guarded in primary {
            self.ask(guarded, target, deadline, &mut found).await;
        }

        // The fallback providers only run when the source of truth has not
        // already answered: `worth_the_wait` is "nothing found so far explains
        // the request", so asking them otherwise would blend a second opinion
        // into a ranking the first one already settled.
        if self.worth_the_wait(target, &found) {
            for guarded in fallback {
                self.ask(guarded, target, deadline, &mut found).await;
            }

            // The slow path only runs when the fast providers found nothing that
            // already explains the request, and only if there is time for it: at
            // one request a second its own limiter is most of the budget.
            if self.worth_the_wait(target, &found)
                && deadline > TokioInstant::now() + self.slow_path_reserve
            {
                for guarded in self.providers.iter().filter(|g| g.provider.is_slow_path()) {
                    self.ask(guarded, target, deadline, &mut found).await;
                }
            }
        }

        merge(found)
    }

    /// Whether the hits so far are good enough not to spend the slow path.
    fn worth_the_wait(&self, target: &Target, found: &[CatalogTrack]) -> bool {
        // Nothing was found at all, or nothing found is confident on its own:
        // both mean the fast providers had no decisive answer.
        !matches!(
            matcher::decide(&target.raw, found, |t: &CatalogTrack| {
                (t.artist.as_str(), t.title.as_str())
            }),
            Verdict::Confident(_)
        )
    }

    /// Runs one provider's queries, adding whatever it returned.
    ///
    /// All of a provider's queries go out at once rather than one after the
    /// other, under the one shared budget. The plan holds the raw transcript and
    /// every parser reading of it, and for a misheard phrase those are
    /// independent questions whose answers do not depend on each other --
    /// sending them in series would make the wait the sum of four round trips
    /// when it only ever has to be the slowest one. The limiter still decides
    /// which of them are actually served: a query with no token before the
    /// deadline is skipped, not waited for.
    async fn ask(
        &self,
        guarded: &Guarded,
        target: &Target,
        deadline: tokio::time::Instant,
        found: &mut Vec<CatalogTrack>,
    ) {
        if guarded.guard.is_open().await {
            tracing::debug!(
                provider = guarded.provider.name(),
                "the breaker is open; skipping"
            );
            return;
        }
        let queries = self.plan(target, guarded.provider.name());
        let answers = futures::future::join_all(
            queries
                .iter()
                .map(|query| self.ask_one(guarded, query, deadline)),
        )
        .await;
        for tracks in answers {
            found.extend(tracks);
        }
    }

    /// Runs one query, or reports nothing for it.
    async fn ask_one(
        &self,
        guarded: &Guarded,
        query: &CatalogQuery,
        deadline: tokio::time::Instant,
    ) -> Vec<CatalogTrack> {
        if deadline <= TokioInstant::now() {
            tracing::debug!(
                provider = guarded.provider.name(),
                "out of catalog budget; skipping this query"
            );
            return Vec::new();
        }
        if !guarded.guard.take(deadline).await {
            tracing::debug!(
                provider = guarded.provider.name(),
                kind = query.kind(),
                "no rate-limit token before the deadline; skipping this query"
            );
            return Vec::new();
        }

        let started = TokioInstant::now();
        let outcome = timeout_at(deadline, guarded.provider.search(query)).await;
        match outcome {
            Ok(Ok(tracks)) => {
                guarded.guard.record(Ok(())).await;
                tracing::debug!(
                    provider = guarded.provider.name(),
                    kind = query.kind(),
                    query = %query.text(),
                    hits = tracks.len(),
                    took_ms = started.elapsed().as_millis() as u64,
                    "catalog answered"
                );
                tracks
            }
            Ok(Err(e)) => {
                guarded.guard.record(Err(e)).await;
                tracing::warn!(
                    provider = guarded.provider.name(),
                    kind = query.kind(),
                    query = %query.text(),
                    problem = %e,
                    "this provider had nothing; carrying on with the others"
                );
                Vec::new()
            }
            Err(_) => {
                let e = ProviderError::Unavailable;
                guarded.guard.record(Err(e)).await;
                tracing::debug!(
                    provider = guarded.provider.name(),
                    kind = query.kind(),
                    query = %query.text(),
                    "the catalog budget ran out before this answer arrived"
                );
                Vec::new()
            }
        }
    }
}

/// How many queries one provider is asked per request.
const MAX_QUERIES_PER_PROVIDER: usize = 4;

/// The provider whose video id identifies the song rather than an upload of it.
///
/// Named as a constant because the merge rule below is the one place that
/// decides between two providers' ids, and the name is what makes that
/// decision readable.
const YTMUSIC: &str = "ytmusic";

/// Merges provider results into one list, de-duplicated on the song.
fn merge(found: Vec<CatalogTrack>) -> Vec<Hit> {
    let mut hits: Vec<Hit> = Vec::new();
    let mut index: HashMap<(String, String), usize> = HashMap::new();

    for track in found {
        let key = (
            matcher::normalize(&track.artist),
            matcher::normalize(&track.title),
        );
        if key.1.is_empty() {
            continue;
        }
        let Some(slot) = index.get(&key).copied() else {
            let hit = Hit {
                artist: track.artist.clone(),
                title: track.title.clone(),
                durations: track.duration_secs.into_iter().collect(),
                duration_uncertain: false,
                agreement: 1,
                popularity: track.popularity,
                video_id: track.video_id.clone().filter(|id| !id.is_empty()),
                video_provider: track.provider,
            };
            index.insert(key, hits.len());
            hits.push(hit);
            continue;
        };

        let hit = &mut hits[slot];
        // Two providers that both named it is a confidence bonus, and worth
        // knowing even when only one of them reported a length.
        hit.agreement += 1;
        if let Some(seconds) = track.duration_secs
            && !hit
                .durations
                .iter()
                .any(|seen| (seen - seconds).abs() < 1.0)
        {
            hit.durations.push(seconds);
        }
        // Two providers can place the same song on two different videos, and
        // only one of them is the song: YouTube Music reports the album track,
        // while anything else can be reporting an upload of it. So the id from
        // YouTube Music wins, and the first id reported wins over none.
        let wins = match &hit.video_id {
            None => true,
            Some(_) => track.provider == YTMUSIC && hit.video_provider != YTMUSIC,
        };
        if wins && let Some(id) = track.video_id.filter(|id| !id.is_empty()) {
            hit.video_id = Some(id);
            hit.video_provider = track.provider;
        }
        hit.popularity = match (hit.popularity, track.popularity) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
    }

    for hit in &mut hits {
        hit.duration_uncertain = hit.durations.len() > 1
            && hit.durations.iter().all(|seconds| seconds.is_finite())
            && hit.durations.iter().any(|a| {
                hit.durations
                    .iter()
                    .any(|b| (a - b).abs() > crate::search::DURATION_TOLERANCE_SECS)
            });
    }
    hits
}

/// The matcher verdict, re-exported so callers need only one import.
pub use matcher::Verdict;

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
            popularity: None,
            video_id: None,
        }
    }

    fn target(title: Option<&str>, artist: Option<&str>) -> Target {
        let raw = [title, artist]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        Target::from_request(&PlayRequest {
            query: raw.clone(),
            title: title.map(str::to_string),
            artist: artist.map(str::to_string),
            splits: crate::parser::readings(&raw)
                .into_iter()
                .filter_map(|(title, artist)| Some((title?, artist?)))
                .collect(),
        })
    }

    fn heard(phrase: &str) -> Target {
        Target::from_request(&PlayRequest {
            query: phrase.to_string(),
            title: None,
            artist: None,
            splits: crate::parser::readings(phrase)
                .into_iter()
                .filter_map(|(title, artist)| Some((title?, artist?)))
                .collect(),
        })
    }

    // ---- normalisation -----------------------------------------------------

    #[test]
    fn normalisation_drops_parenthesised_asides() {
        assert_eq!(matcher::normalize("Dracula (feat. The Weeknd)"), "dracula");
        assert_eq!(matcher::normalize("Song (Remastered 2011)"), "song");
        assert_eq!(matcher::normalize("Song - Single Version"), "song");
        assert_eq!(matcher::normalize("Song [Explicit]"), "song");
    }

    /// A bracket that *opens* the title is part of it. Dropping it reduced
    /// "(I Can't Get No) Satisfaction" to "satisfaction".
    #[test]
    fn normalisation_keeps_a_leading_parenthetical() {
        assert_eq!(
            matcher::normalize("(I Can't Get No) Satisfaction"),
            "i can t get no satisfaction"
        );
        assert_eq!(
            matcher::normalize("(How Does It Feel) Pios Bolly"),
            "how does it feel pios bolly"
        );
    }

    #[test]
    fn normalisation_drops_a_trailing_feature_credit() {
        assert_eq!(matcher::normalize("Song feat. Someone"), "song someone");
    }

    #[test]
    fn normalisation_strips_punctuation_and_case() {
        assert_eq!(matcher::normalize("BTS!"), "bts");
        assert_eq!(matcher::normalize("Don't Stop Me Now"), "don t stop me now");
        assert_eq!(matcher::normalize("  spaced   out  "), "spaced out");
    }

    #[test]
    fn normalisation_keeps_non_latin_scripts() {
        assert_eq!(
            matcher::normalize("방탄소년단 Dynamite"),
            "방탄소년단 dynamite"
        );
        assert_eq!(
            matcher::normalize("BTS (방탄소년단) Dynamite"),
            "bts dynamite"
        );
    }

    #[test]
    fn normalisation_keeps_the_words_that_identify_a_song() {
        assert_eq!(
            matcher::normalize("The Dark Side of the Moon"),
            "the dark side of the moon"
        );
    }

    // ---- provider decoding -------------------------------------------------

    /// Deezer reports a spent quota as HTTP 200 with an `error` body, so
    /// nothing about the status line says anything is wrong and the decode step
    /// is the only place that can notice.
    #[test]
    fn a_quota_reply_arriving_in_a_200_is_an_error_not_a_track() {
        let body: DeezerSearch =
            serde_json::from_str(include_str!("../tests/fixtures/deezer_quota.json"))
                .expect("the recorded quota body decodes");
        let err = parse_search(body, "dracula tame impala")
            .expect_err("a quota body has no tracks in it")
            .to_string();
        assert!(err.contains("quota"), "unhelpful: {err}");
    }

    #[test]
    fn a_recorded_deezer_response_becomes_tracks() {
        let body: DeezerSearch =
            serde_json::from_str(include_str!("../tests/fixtures/deezer_dracula.json"))
                .expect("the recorded response decodes");
        let tracks = parse_search(body, "dracula tame impala").expect("hits");
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].artist, "Tame Impala");
        assert_eq!(tracks[0].title, "Dracula", "the decorations are dropped");
        assert_eq!(tracks[0].duration_secs, Some(205.0));
        assert_eq!(tracks[0].popularity, Some(90000));
    }

    /// iTunes reports the running time in milliseconds. Reading it as seconds
    /// would turn every track into a song longer than any mix.
    #[test]
    fn itunes_milliseconds_become_seconds() {
        let hit: ITunesHit = serde_json::from_str(
            r#"{"trackId":1440857781,"trackName":"Dynamite","artistName":"BTS",
                "trackTimeMillis":173937}"#,
        )
        .expect("the documented shape decodes");
        let track = hit.into_track().expect("a track");
        assert_eq!(track.duration_secs, Some(173.937));
    }

    /// A result with no running time is still kept. Dropping it threw away the
    /// title and artist, which are exactly what the matcher needs.
    #[test]
    fn an_itunes_result_with_no_duration_is_kept() {
        let hit: ITunesHit = serde_json::from_str(
            r#"{"trackId":1,"trackName":"Dracula","artistName":"Tame Impala"}"#,
        )
        .unwrap();
        let track = hit.into_track().expect("a track with an unknown length");
        assert_eq!(track.duration_secs, None);
        assert_eq!(track.artist, "Tame Impala");
    }

    /// The recorded MusicBrainz shape, including a recording with no length --
    /// which is most of them, and the reason it is only the slow path.
    #[test]
    fn a_recorded_musicbrainz_response_becomes_tracks() {
        let body: MusicBrainzSearch =
            serde_json::from_str(include_str!("../tests/fixtures/musicbrainz_dracula.json"))
                .expect("the recorded response decodes");
        assert_eq!(body.recordings.len(), 2);
        let tracks: Vec<CatalogTrack> = body
            .recordings
            .into_iter()
            .filter_map(MusicBrainzHit::into_track)
            .collect();
        assert_eq!(tracks[0].artist, "Tame Impala");
        assert_eq!(tracks[0].title, "Dracula");
        assert_eq!(
            tracks[0].duration_secs,
            Some(205.0),
            "milliseconds become seconds"
        );
        assert_eq!(tracks[1].duration_secs, None, "length is often absent");
        assert_eq!(tracks[1].provider, "musicbrainz");
    }

    /// The fuzzy term is what makes this provider worth the wait: an exact
    /// Lucene term for "drakula" would not find *Dracula*.
    #[test]
    fn musicbrainz_asks_with_fuzzy_terms() {
        assert_eq!(
            MusicBrainz::lucene(&CatalogQuery::Loose("drakula".to_string())),
            "recording:(drakula~)"
        );
        assert_eq!(
            MusicBrainz::lucene(&CatalogQuery::Strict {
                artist: "tame impala".to_string(),
                title: "drakula".to_string()
            }),
            "recording:(drakula~) AND artist:(tame~ impala~)"
        );
    }

    /// Deezer's fielded syntax, quoted so a multi-word value is one term.
    #[test]
    fn deezer_fielded_queries_are_quoted() {
        assert_eq!(
            Deezer::q(&CatalogQuery::ArtistTracks("tame impala".to_string())),
            "artist:\"tame impala\""
        );
        assert_eq!(
            Deezer::q(&CatalogQuery::Strict {
                artist: "tame impala".to_string(),
                title: "dracula".to_string()
            }),
            "artist:\"tame impala\" track:\"dracula\""
        );
    }

    // ---- the query plan ----------------------------------------------------

    /// A correctly heard artist with a misheard title is rescued by asking for
    /// the artist's catalogue: that query does not need the title to be right.
    #[test]
    fn the_plan_asks_for_the_artists_own_tracks() {
        let catalog = Catalog::new(Box::new(Nothing), 0.72);
        let kinds: Vec<&str> = catalog
            .plan(&target(Some("drakula"), Some("tame impala")), "deezer")
            .iter()
            .map(CatalogQuery::kind)
            .collect();
        assert_eq!(kinds, ["loose", "artist", "title"]);
    }

    /// iTunes is limited to roughly 20 calls a minute, so it gets the two cheap
    /// queries and nothing else.
    #[test]
    fn the_plan_gives_itunes_only_the_cheap_queries() {
        let catalog = Catalog::new(Box::new(Nothing), 0.72);
        let plan = catalog.plan(&target(Some("drakula"), Some("tame impala")), "itunes");
        assert_eq!(plan.len(), 1, "one free-text query is all iTunes gets");
        assert!(plan.iter().all(CatalogQuery::is_loose));
        assert!(
            !plan
                .iter()
                .any(|q| matches!(q, CatalogQuery::ArtistTracks(_)))
        );
    }

    /// Four requests a provider can spare is the whole budget, whatever the
    /// request looks like.
    #[test]
    fn the_plan_never_exceeds_four_queries() {
        let catalog = Catalog::new(Box::new(Nothing), 0.72);
        let request = PlayRequest {
            query: "a by b by c by d".to_string(),
            title: Some("a".to_string()),
            artist: Some("b".to_string()),
            splits: vec![
                ("a".to_string(), "b".to_string()),
                ("a by b".to_string(), "c".to_string()),
                ("a by b by c".to_string(), "d".to_string()),
            ],
        };
        for provider in ["deezer", "itunes", "musicbrainz"] {
            assert!(
                catalog
                    .plan(&Target::from_request(&request), provider)
                    .len()
                    <= MAX_QUERIES_PER_PROVIDER,
                "{provider}"
            );
        }
    }

    /// Asking the same question twice costs quota for nothing.
    #[test]
    fn the_plan_never_repeats_a_query() {
        let catalog = Catalog::new(Box::new(Nothing), 0.72);
        let request = PlayRequest {
            query: "stand by me".to_string(),
            title: Some("stand".to_string()),
            artist: Some("me".to_string()),
            splits: vec![("stand".to_string(), "me".to_string())],
        };
        let plan = catalog.plan(&Target::from_request(&request), "deezer");
        let texts: Vec<String> = plan.iter().map(CatalogQuery::text).collect();
        let mut sorted = texts.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), texts.len(), "{texts:?}");
    }

    // ---- merging -----------------------------------------------------------

    /// Two providers that both named the song agree, and the shortest running
    /// time is the recording.
    #[test]
    fn merging_counts_agreement_and_keeps_every_duration() {
        let hits = merge(vec![
            track("Tame Impala", "Dracula", Some(205.0)),
            track("Tame Impala", "Dracula", Some(205.6)),
            track("Tame Impala", "Borderline", Some(238.0)),
        ]);
        assert_eq!(hits.len(), 2);
        let dracula = &hits[0];
        assert_eq!(dracula.agreement, 2);
        assert_eq!(dracula.durations.len(), 1, "3 s apart is one recording");
        assert_eq!(dracula.duration_secs(), Some(205.0));
        assert_eq!(hits[1].agreement, 1);
    }

    /// Providers that disagree about the length by more than the tolerance are
    /// not to be trusted for ranking uploads, and the hit says so rather than
    /// picking one.
    #[test]
    fn a_length_nobody_agrees_on_is_marked_uncertain() {
        let hits = merge(vec![
            track("Tame Impala", "Dracula", Some(205.0)),
            track("Tame Impala", "Dracula", Some(241.0)),
        ]);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].duration_uncertain);
        assert_eq!(
            hits[0].duration_secs(),
            None,
            "an uncertain length must not be used to rank uploads"
        );
    }

    /// "Song" and "Song (Live)" are one song.
    #[test]
    fn merging_de_dupes_on_the_normalized_song() {
        let hits = merge(vec![
            track("Queen", "Don't Stop Me Now", Some(214.0)),
            track("Queen", "Don't Stop Me Now (Live)", Some(228.0)),
        ]);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn a_hit_with_no_title_is_dropped() {
        assert!(merge(vec![track("Tame Impala", "  ", Some(205.0))]).is_empty());
    }

    // ---- the fan-out -------------------------------------------------------

    struct Nothing;
    #[async_trait]
    impl CatalogProvider for Nothing {
        fn name(&self) -> &'static str {
            "nothing"
        }
        async fn search(&self, _q: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
            Ok(Vec::new())
        }
    }

    /// The whole point of the fan-out: one dead provider must not cost the
    /// request. Before this, `?` on a provider error aborted the whole thing.
    #[tokio::test]
    async fn a_failing_provider_does_not_stop_the_others() {
        struct Broken;
        #[async_trait]
        impl CatalogProvider for Broken {
            fn name(&self) -> &'static str {
                "broken"
            }
            async fn search(&self, _q: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
                Err(ProviderError::Unavailable)
            }
        }
        struct Working;
        #[async_trait]
        impl CatalogProvider for Working {
            fn name(&self) -> &'static str {
                "working"
            }
            async fn search(&self, q: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
                if q.text() == "drakula tame impala" {
                    Ok(vec![track("Tame Impala", "Dracula", Some(205.0))])
                } else {
                    Ok(Vec::new())
                }
            }
        }

        let catalog = Catalog::with_budget(vec![Box::new(Broken), Box::new(Working)], 0.72, 2_500);
        let hits = catalog.hits(&heard("drakula tame impala")).await;
        assert_eq!(hits.len(), 1, "the working provider still answered");
        assert_eq!(hits[0].label(), "Tame Impala — Dracula");
    }

    /// The fan-out returns inside its budget however slow the providers are.
    /// A straggler is dropped, not waited for.
    #[tokio::test(start_paused = true)]
    async fn the_fan_out_returns_inside_its_budget() {
        struct Slow;
        #[async_trait]
        impl CatalogProvider for Slow {
            fn name(&self) -> &'static str {
                "slow"
            }
            fn rate(&self) -> Rate {
                // One a second, so the limiter alone would eat the budget.
                Rate::new(1.0, 1.0)
            }
            async fn search(&self, _q: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(vec![track("Tame Impala", "Dracula", Some(205.0))])
            }
        }
        let catalog = Catalog::with_budget(vec![Box::new(Slow)], 0.72, 500);
        let hits = tokio::time::timeout(
            Duration::from_secs(5),
            catalog.hits(&heard("drakula tame impala")),
        )
        .await
        .expect("the fan-out must not wait for a straggler");
        assert!(hits.is_empty(), "a straggler is dropped, not used");
    }

    /// A provider that never answers must not hold the request open.
    #[tokio::test(start_paused = true)]
    async fn a_provider_that_never_answers_times_out_instead_of_hanging() {
        struct Stalls;
        #[async_trait]
        impl CatalogProvider for Stalls {
            fn name(&self) -> &'static str {
                "stalls"
            }
            async fn search(&self, _q: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
                std::future::pending().await
            }
        }
        let catalog = Catalog::with_budget(vec![Box::new(Stalls)], 0.72, 2_000);
        let hits = tokio::time::timeout(Duration::from_secs(30), catalog.hits(&heard("dracula")))
            .await
            .expect("resolve must return rather than hang");
        assert!(hits.is_empty());
    }

    /// Three failures in a row stop the bot asking that provider, and the
    /// recovery is a half-open: one request through, to see.
    #[tokio::test]
    async fn the_breaker_opens_after_repeated_failures_and_half_opens_afterwards() {
        let guard = Guard::new(Rate::new(10.0, 10.0));
        assert!(!guard.is_open().await, "a fresh guard is closed");

        for _ in 0..BREAKER_FAILURES {
            guard.record(Err(ProviderError::Unavailable)).await;
        }
        assert!(guard.is_open().await, "three failures open it");

        // Nothing but time closes it, and there is no time to wait here.
        let waited = tokio::time::timeout(Duration::from_millis(50), async {}).await;
        assert!(waited.is_ok());
        assert!(
            guard.is_open().await,
            "the breaker stays open until its window passes"
        );
    }

    /// A success closes it again, so one bad minute is not a permanent loss of
    /// a provider.
    #[tokio::test]
    async fn one_success_resets_the_failure_count() {
        let guard = Guard::new(Rate::new(10.0, 10.0));
        guard.record(Err(ProviderError::Unavailable)).await;
        guard.record(Err(ProviderError::Unavailable)).await;
        guard.record(Ok(())).await;
        guard.record(Err(ProviderError::Unavailable)).await;
        guard.record(Err(ProviderError::Unavailable)).await;
        assert!(
            !guard.is_open().await,
            "the run was broken by a success, not by three in a row"
        );
    }

    /// The limiter refuses a request it cannot serve in time rather than
    /// holding the whole fan-out for one token.
    #[tokio::test(start_paused = true)]
    async fn the_limiter_refuses_rather_than_waiting_past_the_deadline() {
        let guard = Guard::new(Rate::new(1.0, 0.1));
        let deadline = TokioInstant::now();
        assert!(guard.take(deadline).await, "the burst is available");
        assert!(
            !guard.take(deadline).await,
            "the next token is ten seconds away and the deadline is now"
        );
    }

    /// With time left, the limiter waits rather than refusing.
    #[tokio::test(start_paused = true)]
    async fn the_limiter_waits_when_there_is_time() {
        let guard = Guard::new(Rate::new(1.0, 100.0));
        let deadline = TokioInstant::now() + Duration::from_secs(5);
        assert!(guard.take(deadline).await);
        assert!(guard.take(deadline).await, "100 a second refills fast");
    }

    /// The slow path is only spent when the fast providers found nothing
    /// decisive, because at one request a second it is most of the budget.
    #[tokio::test(start_paused = true)]
    async fn the_slow_path_is_skipped_when_a_fast_provider_was_decisive() {
        struct Fast;
        #[async_trait]
        impl CatalogProvider for Fast {
            fn name(&self) -> &'static str {
                "fast"
            }
            fn rate(&self) -> Rate {
                Rate::new(10.0, 10.0)
            }
            async fn search(&self, _q: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
                Ok(vec![track("Tame Impala", "Dracula", Some(205.0))])
            }
        }
        struct Slow;
        #[async_trait]
        impl CatalogProvider for Slow {
            fn name(&self) -> &'static str {
                "slow"
            }
            fn is_slow_path(&self) -> bool {
                true
            }
            async fn search(&self, _q: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
                Ok(vec![track("Someone Else", "Some Other Song", Some(200.0))])
            }
        }
        let catalog = Catalog::with_budget(vec![Box::new(Fast), Box::new(Slow)], 0.72, 5_000);
        let hits = catalog.hits(&heard("drakula tame impala")).await;
        assert_eq!(hits.len(), 1, "the slow provider was not asked");
        assert_eq!(hits[0].artist, "Tame Impala");
    }

    /// ...and is asked when the fast providers had nothing.
    #[tokio::test(start_paused = true)]
    async fn the_slow_path_runs_when_the_fast_providers_found_nothing() {
        struct Slow;
        #[async_trait]
        impl CatalogProvider for Slow {
            fn name(&self) -> &'static str {
                "slow"
            }
            fn is_slow_path(&self) -> bool {
                true
            }
            fn rate(&self) -> Rate {
                Rate::new(10.0, 10.0)
            }
            async fn search(&self, _q: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
                Ok(vec![track("Tame Impala", "Dracula", Some(205.0))])
            }
        }
        let catalog = Catalog::with_budget(vec![Box::new(Nothing), Box::new(Slow)], 0.72, 5_000);
        let hits = catalog.hits(&heard("drakula tame impala")).await;
        assert_eq!(hits.len(), 1, "the slow provider rescued it");
        assert_eq!(hits[0].artist, "Tame Impala");
    }

    /// The slow path needs a second of budget to be worth starting, and asking
    /// it with less than that would only spend the request.
    #[tokio::test(start_paused = true)]
    async fn the_slow_path_is_not_started_without_room_to_finish() {
        struct Slow;
        #[async_trait]
        impl CatalogProvider for Slow {
            fn name(&self) -> &'static str {
                "slow"
            }
            fn is_slow_path(&self) -> bool {
                true
            }
            async fn search(&self, _q: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
                Ok(vec![track("Tame Impala", "Dracula", Some(205.0))])
            }
        }
        let catalog = Catalog::with_budget(vec![Box::new(Nothing), Box::new(Slow)], 0.72, 400);
        let hits = catalog.hits(&heard("drakula tame impala")).await;
        assert!(hits.is_empty(), "no room, so no slow path");
    }

    // ---- settings ----------------------------------------------------------

    #[test]
    fn the_built_in_providers_name_themselves() {
        for name in ["deezer", "itunes"] {
            assert_eq!(Catalog::provider(name).unwrap().name(), name);
        }
        // MusicBrainz cannot be built without contact details, by design.
        let err = match Catalog::provider("musicbrainz") {
            Ok(_) => panic!("musicbrainz must not build without a contact"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("MUSICBRAINZ_CONTACT"), "unhelpful: {err}");
        let unknown = Catalog::provider("spotify")
            .err()
            .expect("an unknown provider is refused")
            .to_string();
        assert_eq!(unknown, "unknown catalog provider 'spotify'");
    }

    /// Every documented limit is respected by the limiter configured for it.
    #[test]
    fn each_provider_gets_its_documented_rate() {
        assert!(Catalog::provider("deezer").unwrap().rate().per_second > 5.0);
        assert!(Catalog::provider("itunes").unwrap().rate().per_second <= 1.0);
        assert_eq!(
            Catalog::provider("itunes").unwrap().rate().per_second,
            1.0 / 3.0
        );
        assert_eq!(
            MusicBrainz::new("a@example.com").unwrap().rate().per_second,
            1.0
        );
    }

    /// The threshold moved with the scorer, and its accepted range moved with
    /// it: the old 0.62 was on a scale where a shared word was worth 0.72.
    #[test]
    fn the_catalog_threshold_is_on_the_matchers_scale() {
        assert_eq!(
            crate::config::DEFAULT_CATALOG_MATCH_THRESHOLD,
            matcher::ACCEPT
        );
    }
}
