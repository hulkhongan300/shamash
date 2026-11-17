//! Chooses which YouTube video to play for a spoken request.
//!
//! The query is built from a catalog match (see [`crate::resolve`]), and results are ranked
//! mainly by running time, which no uploader can misreport, rather than by views.

use crate::library;
use crate::matcher;
use crate::parser::PlayRequest;
use crate::resolve::{self, CatalogTrack};
use anyhow::Context;
use serde::Deserialize;
#[cfg(test)]
use serenity::model::id::GuildId;
use tokio::process::Command;

/// Shorter than this and it is a clip or sound effect, not a song.
const MIN_DURATION_SECS: f64 = 30.0;
/// Longer than this and it is a mix, compilation, or concert recording.
const MAX_DURATION_SECS: f64 = 15.0 * 60.0;
/// Title fragments marking a result as not the song; "lyric" is absent since lyric videos carry the real recording.
const NON_SONG_MARKERS: [&str; 13] = [
    "cover band",
    "cover version",
    "nightcore",
    "slowed",
    "sped up",
    "reverb",
    "remix",
    "karaoke",
    "instrumental",
    "reaction",
    "tutorial",
    "vlog",
    "compilation",
];

/// Title fragments marking a video upload; penalised, not rejected, since an MV is often the best recording.
const VIDEO_MARKERS: [&str; 8] = [
    "official mv",
    "official m/v",
    "music video",
    "visualizer",
    "dance practice",
    "dance video",
    "performance video",
    "dance cover",
];

/// Title fragments marking an audio-only upload.
const AUDIO_MARKERS: [&str; 4] = ["official audio", "audio only", "audio", "official track"];

/// Title fragments marking a lyric video: a still image over the same recording.
const LYRIC_MARKERS: [&str; 2] = ["lyric video", "lyrics"];

/// Max length gap still counted as the same recording (edits, silence at either end).
pub const DURATION_TOLERANCE_SECS: f64 = 4.0;

/// Title words costing [`PENALTY`] unless the request used them (asked-for "live" stays live).
///
/// Single words match whole, so "mix" skips "remix"; "lyric" also covers "lyrics".
const PENALTY_MARKERS: [&str; 13] = [
    "cover",
    "live",
    "remix",
    "lyric",
    "karaoke",
    "sped up",
    "slowed",
    "reverb",
    "instrumental",
    "1 hour",
    "mix",
    "reaction",
    "tutorial",
];

/// Score for a length within tolerance; outweighs the rest, as only length separates the studio cut.
const DURATION_EXACT: f64 = 100.0;
/// Charged per second outside the tolerance.
const DURATION_PENALTY_PER_SEC: f64 = 3.0;
/// Ceiling on that penalty, so a 10-minute mix cannot dominate the arithmetic.
const DURATION_PENALTY_MAX: f64 = 90.0;

/// A "- Topic" channel: YouTube's auto-generated artist channel, the best provenance signal.
const CHANNEL_TOPIC: f64 = 45.0;
/// The upload is on the artist's own channel.
const CHANNEL_ARTIST: f64 = 30.0;
/// A VEVO channel, which is an official label channel.
const CHANNEL_VEVO: f64 = 25.0;
/// A channel that calls itself "official" without a VEVO handle.
const CHANNEL_OFFICIAL: f64 = 15.0;
/// Ceiling for matching the canonical title against the upload's title.
const RELEVANCE_MAX: f64 = 25.0;
/// The upload claims to be the audio on its own.
const MEDIA_AUDIO: f64 = 12.0;
/// A lyric video: video-shaped, but the same recording under a still image.
const MEDIA_LYRIC: f64 = 6.0;
/// An official MV, visualizer or dance video.
const MEDIA_VIDEO: f64 = -6.0;
/// Charged per penalty marker.
const PENALTY: f64 = 30.0;
/// Popularity weight; small and logarithmic, so views only break ties.
const VIEWS_WEIGHT: f64 = 1.0;
/// Popularity ceiling, so 75 M views cannot outvote a length mismatch.
const VIEWS_MAX: f64 = 12.0;

/// One entry of a YouTube search result.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Candidate {
    /// Video title as shown on YouTube.
    pub title: Option<String>,
    /// Watch URL. Not aliased to `webpage_url`: flat listings carry both and serde rejects the duplicate.
    pub url: String,
    /// View count, when YouTube reports one.
    pub view_count: Option<u64>,
    /// Duration in seconds, when known.
    pub duration: Option<f64>,
    /// Uploading channel, used as the artist hint in logs.
    pub channel: Option<String>,
    /// Bare video id for thumbnails; some yt-dlp versions omit it.
    #[serde(default)]
    pub id: Option<String>,
}

impl Candidate {
    pub fn title(&self) -> &str {
        self.title.as_deref().unwrap_or("unknown title")
    }

    pub fn channel(&self) -> &str {
        self.channel.as_deref().unwrap_or("unknown channel")
    }

    /// The video id, from `id` or parsed out of `url`.
    pub fn video_id(&self) -> Option<&str> {
        if let Some(id) = self.id.as_deref().filter(|id| !id.is_empty()) {
            return Some(id);
        }
        url_video_id(&self.url)
    }

    /// The one upload a provider already named for this track; never ranked.
    pub fn for_track(track: &CatalogTrack, video_id: &str) -> Self {
        Self {
            title: Some(track.title.clone()),
            url: format!("https://www.youtube.com/watch?v={video_id}"),
            view_count: None,
            duration: track.duration_secs,
            channel: Some(track.artist.clone()),
            id: Some(video_id.to_string()),
        }
    }

    /// Whether this is worth ranking against `target`.
    ///
    /// Markers are judged against the request, so asking for "live" keeps live recordings.
    fn is_song_for(&self, target: &Target) -> bool {
        let plausible_length = match self.duration {
            Some(duration) => (MIN_DURATION_SECS..=MAX_DURATION_SECS).contains(&duration),
            // Unknown duration: let the other signals decide.
            None => true,
        };
        if !plausible_length {
            return false;
        }
        let title = self.title_lowercase();
        let requested = matcher::normalize(&target.requested);
        !NON_SONG_MARKERS
            .iter()
            .any(|marker| contains_marker(&title, marker) && !contains_marker(&requested, marker))
    }

    fn views(&self) -> u64 {
        self.view_count.unwrap_or(0)
    }

    fn title_lowercase(&self) -> String {
        self.title
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
    }

    /// How audio-like the title claims to be: 3 audio, 2 lyric video, 1 ordinary, 0 MV/visualizer.
    ///
    /// Title-based because a flat search reports no stream details.
    fn media_rank(&self) -> u8 {
        let title = self.title_lowercase();
        if VIDEO_MARKERS.iter().any(|m| title.contains(m)) {
            return 0;
        }
        if LYRIC_MARKERS.iter().any(|m| title.contains(m)) {
            return 2;
        }
        if AUDIO_MARKERS.iter().any(|m| title.contains(m)) {
            return 3;
        }
        1
    }

    fn channel_lowercase(&self) -> String {
        self.channel().to_lowercase()
    }
}

/// `text` if it has anything in it, otherwise `None`.
fn non_empty(text: &str) -> Option<String> {
    (!text.trim().is_empty()).then(|| text.to_string())
}

/// Text without spaces or punctuation, since channels may be `TameImpala` or `Tame Impala`.
fn squashed(text: &str) -> String {
    matcher::normalize(text).replace(' ', "")
}

/// Phrases match by substring, words whole ("mix" skips "remix", "live" skips "delivery");
/// words over 4 chars also match extensions ("lyric" -> "lyrics").
fn contains_marker(haystack: &str, needle: &str) -> bool {
    if needle.contains(' ') {
        return haystack.contains(needle);
    }
    haystack
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word == needle || (needle.len() > 4 && word.starts_with(needle)))
}

/// A reference recording to score uploads against, from a catalog match or the raw transcript.
#[derive(Debug, Clone)]
pub struct Target {
    pub artist: Option<String>,
    pub title: Option<String>,
    /// Every running time the catalog reported; the closest scores, since providers disagree on outros.
    pub durations: Vec<f64>,
    /// The whole request, kept so a requested "live" is not penalised.
    pub requested: String,
    /// Whether a catalog match backed this target.
    pub catalogued: bool,
}

impl Target {
    /// A target backed by a catalog hit, with the corrected names.
    pub fn from_track(track: &CatalogTrack, durations: Vec<f64>, requested: &str) -> Self {
        Self {
            artist: Some(track.artist.clone()),
            title: Some(track.title.clone()),
            durations,
            requested: requested.to_string(),
            catalogued: true,
        }
    }

    /// A target from the transcript alone; the request doubles as the title so relevance still counts.
    pub fn from_request(request: &PlayRequest) -> Self {
        Self {
            artist: request.artist.clone(),
            title: request.title.clone().or_else(|| non_empty(&request.query)),
            durations: Vec::new(),
            requested: request.query.clone(),
            catalogued: false,
        }
    }

    /// What to search YouTube for: `"<artist> <title>"` when known, else the transcript.
    pub fn search_query(&self) -> String {
        match (&self.artist, &self.title) {
            (Some(artist), Some(title)) => format!("{artist} {title}"),
            (None, Some(title)) => title.clone(),
            _ => self.requested.clone(),
        }
    }

    /// The threshold for this target; uncatalogued targets score on a different scale.
    pub fn effective_threshold(&self, threshold: f64) -> f64 {
        if self.catalogued {
            threshold
        } else {
            UNRESOLVED_MATCH_THRESHOLD
        }
    }
}

/// The bar for a request the catalog could not resolve, in points.
///
/// Not a fraction of the default: without a length the max is ~125, not ~190.
/// It only orders the chooser; an unverified result is never autoplayed.
const UNRESOLVED_MATCH_THRESHOLD: f64 = 20.0;

/// A candidate's score, broken down so it can be logged and tuned.
#[derive(Debug, Clone, Default)]
pub struct ScoreParts {
    pub duration: f64,
    pub channel: f64,
    pub relevance: f64,
    pub media: f64,
    pub views: f64,
    /// The markers that cost this candidate points.
    pub penalties: Vec<&'static str>,
}

impl ScoreParts {
    /// The total, one penalty per distinct marker.
    pub fn total(&self) -> f64 {
        self.duration + self.channel + self.relevance + self.media + self.views
            - PENALTY * self.penalties.len() as f64
    }

    /// A one-line explanation, for the debug log.
    pub fn explain(&self) -> String {
        format!(
            "total {:.1} (duration {:+.1}, channel {:+.1}, relevance {:+.1}, media {:+.1}, \
             views {:+.1}{})",
            self.total(),
            self.duration,
            self.channel,
            self.relevance,
            self.media,
            self.views,
            if self.penalties.is_empty() {
                String::new()
            } else {
                format!(
                    ", penalties -{} for {:?}",
                    self.penalties.len() * 30,
                    self.penalties
                )
            }
        )
    }
}

/// A candidate with its score.
#[derive(Debug, Clone)]
pub struct ScoredCandidate {
    pub candidate: Candidate,
    pub parts: ScoreParts,
}

impl ScoredCandidate {
    pub fn score(&self) -> f64 {
        self.parts.total()
    }
}

/// The outcome of choosing between candidates.
#[derive(Debug, Clone)]
pub struct Choice {
    /// Ranked best first.
    pub ranked: Vec<ScoredCandidate>,
    /// Whether the best score cleared the threshold.
    pub confident: bool,
    /// The threshold that was applied, for logging.
    pub threshold: f64,
    /// The catalog match the query was built from, so the caller can say what it thinks was asked for.
    pub resolved: Option<resolve::CatalogTrack>,
    /// What the request was understood as.
    pub requested: String,
}

impl Choice {
    pub fn best(&self) -> Option<&ScoredCandidate> {
        self.ranked.first()
    }

    /// The best `count` results for the chooser, top result included.
    pub fn top(&self, count: usize) -> Vec<&ScoredCandidate> {
        self.ranked.iter().take(count).collect()
    }
}

/// Scores one candidate; length outweighs everything, views count least.
pub fn score(candidate: &Candidate, target: &Target) -> ScoreParts {
    let mut parts = ScoreParts::default();
    let title = candidate.title_lowercase();

    if let (Some(found), true) = (candidate.duration, !target.durations.is_empty()) {
        // Closest provider wins, so one counting the outro cannot drag it away.
        let delta = target
            .durations
            .iter()
            .map(|wanted| (wanted - found).abs())
            .fold(f64::INFINITY, f64::min);
        parts.duration = if delta <= DURATION_TOLERANCE_SECS {
            DURATION_EXACT
        } else {
            -((delta - DURATION_TOLERANCE_SECS) * DURATION_PENALTY_PER_SEC)
                .min(DURATION_PENALTY_MAX)
        };
    }

    let channel = candidate.channel_lowercase();
    if channel.ends_with("- topic") {
        parts.channel += CHANNEL_TOPIC;
    }
    if let Some(artist) = target.artist.as_deref() {
        let artist = squashed(artist);
        if !artist.is_empty() && squashed(&channel).contains(&artist) {
            parts.channel += CHANNEL_ARTIST;
        }
    }
    if channel.contains("vevo") {
        parts.channel += CHANNEL_VEVO;
    }
    if channel.contains("official") {
        parts.channel += CHANNEL_OFFICIAL;
    }

    if let Some(wanted) = target.title.as_deref() {
        let wanted = matcher::normalize(wanted);
        let found = matcher::normalize(&title);
        if !wanted.is_empty() && !found.is_empty() {
            parts.relevance = matcher::sim(&wanted, &found) as f64 * RELEVANCE_MAX;
        }
    }

    parts.media = match candidate.media_rank() {
        3 => MEDIA_AUDIO,
        2 => MEDIA_LYRIC,
        0 => MEDIA_VIDEO,
        _ => 0.0,
    };

    let requested = matcher::normalize(&target.requested);
    for marker in PENALTY_MARKERS {
        if contains_marker(&title, marker)
            && !contains_marker(&requested, marker)
            && !parts.penalties.contains(&marker)
        {
            parts.penalties.push(marker);
        }
    }

    let views = candidate.views() as f64;
    parts.views = if views > 0.0 {
        views.log10().min(VIEWS_MAX) * VIEWS_WEIGHT
    } else {
        0.0
    };

    parts
}

/// Ranks candidates against a target, best first.
pub fn pick(candidates: &[Candidate], target: &Target, threshold: f64) -> Choice {
    let threshold = target.effective_threshold(threshold);
    let mut ranked: Vec<ScoredCandidate> = candidates
        .iter()
        .filter(|candidate| candidate.is_song_for(target))
        .map(|candidate| ScoredCandidate {
            parts: score(candidate, target),
            candidate: candidate.clone(),
        })
        .collect();
    ranked.sort_by(|a, b| b.score().total_cmp(&a.score()));

    let confident = ranked.first().is_some_and(|best| best.score() >= threshold);
    Choice {
        ranked,
        confident,
        threshold,
        // Filled in by `choose_match`, which knows the request and catalog match.
        resolved: None,
        requested: target.requested.clone(),
    }
}

/// Bound on a yt-dlp search; `Player::play` holds the guild's ordering meanwhile, so a hang blocks skip/stop.
const YOUTUBE_SEARCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Searches YouTube via `yt-dlp` and returns the raw candidates.
///
/// `--flat-playlist` keeps it to ~1 s. Not `music.youtube.com`: its flat search nulls duration, channel and views.
pub async fn youtube_candidates(query: &str, count: usize) -> anyhow::Result<Vec<Candidate>> {
    anyhow::ensure!(count > 0, "need at least one search candidate");

    // `kill_on_drop` so a timeout does not leave yt-dlp running.
    let mut command = Command::new("yt-dlp");
    command
        .kill_on_drop(true)
        .args(["-j", "--flat-playlist", "--no-warnings", "--playlist-end"])
        .arg(count.to_string())
        .arg(format!("ytsearch{count}:{query}"))
        .stdin(std::process::Stdio::null());

    let output = tokio::time::timeout(YOUTUBE_SEARCH_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "yt-dlp search for '{query}' took longer than {YOUTUBE_SEARCH_TIMEOUT:?}"
            )
        })?
        .context("failed to run yt-dlp; is it installed and on PATH?")?;

    anyhow::ensure!(
        output.status.success(),
        "yt-dlp search failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).context("failed to parse yt-dlp JSON output"))
        .collect::<Result<_, _>>()
}

/// Where YouTube candidates come from; a seam so selection is testable without yt-dlp.
#[async_trait::async_trait]
pub trait VideoSearch: Send + Sync {
    async fn candidates(&self, query: &str, count: usize) -> anyhow::Result<Vec<Candidate>>;
}

/// The real search, through yt-dlp.
pub struct YtDlpSearch;

#[async_trait::async_trait]
impl VideoSearch for YtDlpSearch {
    async fn candidates(&self, query: &str, count: usize) -> anyhow::Result<Vec<Candidate>> {
        youtube_candidates(query, count).await
    }
}

/// What a request turned into; a wrong confident answer is worse than asking.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolved {
    /// One song confirmed by the catalog, with every running time it reported.
    Track {
        track: CatalogTrack,
        durations: Vec<f64>,
    },
    /// Several songs fit and the request cannot choose; ask, in songs not videos.
    Ask(Vec<CatalogTrack>),
    /// Nothing believable. The caller plays a miss tone and says so.
    Unknown,
}

impl From<matcher::Verdict<'_, resolve::Hit>> for Resolved {
    /// A verdict with no popularity policy, for the guild's own history.
    fn from(verdict: matcher::Verdict<'_, resolve::Hit>) -> Self {
        match verdict {
            matcher::Verdict::NoMatch => Self::Unknown,
            matcher::Verdict::Confident(hit) => Self::Track {
                track: hit.to_track(),
                durations: hit.durations.clone(),
            },
            matcher::Verdict::Ambiguous(options) => {
                Self::Ask(options.iter().map(|hit| hit.to_track()).collect())
            }
        }
    }
}

impl Resolved {
    /// The one song, when there is one.
    pub fn as_track(&self) -> Option<&CatalogTrack> {
        match self {
            Self::Track { track, .. } => Some(track),
            _ => None,
        }
    }
}

/// The most-played option sharing the winner's title; popularity breaks ties, never picks another song.
fn most_played_same_title<'a>(options: &[&'a resolve::Hit]) -> Option<&'a resolve::Hit> {
    let top = *options.first()?;
    let title = matcher::normalize(&top.title);
    let mut best = top;
    for hit in options
        .iter()
        .copied()
        .filter(|hit| matcher::normalize(&hit.title) == title)
    {
        if hit.popularity.unwrap_or(0) > best.popularity.unwrap_or(0) {
            best = hit;
        }
    }
    Some(best)
}

/// Whether `winner` is at least 3x more popular than any other artist's rival (`AMBIGUOUS_POLICY=popular`).
///
/// Off by default: popularity is a guess about which one the listener meant.
pub fn popular_beats_a_rival(winner: &CatalogTrack, rivals: &[CatalogTrack]) -> bool {
    let Some(top) = winner.popularity else {
        return false;
    };
    let Some(best_rival) = rivals
        .iter()
        .filter(|rival| matcher::normalize(&rival.artist) != matcher::normalize(&winner.artist))
        .filter_map(|rival| rival.popularity)
        .max()
    else {
        // No rival popularity to compare against.
        return false;
    };
    top >= best_rival.saturating_mul(3)
}

/// Turns a spoken request into a song, a question, or nothing. Never fails.
///
/// `rewrite` is scored by the same matcher, so it cannot bypass verification.
/// The caller checks [`remembered`] first.
pub async fn resolve_request(
    request: &PlayRequest,
    catalog: &resolve::Catalog,
    rewrite: Option<&str>,
    library: Option<&library::Library>,
    guild_id: serenity::model::id::GuildId,
    timing: &crate::timing::Timing,
) -> Resolved {
    let heard = request.query.clone();

    // The target carries every reading of the phrase for the query plan.
    let mut hits = timing
        .stage(
            "catalog.hits",
            catalog.hits(&resolve::Target::from_request(request)),
        )
        .await;

    // Guild history joins the catalog hits: it can win, but must still explain the request.
    if let Some(library) = library {
        let remembered = library.history_hits(guild_id.get());
        let confident = matches!(
            matcher::decide(&heard, &remembered, |hit: &resolve::Hit| {
                (hit.artist.as_str(), hit.title.as_str())
            }),
            matcher::Verdict::Confident(_)
        );
        if confident {
            tracing::info!(said = %heard, "the play history alone resolved this request");
            return matcher::decide(&heard, &remembered, |hit: &resolve::Hit| {
                (hit.artist.as_str(), hit.title.as_str())
            })
            .into();
        }
        hits.extend(remembered);
    }

    let verdict = matcher::decide(&heard, &hits, |hit: &resolve::Hit| {
        (hit.artist.as_str(), hit.title.as_str())
    });
    if let Resolved::Track { .. } = decide(&verdict)
        && let Some((rewritten, rewritten_hits)) = try_rewrite(catalog, rewrite, timing).await
    {
        let alternative = matcher::decide(&rewritten, &rewritten_hits, |hit: &resolve::Hit| {
            (hit.artist.as_str(), hit.title.as_str())
        });
        // Only a confident rewrite displaces the heard words, and only if they matched nothing.
        if matches!(verdict, matcher::Verdict::NoMatch)
            && matches!(alternative, matcher::Verdict::Confident(_))
        {
            tracing::info!(
                heard = %heard,
                rewritten = %rewritten,
                "the transcript rewrite resolved the request and the words as heard did not"
            );
            return decide(&alternative);
        }
    }
    decide(&verdict)
}

/// The song this exact phrase was taught to mean; skips the catalog lookup.
pub fn remembered(library: &library::Library, heard: &str) -> Option<Resolved> {
    let track = library.recall(heard)?;
    tracing::info!(said = %heard, track = %track.label(), "recalled from the song library");
    Some(Resolved::Track {
        track: track.to_catalog_track(),
        durations: track.duration_secs.into_iter().collect(),
    })
}

/// Runs the rewrite as a second request and returns its hits.
async fn try_rewrite(
    catalog: &resolve::Catalog,
    rewrite: Option<&str>,
    timing: &crate::timing::Timing,
) -> Option<(String, Vec<resolve::Hit>)> {
    let rewritten = rewrite
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())?;
    let alternative = PlayRequest {
        query: rewritten.to_lowercase(),
        title: None,
        artist: None,
        splits: crate::parser::readings(rewritten)
            .into_iter()
            .filter_map(|(title, artist)| Some((title?, artist?)))
            .collect(),
    };
    let hits = timing
        .stage(
            "catalog.hits_rewrite",
            catalog.hits(&resolve::Target::from_request(&alternative)),
        )
        .await;
    Some((rewritten.to_string(), hits))
}

/// Turns a matcher verdict into what the bot will do.
fn decide(verdict: &matcher::Verdict<'_, resolve::Hit>) -> Resolved {
    match verdict {
        matcher::Verdict::NoMatch => Resolved::Unknown,
        matcher::Verdict::Confident(hit) => Resolved::Track {
            track: hit.to_track(),
            durations: hit.durations.clone(),
        },
        matcher::Verdict::Ambiguous(options) => {
            let tracks: Vec<CatalogTrack> = options.iter().map(|hit| hit.to_track()).collect();
            match options.first() {
                Some(top)
                    if crate::config::ambiguous_policy()
                        == crate::config::AmbiguousPolicy::Popular
                        && popular_beats_a_rival(&top.to_track(), &tracks) =>
                {
                    tracing::info!(
                        artist = %top.artist,
                        title = %top.title,
                        "AMBIGUOUS_POLICY=popular settled a shared title by popularity"
                    );
                    Resolved::Track {
                        track: top.to_track(),
                        durations: top.durations.clone(),
                    }
                }
                Some(top)
                    if crate::config::ambiguous_policy()
                        == crate::config::AmbiguousPolicy::Auto =>
                {
                    let picked = most_played_same_title(options).unwrap_or(top);
                    tracing::info!(
                        artist = %picked.artist,
                        title = %picked.title,
                        plays = ?picked.popularity,
                        "AMBIGUOUS_POLICY=auto played the same title with the most plays"
                    );
                    Resolved::Track {
                        track: picked.to_track(),
                        durations: picked.durations.clone(),
                    }
                }
                _ => Resolved::Ask(tracks),
            }
        }
    }
}

/// Finds the upload to play for a catalog track, and ranks the alternatives.
///
/// Confident only if the best upload clears the threshold: the catalog proved the song, not the recording.
pub async fn find_video(
    resolved: &Resolved,
    requested: &str,
    searcher: &dyn VideoSearch,
    candidate_count: usize,
    threshold: f64,
    timing: &crate::timing::Timing,
) -> anyhow::Result<Choice> {
    let Resolved::Track { track, durations } = resolved else {
        anyhow::bail!("find_video needs a resolved song, not a question or nothing");
    };
    // The provider already named the upload, so play it as-is.
    if let Some(video_id) = track.video_id.as_deref().filter(|id| !id.is_empty()) {
        return Ok(Choice {
            ranked: vec![ScoredCandidate {
                candidate: Candidate::for_track(track, video_id),
                parts: ScoreParts::default(),
            }],
            confident: true,
            threshold,
            resolved: Some(track.clone()),
            requested: requested.to_string(),
        });
    }
    let target = Target::from_track(track, durations.clone(), requested);
    let query = target.search_query();
    let candidates = timing
        .stage(
            "youtube_candidates",
            searcher.candidates(&query, candidate_count),
        )
        .await
        .with_context(|| format!("could not search YouTube for '{query}'"))?;
    anyhow::ensure!(
        !candidates.is_empty(),
        "YouTube returned no results for '{query}'"
    );

    let mut choice = pick(&candidates, &target, threshold);
    choice.requested = requested.to_string();
    choice.resolved = Some(track.clone());
    log_choice(&choice, &query);
    Ok(choice)
}

/// Finds uploads for a request no catalog track answers; never autoplayed, as nothing verifies it.
pub async fn find_unverified(
    request: &PlayRequest,
    searcher: &dyn VideoSearch,
    candidate_count: usize,
    threshold: f64,
    timing: &crate::timing::Timing,
) -> anyhow::Result<Choice> {
    let target = Target::from_request(request);
    let query = target.search_query();
    let candidates = timing
        .stage(
            "youtube_candidates",
            searcher.candidates(&query, candidate_count),
        )
        .await
        .with_context(|| format!("could not search YouTube for '{query}'"))?;
    anyhow::ensure!(
        !candidates.is_empty(),
        "YouTube returned no results for '{query}'"
    );

    let mut choice = pick(&candidates, &target, threshold);
    choice.requested = request.query.clone();
    // Enforced here: unverified results are never autoplayed.
    choice.confident = false;
    log_choice(&choice, &query);
    Ok(choice)
}

/// The one place a ranked choice is described, so every path logs the same way.
fn log_choice(choice: &Choice, query: &str) {
    let Some(best) = choice.best() else {
        return;
    };
    tracing::debug!(
        searched = %query,
        picked = best.candidate.title(),
        channel = best.candidate.channel(),
        chosen = %best.parts.explain(),
        runner_up = choice.ranked.get(1).map(|scored| format!(
            "{} ({:.1})",
            scored.candidate.title(),
            scored.score()
        )),
        threshold = choice.threshold,
        confident = choice.confident,
        "chose a video"
    );
}

/// Reads the video id out of a YouTube URL (bare id, `watch?v=`, `youtu.be/`, `/shorts/`).
pub fn url_video_id(url: &str) -> Option<&str> {
    const PREFIXES: [&str; 4] = [
        "https://www.youtube.com/watch?v=",
        "https://youtube.com/watch?v=",
        "https://youtu.be/",
        "https://www.youtube.com/shorts/",
    ];
    for prefix in PREFIXES {
        if let Some(rest) = url.strip_prefix(prefix) {
            let id = rest.split(['&', '?']).next().unwrap_or(rest);
            if !id.is_empty() {
                return Some(id);
            }
        }
    }
    // A bare id, which is what a flat playlist search reports.
    if !url.is_empty()
        && url
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Some(url);
    }
    None
}

#[cfg(test)]
#[path = "tests/search.rs"]
mod tests;
