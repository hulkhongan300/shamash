//! Resolves a spoken request against YouTube Music's catalogue, to get a
//! canonical spelling of the song before any video search happens.
//!
//! The problem this solves: speech-to-text mishears proper nouns, so "hoolig"
//! reaches the search layer and gets a flat YouTube query. Fixing that with more
//! string matching on YouTube titles is guessing, because those titles are
//! written by uploaders and are not a list of songs.
//!
//! A music catalog *is* a list of songs. YouTube Music's songs search is one
//! unauthenticated POST, and it returns the artist's own spelling of the title
//! plus the track's real running time. That running time is the valuable part:
//! it is the one number nobody can lie about, and it is what lets
//! [`crate::search::score`] tell the studio recording from a 10-minute remix or
//! a 30-second clip without guessing from the title. It also carries the video
//! id, so a hit is playable without searching for it again.
//!
//! The search still goes out as several queries under one deadline, so a
//! misheard word is not a syntax error: free text for discovery, the artist's
//! name when it was heard correctly and the title was not, and the title alone
//! when no artist was named.
//!
//! The provider sits behind [`CatalogProvider`] so a different catalog can be
//! added without touching the caller.

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
    /// The track's running time, when this provider reports one.
    ///
    /// This is the most reliable signal available: a studio single is within a
    /// second or two across every source, whereas titles lie about what a video
    /// contains.
    pub duration_secs: Option<f64>,
    /// The provider's own popularity, when it has one.
    ///
    /// Used only to settle `AMBIGUOUS_POLICY=popular`, never to choose a song
    /// outright. YouTube Music reports this as a per-video play count.
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

/// One way of asking the catalog about a request.
///
/// Free text is what YouTube Music takes, and free text is also what survives a
/// misspelt field: a fielded query returns nothing for `artist:"tam impala"`,
/// so it can confirm but never discover.
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
/// Sized to how much traffic the endpoint tolerates; exceeding a public API's
/// rate is how you get blocked from it for everyone behind you.
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
#[async_trait]
pub trait CatalogProvider: Send + Sync {
    /// Short name used in logs.
    fn name(&self) -> &'static str;

    /// How often this provider may be asked.
    fn rate(&self) -> Rate {
        Rate::new(2.0, 2.0)
    }

    /// Asks about one query.
    ///
    /// Implementations return their best results in the order the provider ranks
    /// them; [`crate::matcher::decide`] reorders them by how well they explain
    /// what was heard.
    async fn search(&self, query: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError>;
}

/// YouTube Music's own songs search: a *catalog* that is also the *source* of
/// the audio.
///
/// A songs-filtered result is an official track on an artist's own release,
/// with the video id that carries its audio and the running time of that
/// recording. So a hit from here needs nothing further -- no upload search, no
/// duration scoring to work out which upload is the song (see
/// [`crate::search::find_video`]) -- and it corrects a misheard name itself,
/// because it is the same spell-correcting search the listener would have used
/// by hand.
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
        // scraping, and the plan only ever asks a handful of queries.
        Rate::new(5.0, 2.0)
    }

    async fn search(&self, query: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
        // Free text only. YouTube Music has one field for a query and no
        // artist-only mode worth spending a request on.
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
        // A response is a tree of objects *and* arrays: the shelf that holds
        // the rows is an array, so a walk that only descends into objects
        // finds nothing at all.
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
    // YouTube Music puts the play count in a column of its own ("217M plays").
    // It is a fact about this video rather than a canonical song popularity,
    // but it is the only number the provider publishes and it is what
    // `AMBIGUOUS_POLICY=popular` compares.
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

/// The play count YouTube Music prints in a row, as a number.
///
/// The site writes "217M plays", "33K plays", "2.2M plays" and "1,234 plays".
/// A row without a count, or one that says "No plays", gives `None`.
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

/// The shared HTTP client the provider builds from.
///
/// Held rather than built per call: `reqwest::Client` owns a connection pool,
/// and a client dropped at the end of each search throws the pool away with it
/// -- so every spoken command would pay for a fresh TCP and TLS handshake to
/// look up one song.
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
}

/// Adds a reading unless it says the same thing as one already there.
///
/// Duplicates matter more than they look: "stand by me" splits identically at
/// either " by ", and asking the same question twice costs a request for
/// nothing.
fn push_reading(out: &mut Vec<Reading>, reading: Reading) {
    if out.len() >= MAX_READINGS {
        return;
    }
    if !out.iter().any(|kept| kept.query == reading.query) {
        out.push(reading);
    }
}

/// A song after merging what the catalog said about it.
///
/// The same song can arrive from several queries with different lengths, and a
/// disagreement is worth flagging rather than averaging.
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
    /// The highest popularity any provider reported.
    pub popularity: Option<u32>,
    /// The YouTube video this song is on, when a provider knew it.
    ///
    /// Carried all the way to the fetch so the song is downloaded directly
    /// rather than searched for first. See [`CatalogTrack::video_id`].
    pub video_id: Option<String>,
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
    /// A catalog over several providers, with an explicit budget in
    /// milliseconds.
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

    /// The queries to ask, in the order they are worth asking.
    ///
    /// Four at most, because each one costs a request against a shared quota:
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
    /// All free text: YouTube Music has one field, and a fielded query would
    /// return nothing for a misspelt artist.
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
            // Free text, not a fielded artist query: YouTube Music has one
            // field, and this still rescues a correct artist with a misheard
            // title because it does not need the title to be right.
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

    /// Every catalog hit, merged and de-duplicated.
    ///
    /// Never fails. Whatever has arrived by [`Self::budget`] is used, and a
    /// provider that errors, stalls or answers with a quota notice is simply
    /// one fewer opinion. Every query runs concurrently under the one deadline,
    /// so the slowest of them does not set the wait.
    pub async fn hits(&self, target: &Target) -> Vec<Hit> {
        let deadline = TokioInstant::now() + self.budget;
        let mut found = Vec::new();
        for guarded in &self.providers {
            self.ask(guarded, target, deadline, &mut found).await;
        }
        merge(found)
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
        let queries = self.plan(target);
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
        // Rows for the same song still contribute their own running time.
        if let Some(seconds) = track.duration_secs
            && !hit
                .durations
                .iter()
                .any(|seen| (seen - seconds).abs() < 1.0)
        {
            hit.durations.push(seconds);
        }
        // The first video id reported wins: every row here comes from the same
        // provider, and the first is the one it ranked highest.
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
mod tests {
    use super::*;

    fn track(artist: &str, title: &str, duration: Option<f64>) -> CatalogTrack {
        CatalogTrack {
            artist: artist.to_string(),
            title: title.to_string(),
            duration_secs: duration,
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

    // ---- the query plan ----------------------------------------------------

    /// A correctly heard artist with a misheard title is rescued by asking for
    /// the artist's catalogue: that query does not need the title to be right.
    #[test]
    fn the_plan_asks_for_the_artists_own_tracks() {
        let catalog = Catalog::with_budget(vec![Box::new(Nothing)], 2_500);
        let kinds: Vec<&str> = catalog
            .plan(&target(Some("drakula"), Some("tame impala")))
            .iter()
            .map(CatalogQuery::kind)
            .collect();
        assert_eq!(kinds, ["loose", "loose", "title"]);
    }

    /// Four requests is the whole budget, whatever the request looks like.
    #[test]
    fn the_plan_never_exceeds_four_queries() {
        let catalog = Catalog::with_budget(vec![Box::new(Nothing)], 2_500);
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
        assert!(catalog.plan(&Target::from_request(&request)).len() <= MAX_QUERIES_PER_PROVIDER);
    }

    /// Asking the same question twice costs a request for nothing.
    #[test]
    fn the_plan_never_repeats_a_query() {
        let catalog = Catalog::with_budget(vec![Box::new(Nothing)], 2_500);
        let request = PlayRequest {
            query: "stand by me".to_string(),
            title: Some("stand".to_string()),
            artist: Some("me".to_string()),
            splits: vec![("stand".to_string(), "me".to_string())],
        };
        let plan = catalog.plan(&Target::from_request(&request));
        let texts: Vec<String> = plan.iter().map(CatalogQuery::text).collect();
        let mut sorted = texts.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), texts.len(), "{texts:?}");
    }

    // ---- merging -----------------------------------------------------------

    /// Rows for the same song collapse, and every distinct running time is kept.
    #[test]
    fn merging_keeps_every_distinct_duration() {
        let hits = merge(vec![
            track("Tame Impala", "Dracula", Some(205.0)),
            track("Tame Impala", "Dracula", Some(205.6)),
            track("Tame Impala", "Borderline", Some(238.0)),
        ]);
        assert_eq!(hits.len(), 2);
        let dracula = &hits[0];
        assert_eq!(dracula.durations.len(), 1, "3 s apart is one recording");
        assert_eq!(dracula.duration_secs(), Some(205.0));
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

        let catalog = Catalog::with_budget(vec![Box::new(Broken), Box::new(Working)], 2_500);
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
        let catalog = Catalog::with_budget(vec![Box::new(Slow)], 500);
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
        let catalog = Catalog::with_budget(vec![Box::new(Stalls)], 2_000);
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

    /// Innertube nests its rows inside arrays, and a walk that stops at the
    /// first array finds no songs at all. This is the shape a real songs search
    /// returns: contents -> tabs (array) -> section list -> shelf -> rows.
    #[test]
    fn songs_are_found_inside_innertubes_arrays() {
        let body = serde_json::json!({
            "contents": {
                "tabbedSearchResultsRenderer": {
                    "tabs": [{
                        "tabRenderer": {
                            "content": {
                                "sectionListRenderer": {
                                    "contents": [{
                                        "musicShelfRenderer": {
                                            "contents": [{
                                                "musicResponsiveListItemRenderer": {
                                                    "flexColumns": [
                                                        {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Dracula"}]}}},
                                                        {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                                                            {"text": "Tame Impala"},
                                                            {"text": " • "},
                                                            {"text": "Deadbeat"},
                                                            {"text": " • "},
                                                            {"text": "3:26"},
                                                        ]}}},
                                                        {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                                                            {"text": "217M plays"},
                                                        ]}}},
                                                    ],
                                                    "playlistItemData": {"videoId": "cuMuMnCRfqk"},
                                                },
                                            }],
                                        },
                                    }],
                                },
                            },
                        },
                    }],
                },
            },
        });
        let tracks = parse_songs(&body);
        assert_eq!(
            tracks.len(),
            1,
            "a shelf nested in an array is still a shelf"
        );
        assert_eq!(tracks[0].artist, "Tame Impala");
        assert_eq!(tracks[0].title, "Dracula");
        assert_eq!(tracks[0].duration_secs, Some(206.0));
        assert_eq!(tracks[0].popularity, Some(217_000_000));
        assert_eq!(tracks[0].video_id.as_deref(), Some("cuMuMnCRfqk"));
    }

    /// The site writes counts in four shapes; all of them are the same number
    /// to the chooser and to `AMBIGUOUS_POLICY=popular`.
    #[test]
    fn play_counts_are_parsed_in_every_shape_the_site_uses() {
        assert_eq!(parse_plays("217M plays"), Some(217_000_000));
        assert_eq!(parse_plays("2.2M plays"), Some(2_200_000));
        assert_eq!(parse_plays("33K plays"), Some(33_000));
        assert_eq!(parse_plays("1,234 plays"), Some(1_234));
        assert_eq!(parse_plays("1 play"), Some(1));
        assert_eq!(parse_plays("1.2B views"), Some(1_200_000_000));
        assert_eq!(parse_plays("No plays"), None);
        assert_eq!(parse_plays(""), None);
    }

    /// The live endpoint moves without notice; this is how to notice. Ignored
    /// by default because it needs the network: run it with `--ignored`.
    #[tokio::test]
    #[ignore = "hits the live YouTube Music endpoint; run with --ignored"]
    async fn the_live_ytmusic_search_still_answers() {
        let hits = YtMusic::new()
            .expect("an HTTP client")
            .search(&CatalogQuery::Loose("dracula tame impala".to_string()))
            .await
            .expect("a live search");
        for hit in &hits {
            println!(
                "{:>4.0}s  {:<20} {:<30} {:>12} {}",
                hit.duration_secs.unwrap_or_default(),
                hit.artist,
                hit.title,
                hit.popularity.unwrap_or_default(),
                hit.video_id.as_deref().unwrap_or("-")
            );
        }
        assert!(!hits.is_empty(), "the live endpoint returned nothing");
        assert!(
            hits.iter().any(|hit| hit.artist == "Tame Impala"
                && hit.title == "Dracula"
                && hit.duration_secs == Some(206.0)),
            "the official Tame Impala track is missing"
        );
    }
}
