//! Resolves a spoken request against YouTube Music's catalogue for a canonical song before any video search.
//!
//! Uploader titles are not a song list; the catalog gives the real spelling, running time and video id.

use crate::matcher;
use crate::parser::PlayRequest;
use anyhow::Context as _;
use async_trait::async_trait;
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
    /// Running time; the most reliable signal, unlike titles.
    pub duration_secs: Option<f64>,
    /// Provider popularity (YouTube Music play count); only settles `AMBIGUOUS_POLICY=popular`.
    pub popularity: Option<u32>,
    /// The video this song *is* per the provider, unlike a search hit, so it is never searched for.
    pub video_id: Option<String>,
}

impl CatalogTrack {
    /// A short label for logs and for the "did you mean" embed.
    pub fn label(&self) -> String {
        format!("{} — {}", self.artist, self.title)
    }
}

/// What kind of release an album search row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseKind {
    Album,
    Ep,
    /// One song; YouTube Music lists most songs as singles, so only played as an album when asked.
    Single,
}

impl ReleaseKind {
    /// The kind as YouTube Music writes it, or `None` for non-music (audiobooks, podcasts).
    fn from_label(label: &str) -> Option<Self> {
        match label.trim() {
            "Album" => Some(Self::Album),
            "EP" => Some(Self::Ep),
            "Single" => Some(Self::Single),
            _ => None,
        }
    }

    /// How the kind is shown in the chooser.
    pub fn label(self) -> &'static str {
        match self {
            Self::Album => "Album",
            Self::Ep => "EP",
            Self::Single => "Single",
        }
    }
}

/// An album, EP or single as the catalog knows it.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogAlbum {
    pub title: String,
    pub artist: String,
    pub kind: ReleaseKind,
    pub year: Option<u16>,
    /// The playlist that holds its tracks in order (`OLAK5uy_…`).
    pub playlist_id: String,
}

impl CatalogAlbum {
    /// A short label for logs and messages.
    pub fn label(&self) -> String {
        format!("{} — {}", self.artist, self.title)
    }

    /// The link its tracks are listed from.
    pub fn url(&self) -> String {
        format!(
            "https://music.youtube.com/playlist?list={}",
            self.playlist_id
        )
    }
}

/// One way of asking the catalog; free text, since a fielded query cannot survive a misspelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogQuery {
    /// Free text, for discovery.
    Loose(String),
    /// A title with no artist named.
    Title(String),
}

impl CatalogQuery {
    /// The single string form, for the parts of a provider that only take one.
    pub fn text(&self) -> String {
        match self {
            Self::Loose(text) | Self::Title(text) => text.clone(),
        }
    }

    /// A short name for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Loose(_) => "loose",
            Self::Title(_) => "title",
        }
    }
}

/// Why a provider could not answer; typed so the caller can drive the breaker. Never fails the request.
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

/// How often a provider may be asked; exceeding a public API's rate gets everyone blocked.
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

/// A provider's rate limiter and circuit breaker; running out of budget means "no hits", not an error.
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

/// Consecutive failures before the breaker opens; one failure is just a dropped packet.
const BREAKER_FAILURES: u32 = 3;

/// How long the breaker stays open, plus [`BREAKER_JITTER`] so providers failing together do not reopen together.
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

    /// Open duration, jittered by the guard's address so breakers disagree without an RNG.
    fn open_for(&self) -> Duration {
        let skew = (self as *const Guard as usize % 7) as u32;
        BREAKER_OPEN + BREAKER_JITTER * skew / 6
    }

    /// Waits for a token until `deadline`; false means "no hits", never an error.
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
#[async_trait]
pub trait CatalogProvider: Send + Sync {
    /// Short name used in logs.
    fn name(&self) -> &'static str;

    /// How often this provider may be asked.
    fn rate(&self) -> Rate {
        Rate::new(2.0, 2.0)
    }

    /// Asks about one query; results in provider order, [`crate::matcher::decide`] reorders them.
    async fn search(&self, query: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError>;

    /// Releases (albums, EPs, singles) answering a query, best first.
    async fn albums(&self, _query: &str) -> Result<Vec<CatalogAlbum>, ProviderError> {
        Ok(Vec::new())
    }
}

/// YouTube Music's songs search: a catalog that is also the audio source, so hits need no upload search.
///
/// Needs no account; `key` is the public InnerTube web client key and grants nothing.
pub struct YtMusic {
    client: reqwest::Client,
}

/// The InnerTube search endpoint for music.youtube.com.
const YTMUSIC_ENDPOINT: &str = "https://music.youtube.com/youtubei/v1/search";

/// The web client key, which is public and grants nothing.
///
/// shortcut: hardcoded key and client version go stale (400 -> `Unavailable`); bump them when songs come back empty.
const YTMUSIC_KEY: &str = "AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30";
const YTMUSIC_CLIENT: &str = "WEB_REMIX";
const YTMUSIC_CLIENT_VERSION: &str = "1.20240101.00.00";

/// The `params` blob for songs; the widely quoted `EgWKAQIYAWoK...` now selects albums.
const YTMUSIC_SONGS_FILTER: &str = "EgWKAQIIAWoSEAQQAxAKEAUQCRAREBAQFRAO";

/// The `params` blob for albums, EPs and singles, from the "Albums" filter chip.
const YTMUSIC_ALBUMS_FILTER: &str = "EgWKAQIYAWoQEAkQBRADEAQQEBAVEBEQCg==";

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
        // InnerTube publishes no limit; self-imposed so bursts do not look like scraping.
        Rate::new(5.0, 2.0)
    }

    async fn search(&self, query: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
        // Free text only: YouTube Music has one query field.
        let body = self.post(&query.text(), YTMUSIC_SONGS_FILTER).await?;
        Ok(parse_songs(&body))
    }

    async fn albums(&self, query: &str) -> Result<Vec<CatalogAlbum>, ProviderError> {
        let body = self.post(query, YTMUSIC_ALBUMS_FILTER).await?;
        Ok(parse_albums(&body))
    }
}

impl YtMusic {
    /// One filtered search, as the response tree.
    async fn post(&self, query: &str, filter: &str) -> Result<serde_json::Value, ProviderError> {
        let body = serde_json::json!({
            "context": { "client": {
                "clientName": YTMUSIC_CLIENT,
                "clientVersion": YTMUSIC_CLIENT_VERSION,
                "hl": "en",
            }},
            "query": query,
            "params": filter,
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
        // A tree, not a typed body: InnerTube's shape shifts and a struct would reject it whole.
        response
            .json()
            .await
            .map_err(|_| ProviderError::BadResponse)
    }
}

/// Every song in an InnerTube response, in YouTube's order; rows sit at varying depths, so the tree is walked.
pub(crate) fn parse_songs(body: &serde_json::Value) -> Vec<CatalogTrack> {
    rows(body).into_iter().filter_map(song).collect()
}

/// Every music release with a playlist in an albums-filtered response, in YouTube's order.
pub(crate) fn parse_albums(body: &serde_json::Value) -> Vec<CatalogAlbum> {
    rows(body).into_iter().filter_map(album).collect()
}

/// Every `musicResponsiveListItemRenderer` in a response, in order.
fn rows(body: &serde_json::Value) -> Vec<&serde_json::Value> {
    fn walk<'a>(node: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
        // The shelf holding rows is an array, so arrays must be walked too.
        if let Some(items) = node.as_array() {
            for item in items {
                walk(item, out);
            }
            return;
        }
        let Some(fields) = node.as_object() else {
            return;
        };
        for (key, value) in fields {
            if key == "musicResponsiveListItemRenderer" {
                out.push(value);
                continue;
            }
            walk(value, out);
        }
    }
    let mut out = Vec::new();
    walk(body, &mut out);
    out
}

/// One album row; the subtitle ("Album • A & B • 2017") is split on the bullet since artists span several runs.
fn album(row: &serde_json::Value) -> Option<CatalogAlbum> {
    let title = column(row, 0)?;
    let subtitle: String = row
        .pointer("/flexColumns/1/musicResponsiveListItemFlexColumnRenderer/text/runs")?
        .as_array()?
        .iter()
        .filter_map(|run| run.get("text").and_then(serde_json::Value::as_str))
        .collect();
    let parts: Vec<&str> = subtitle.split('\u{2022}').map(str::trim).collect();
    let kind = ReleaseKind::from_label(parts.first()?)?;
    let artist = parts.get(1).filter(|a| !a.is_empty())?.to_string();
    let year = parts.get(2).and_then(|y| y.parse().ok());
    let playlist_id = row
        .pointer(
            "/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer\
             /playNavigationEndpoint/watchPlaylistEndpoint/playlistId",
        )?
        .as_str()?
        .to_string();
    Some(CatalogAlbum {
        title,
        artist,
        kind,
        year,
        playlist_id,
    })
}

/// One `musicResponsiveListItemRenderer` as a track, when it is a song.
fn song(row: &serde_json::Value) -> Option<CatalogTrack> {
    let title = column(row, 0)?;
    // Subtitle runs: artist first, running time the last clock-like run; the album between can be anything.
    let subtitle = runs(row, 1)?;
    let artist = subtitle.first()?.trim().to_string();
    let duration = subtitle
        .iter()
        .rev()
        .find_map(|run| run.trim().parse_clock().ok())
        .map(|clock| clock.as_secs_f64());
    // No video id means an album or playlist in a song row's shape.
    let video_id = row.pointer("/playlistItemData/videoId")?.as_str()?;
    if artist.is_empty() {
        return None;
    }
    // The play count ("217M plays") is per video, but it is all the provider publishes.
    let popularity = (1..4)
        .filter_map(|index| runs(row, index))
        .flatten()
        .find_map(|run| parse_plays(&run));
    Some(CatalogTrack {
        artist,
        title,
        duration_secs: duration,
        popularity,
        video_id: Some(video_id.to_string()),
    })
}

/// Every non-empty run of one `flexColumns` entry; kept separate since runs carry different facts.
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

/// A row's play count ("217M plays", "2.2M plays", "1,234 plays") as a number.
fn parse_plays(text: &str) -> Option<u32> {
    let text = text.trim();
    let count = text
        .strip_suffix(" plays")
        .or_else(|| text.strip_suffix(" play"))
        .or_else(|| text.strip_suffix(" views"))
        .or_else(|| text.strip_suffix(" view"))?
        .trim();
    let (digits, scale) = match count.as_bytes().last()? {
        b'K' => (&count[..count.len() - 1], 1_000.0),
        b'M' => (&count[..count.len() - 1], 1_000_000.0),
        b'B' => (&count[..count.len() - 1], 1_000_000_000.0),
        _ => (count, 1.0),
    };
    let value: f64 = digits.trim().replace(',', "").parse().ok()?;
    let plays = (value * scale).round();
    (plays.is_finite() && plays >= 0.0).then(|| plays.min(u32::MAX as f64) as u32)
}

/// One `flexColumns` entry as the single string it reads as.
fn column(row: &serde_json::Value, index: usize) -> Option<String> {
    let text = runs(row, index)?.join("");
    (!text.is_empty()).then_some(text)
}

/// A `m:ss` or `h:mm:ss` running time; a bare number is refused, as the scorer fully trusts durations.
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

/// The shared HTTP client; held so its connection pool survives between searches.
fn catalog_http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(crate::config::HTTP_USER_AGENT)
        .build()
        .context("failed to build the catalog HTTP client")
}

/// A request's readings before catalog correction; a list, since " by " is often inside titles (*Stand By Me*).
#[derive(Debug, Clone)]
pub struct Target {
    /// Plausible readings, most likely first; never empty for a non-empty request.
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

/// Max readings per request; past three " by "s it is a conversation, not a title.
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
}

/// Adds a reading unless its query duplicates one ("stand by me" splits alike at either " by ").
fn push_reading(out: &mut Vec<Reading>, reading: Reading) {
    if out.len() >= MAX_READINGS {
        return;
    }
    if !out.iter().any(|kept| kept.query == reading.query) {
        out.push(reading);
    }
}

/// A song after merging provider results; length disagreements are flagged, not averaged.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub artist: String,
    pub title: String,
    /// Every running time reported; the shortest is the recording, longer ones count the outro.
    pub durations: Vec<f64>,
    /// Providers disagreed by more than [`crate::search::DURATION_TOLERANCE_SECS`], so no length is trusted.
    pub duration_uncertain: bool,
    /// The highest popularity any provider reported.
    pub popularity: Option<u32>,
    /// The provider-known YouTube video, carried to the fetch to skip searching. See [`CatalogTrack::video_id`].
    pub video_id: Option<String>,
}

impl Hit {
    pub fn label(&self) -> String {
        format!("{} — {}", self.artist, self.title)
    }

    /// The running time to rank uploads against, when trustworthy.
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

    /// The catalog track for the rest of the pipeline, video id included.
    pub fn to_track(&self) -> CatalogTrack {
        CatalogTrack {
            artist: self.artist.clone(),
            title: self.title.clone(),
            duration_secs: self.duration_secs(),
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
    /// How long the whole fan-out may take.
    budget: Duration,
}

impl Catalog {
    /// A catalog over several providers, with a budget in milliseconds.
    pub fn with_budget(providers: Vec<Box<dyn CatalogProvider>>, budget_ms: u64) -> Self {
        Self {
            providers: providers
                .into_iter()
                .map(|provider| {
                    let guard = Arc::new(Guard::new(provider.rate()));
                    Guarded { provider, guard }
                })
                .collect(),
            budget: Duration::from_millis(budget_ms),
        }
    }

    /// Builds the catalog described by [`crate::config::Config`].
    pub fn from_config(config: &crate::config::Config) -> anyhow::Result<Self> {
        Ok(Self::with_budget(
            vec![Box::new(YtMusic::new()?)],
            config.resolve_budget.as_millis() as u64,
        ))
    }

    /// Up to four free-text queries, in order of worth (each costs quota):
    ///
    /// 1. the whole phrase, which finds unknown songs and survives a mangled artist;
    /// 2. `"<title> <artist>"` for up to two readings, since " by " is often inside titles;
    /// 3. the artist alone, which rescues a misheard title;
    /// 4. the title alone, when no artist was named.
    pub fn plan(&self, target: &Target) -> Vec<CatalogQuery> {
        let mut queries: Vec<CatalogQuery> = Vec::new();
        let push = |query: CatalogQuery, queries: &mut Vec<CatalogQuery>| {
            if queries.len() < MAX_QUERIES_PER_PROVIDER
                && !queries.iter().any(|kept| kept.text() == query.text())
            {
                queries.push(query);
            }
        };

        let splits: Vec<&Reading> = target.readings.iter().collect();

        push(CatalogQuery::Loose(target.raw.clone()), &mut queries);
        for reading in splits.iter().take(2) {
            let text = match (&reading.artist, &reading.title) {
                (Some(artist), Some(title)) => format!("{title} {artist}"),
                _ => continue,
            };
            push(CatalogQuery::Loose(text), &mut queries);
        }
        for reading in splits.iter().take(2) {
            let Some(artist) = reading.artist.as_deref() else {
                continue;
            };
            // Rescues a correct artist with a misheard title.
            push(CatalogQuery::Loose(artist.to_string()), &mut queries);
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

    /// Every catalog hit, merged; never fails, uses whatever arrived within the budget.
    pub async fn hits(&self, target: &Target) -> Vec<Hit> {
        let deadline = TokioInstant::now() + self.budget;
        let answers = futures::future::join_all(
            self.providers
                .iter()
                .map(|guarded| self.ask(guarded, target, deadline)),
        )
        .await;
        merge(answers.into_iter().flatten().collect())
    }

    /// Runs one provider's queries concurrently, so the wait is the slowest, not the sum; tokenless ones are skipped.
    async fn ask(
        &self,
        guarded: &Guarded,
        target: &Target,
        deadline: tokio::time::Instant,
    ) -> Vec<CatalogTrack> {
        if guarded.guard.is_open().await {
            tracing::debug!(
                provider = guarded.provider.name(),
                "the breaker is open; skipping"
            );
            return Vec::new();
        }
        let queries = self.plan(target);
        let answers = futures::future::join_all(
            queries
                .iter()
                .map(|query| self.ask_one(guarded, query, deadline)),
        )
        .await;
        answers.into_iter().flatten().collect()
    }

    /// Runs one query, or reports nothing for it.
    async fn ask_one(
        &self,
        guarded: &Guarded,
        query: &CatalogQuery,
        deadline: tokio::time::Instant,
    ) -> Vec<CatalogTrack> {
        let text = query.text();
        guarded_call(
            guarded,
            query.kind(),
            &text,
            deadline,
            guarded.provider.search(query),
        )
        .await
    }

    /// Releases answering `query`, in provider order; same budget and limits as songs, never fails.
    pub async fn albums(&self, query: &str) -> Vec<CatalogAlbum> {
        let deadline = TokioInstant::now() + self.budget;
        let answers = futures::future::join_all(self.providers.iter().map(|guarded| async move {
            if guarded.guard.is_open().await {
                return Vec::new();
            }
            guarded_call(
                guarded,
                "albums",
                query,
                deadline,
                guarded.provider.albums(query),
            )
            .await
        }))
        .await;
        answers.into_iter().flatten().collect()
    }
}

/// One provider call under its rate limit and budget, feeding the breaker; any failure is just no results.
async fn guarded_call<T>(
    guarded: &Guarded,
    kind: &str,
    query: &str,
    deadline: tokio::time::Instant,
    call: impl std::future::Future<Output = Result<Vec<T>, ProviderError>>,
) -> Vec<T> {
    let provider = guarded.provider.name();
    if deadline <= TokioInstant::now() {
        tracing::debug!(provider, "out of catalog budget; skipping this query");
        return Vec::new();
    }
    if !guarded.guard.take(deadline).await {
        tracing::debug!(
            provider,
            kind,
            "no rate-limit token before the deadline; skipping this query"
        );
        return Vec::new();
    }

    let started = TokioInstant::now();
    match timeout_at(deadline, call).await {
        Ok(Ok(found)) => {
            guarded.guard.record(Ok(())).await;
            tracing::debug!(
                provider,
                kind,
                query,
                hits = found.len(),
                took_ms = started.elapsed().as_millis() as u64,
                "catalog answered"
            );
            found
        }
        Ok(Err(e)) => {
            guarded.guard.record(Err(e)).await;
            tracing::warn!(
                provider,
                kind,
                query,
                problem = %e,
                "this provider had nothing; carrying on with the others"
            );
            Vec::new()
        }
        Err(_) => {
            guarded.guard.record(Err(ProviderError::Unavailable)).await;
            tracing::debug!(
                provider,
                kind,
                query,
                "the catalog budget ran out before this answer arrived"
            );
            Vec::new()
        }
    }
}

/// How many queries one provider is asked per request.
const MAX_QUERIES_PER_PROVIDER: usize = 4;

/// A title with nothing set aside in brackets and no featured artist.
fn is_plain_title(title: &str) -> bool {
    let lower = title.to_lowercase();
    !lower.contains(['(', '['])
        && !lower.contains(" feat.")
        && !lower.contains(" feat ")
        && !lower.contains(" ft.")
}

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
                popularity: track.popularity,
                video_id: track.video_id.clone().filter(|id| !id.is_empty()),
            };
            index.insert(key, hits.len());
            hits.push(hit);
            continue;
        };

        let hit = &mut hits[slot];
        // "Dracula (feat. JENNIE)" normalizes like "Dracula" but is another recording; the plain one wins.
        match (is_plain_title(&hit.title), is_plain_title(&track.title)) {
            (false, true) => {
                hit.title = track.title.clone();
                hit.durations = track.duration_secs.into_iter().collect();
                hit.popularity = track.popularity.or(hit.popularity);
                if let Some(id) = track.video_id.filter(|id| !id.is_empty()) {
                    hit.video_id = Some(id);
                }
                continue;
            }
            (true, false) => continue,
            _ => {}
        }
        // Rows for the same song still contribute their own running time.
        if let Some(seconds) = track.duration_secs
            && !hit
                .durations
                .iter()
                .any(|seen| (seen - seconds).abs() < 1.0)
        {
            hit.durations.push(seconds);
        }
        // First video id wins: the provider ranked it highest.
        if hit.video_id.is_none()
            && let Some(id) = track.video_id.filter(|id| !id.is_empty())
        {
            hit.video_id = Some(id);
        }
        hit.popularity = hit.popularity.or(track.popularity);
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

#[cfg(test)]
#[path = "tests/resolve.rs"]
mod tests;
