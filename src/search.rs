//! Chooses which YouTube video to play for a spoken request.
//!
//! A YouTube search returns whatever matched the words: clips, live
//! performances, lyric videos, 10-minute mixes. It also returns a *different
//! song* with more views often enough to be the actual complaint, because a flat
//! search has no way to tell "the studio recording" from "a remix of it" and
//! popularity is all it has left to sort on.
//!
//! So the search is aimed at a known song rather than at a phrase. The
//! transcript is first resolved against a music catalog (see
//! [`crate::resolve`]), which supplies the song's canonical artist, title and
//! running time. Those names are what the YouTube query is built from, and that
//! running time is the strongest signal available when ranking the results:
//!
//! > Catalog: *Dracula* by Tame Impala, 205 s.
//!
//! | result | length | views | verdict |
//! | --- | --- | --- | --- |
//! | Official Video | 234 s | 75.5 M | wrong cut of the right song |
//! | JENNIE Remix (Lyrics) | 210 s | 50.8 M | a different song entirely |
//! | **Dracula** (artist channel) | **206 s** | 2.8 M | **the one asked for** |
//!
//! Ranking by views picks the 75 M video. Ranking by length alone picks the
//! right one, and it does so from a number no uploader can misreport.

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
/// Title fragments that mark a result as "not the song itself".
///
/// "lyric" is deliberately absent. A lyric video carries the same recording as
/// the official audio, and for less common songs it is sometimes the only
/// upload at all, so rejecting it loses the song the listener asked for.
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

/// Title fragments that mean the upload is a video rather than just the audio.
///
/// The listener asked for a song, and an MV is a video first: it carries
/// choreography, a title card, and edits that get in the way of the music.
/// These are penalised rather than rejected, because for many songs the
/// official MV is the best available recording of it.
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

/// Title fragments that mean the upload is the audio on its own, which is what
/// the listener asked for.
const AUDIO_MARKERS: [&str; 4] = ["official audio", "audio only", "audio", "official track"];

/// Title fragments that mark a lyric video: video, but a still image over the
/// song rather than anything that edits the music.
const LYRIC_MARKERS: [&str; 2] = ["lyric video", "lyrics"];

/// How far a candidate's length may be from the catalog's and still count as
/// the same recording.
///
/// Four seconds absorbs the difference between an album track, an edited
/// single and a video that runs a few seconds of silence at either end, which
/// is normal and is not a different song.
pub const DURATION_TOLERANCE_SECS: f64 = 4.0;

/// Words in a title that mean this is not the recording that was asked for.
///
/// Each costs [`PENALTY`], unless the request itself used the word: someone who
/// asked for the live version wants the live version, and penalising it would
/// make the bot reliably play the studio cut instead.
///
/// Two matching rules keep the list honest. "mix" is matched as a whole word, so
/// it does not also fire on "remix", which is listed separately and is a
/// different mistake. "lyric" covers "lyrics" by prefix, so only one spelling is
/// listed and a title reading "Lyrics [Lyric Video]" is charged once rather than
/// twice for one fault.
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

/// Score awarded for a length within [`DURATION_TOLERANCE_SECS`] of the
/// catalog's.
///
/// Large enough to outweigh everything else combined: it is the only signal
/// that distinguishes the studio recording from every other version of the same
/// title.
const DURATION_EXACT: f64 = 100.0;
/// Charged per second outside the tolerance.
const DURATION_PENALTY_PER_SEC: f64 = 3.0;
/// Ceiling on that penalty, so a 10-minute mix cannot dominate the arithmetic.
const DURATION_PENALTY_MAX: f64 = 90.0;

/// A "- Topic" channel is YouTube's own auto-generated artist channel, which is
/// the most reliable provenance signal on the platform.
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
/// An official MV, visualizer or dance video: the wrong shape for a request to
/// hear a song.
const MEDIA_VIDEO: f64 = -6.0;
/// Charged per penalty marker.
const PENALTY: f64 = 30.0;
/// Weight on popularity. Logarithmic and small on purpose: views break ties
/// between equally good uploads, they do not choose the song.
const VIEWS_WEIGHT: f64 = 1.0;
/// Ceiling on the popularity term, so 75 M views cannot outvote a length
/// mismatch.
const VIEWS_MAX: f64 = 12.0;

/// One entry of a YouTube search result.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Candidate {
    /// Video title as shown on YouTube.
    pub title: Option<String>,
    /// Canonical watch URL.
    ///
    /// `url` and not `webpage_url`, and deliberately not an alias for either:
    /// a flat listing carries *both* keys, so an alias makes serde report a
    /// duplicate field and fail every result.
    pub url: String,
    /// View count, when YouTube reports one.
    pub view_count: Option<u64>,
    /// Duration in seconds, when known.
    pub duration: Option<f64>,
    /// Uploading channel, used as the artist hint in logs.
    pub channel: Option<String>,
    /// Bare video id, which the dashboard needs to build a thumbnail URL.
    ///
    /// Not every yt-dlp version reports it, so it may be absent even when the
    /// id is recoverable from `url`.
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

    /// The video id, used to build the dashboard thumbnail URL.
    ///
    /// Prefers the reported id and falls back to reading it out of the URL,
    /// which yt-dlp writes in a few shapes depending on version and mode.
    pub fn video_id(&self) -> Option<&str> {
        if let Some(id) = self.id.as_deref().filter(|id| !id.is_empty()) {
            return Some(id);
        }
        url_video_id(&self.url)
    }

    /// The one upload a track that already knows its video stands for.
    ///
    /// Not a search result and never ranked against others: the provider named
    /// this video as the song, so there is nothing to choose between and the
    /// search is a second opinion nobody asked for. The catalog's own names
    /// stand in for the title and channel, because they are what it said the
    /// video is.
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

    /// Whether the result looks like a song rather than a clip, cover, or mix.
    /// Whether this is worth ranking against `target`.
    ///
    /// Length is judged unconditionally: a ten-second clip and a six-hour
    /// upload are not songs under any request. The disqualifying markers are
    /// judged against the request, for the same reason the penalties are --
    /// filtering every live recording out of the results would mean that asking
    /// for the live version plays the studio cut, which is the failure this
    /// ranking exists to prevent.
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

    /// How close this upload is to being the song's audio on its own.
    ///
    /// Read from the title, because that is all a flat search reports: yt-dlp
    /// does not fetch stream details until asked, and asking per candidate
    /// costs a round trip on every search. It is not exact -- an "Official
    /// Audio" upload still reports a video codec, because YouTube wraps a
    /// still image in one -- so this ranks the title's claim rather than
    /// checking the file.
    ///
    /// Ordered so that a request for a song lands on its audio:
    ///
    /// - 3 the audio on its own, which is what was asked for
    /// - 2 a lyric video: a static image over the same recording
    /// - 1 an ordinary upload, which may be either
    /// - 0 an official MV, visualizer, or dance video, which is video first
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

/// A form with spacing and punctuation removed, for containment tests.
///
/// An artist channel is as likely to be `TameImpala` as `Tame Impala`, and
/// neither is something a band chose carefully, so neither can be relied on.
fn squashed(text: &str) -> String {
    matcher::normalize(text).replace(' ', "")
}

/// Whether a title or request contains `needle`.
///
/// Multi-word needles match as a substring, because "sped up" and "1 hour" are
/// phrases. Single words match whole words only, so "mix" does not fire on
/// "remix" -- a different mistake, penalised separately -- and "live" does not
/// fire on "delivery". A longer word also matches its own extensions, so
/// "lyric" catches "lyrics" and "remix" catches "remixes".
fn contains_marker(haystack: &str, needle: &str) -> bool {
    if needle.contains(' ') {
        return haystack.contains(needle);
    }
    haystack
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word == needle || (needle.len() > 4 && word.starts_with(needle)))
}

/// A reference recording, as far as it is known, to score uploads against.
///
/// Built from a catalog match when there is one, and from the raw transcript
/// when there is not. The two cases score differently because a request with no
/// catalog match has no length to compare, which removes the strongest signal
/// and has to be reflected in the threshold rather than silently ignored.
#[derive(Debug, Clone)]
pub struct Target {
    pub artist: Option<String>,
    pub title: Option<String>,
    /// Every running time the catalog reported for this song.
    ///
    /// A set rather than one number, because providers disagree: one counts the
    /// outro and one does not, and an upload is the right recording if it matches
    /// *any* of them. The smallest gap to any of them is the score, so a
    /// straggling provider cannot drag the comparison away from the recording
    /// the others agree on.
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

    /// A target from the transcript alone, used when no catalog match cleared
    /// the bar.
    ///
    /// The request doubles as the title, because the parser only reports a
    /// title when it heard "by" and most requests do not contain one. Without
    /// this the title term would be zero for those requests, leaving relevance
    /// -- the only signal left once there is no catalog length -- doing nothing.
    pub fn from_request(request: &PlayRequest) -> Self {
        Self {
            artist: request.artist.clone(),
            title: request.title.clone().or_else(|| non_empty(&request.query)),
            durations: Vec::new(),
            requested: request.query.clone(),
            catalogued: false,
        }
    }

    /// What to search YouTube for.
    ///
    /// Prefers the canonical `"<artist> <title>"`, which is how the song is
    /// actually titled on YouTube, and falls back to the transcript otherwise.
    pub fn search_query(&self) -> String {
        match (&self.artist, &self.title) {
            (Some(artist), Some(title)) => format!("{artist} {title}"),
            (None, Some(title)) => title.clone(),
            _ => self.requested.clone(),
        }
    }

    /// The threshold that applies to this target.
    ///
    /// See [`UNRESOLVED_MATCH_THRESHOLD`]: the two cases are scored on
    /// different scales, so one threshold cannot serve both.
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
/// A separate constant rather than a fraction of
/// [`crate::config::DEFAULT_MATCH_THRESHOLD`], because the two cases are not
/// the same scale: with a catalog match a candidate can earn about 190 points,
/// and without one the most it can earn is the 125 that do not depend on
/// running time. A fraction would move this bar every time the weights in
/// [`score`] were retuned, silently.
///
/// It decides the *order* the chooser offers its options in, and nothing else.
/// With no catalog match there is no verified running time, so the strongest
/// signal is gone and the score cannot be trusted to identify a song at all:
/// `relevance` alone is 18.0 for any upload title sharing one word with the
/// transcript, which is why this bar used to admit almost anything. An
/// unverified best result is therefore never played on its own.
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
    /// The total, with one penalty per distinct marker.
    ///
    /// Markers are de-duplicated before charging so a title reading
    /// "Lyrics (Lyric Video)" is not billed twice for the same fault.
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
    /// The catalog match the query was built from, when there was one.
    ///
    /// Kept so the caller can say what it thinks was asked for, which is the
    /// difference between "picked the wrong song" and "I could not tell which
    /// song you meant".
    pub resolved: Option<resolve::CatalogTrack>,
    /// What the request was understood as.
    pub requested: String,
}

impl Choice {
    pub fn best(&self) -> Option<&ScoredCandidate> {
        self.ranked.first()
    }

    /// The best `count` results, for the chooser.
    ///
    /// Includes the top result: when the bot is not sure enough to play, the
    /// most likely answer is still the first button.
    pub fn top(&self, count: usize) -> Vec<&ScoredCandidate> {
        self.ranked.iter().take(count).collect()
    }
}

/// Scores one candidate against a target.
///
/// The terms are deliberately lopsided. Length is worth more than everything
/// else combined because it is the only signal that survives a correct title in
/// a wrong place, and popularity is worth least because it is what made the old
/// ranking wrong.
pub fn score(candidate: &Candidate, target: &Target) -> ScoreParts {
    let mut parts = ScoreParts::default();
    let title = candidate.title_lowercase();

    if let (Some(found), true) = (candidate.duration, !target.durations.is_empty()) {
        // The closest any provider came, so one that counts the outro does not
        // drag the comparison away from the recording the others agree on.
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
        // Filled in by `choose_match`, which is the only place the request and
        // the catalog match are known.
        resolved: None,
        requested: target.requested.clone(),
    }
}

/// How long a yt-dlp search may take before it is treated as having failed.
///
/// Generous: this is metadata for a handful of results and normally returns in
/// about a second. It has to be bounded regardless, because `Player::play`
/// holds the guild's request ordering while it runs, and a yt-dlp that hangs on
/// a stalled connection would leave that guild unable to skip or stop.
const YOUTUBE_SEARCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Searches YouTube via `yt-dlp` and returns the raw candidates.
///
/// Metadata only: `--flat-playlist` reads the search page without opening each
/// video, which is what keeps a spoken command's latency at about a second
/// instead of half a minute.
///
/// Deliberately not `music.youtube.com`. Its flat search does return cleaner
/// titles, but it reports `channel`, `duration` and `view_count` as null, which
/// is exactly the data this module ranks on; recovering those means dropping
/// `--flat-playlist`, at roughly 5 s per result.
pub async fn youtube_candidates(query: &str, count: usize) -> anyhow::Result<Vec<Candidate>> {
    anyhow::ensure!(count > 0, "need at least one search candidate");

    // `kill_on_drop` matters as much as the timeout: without it, dropping the
    // future on timeout would leave the yt-dlp process running, holding the
    // network and eventually answering a socket nobody is reading.
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

/// Where the YouTube candidates come from.
///
/// The seam exists so the selection path can be exercised without yt-dlp, a
/// network, or a real search result: [`choose_match`] is the only consumer, and
/// the behaviour worth testing there is what happens when the catalog says
/// nothing, which cannot be arranged by talking to YouTube.
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

/// What a request turned into.
///
/// The catalog answers this, not the video search. The principle is that a
/// wrong confident answer is worse than asking: the listener hears a song
/// nobody asked for and has no way to tell it from a right one.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolved {
    /// One song, confirmed by the catalog.
    ///
    /// Carries every running time the catalog reported, because the upload
    /// search is ranked against the whole set: providers disagree, and an upload
    /// is the right recording if it matches any of them.
    Track {
        track: CatalogTrack,
        durations: Vec<f64>,
    },
    /// More than one song answers to what was said, and the request did not say
    /// enough to choose between them. Ask, in songs rather than in videos.
    Ask(Vec<CatalogTrack>),
    /// Nothing believable. The caller plays a miss tone and says so.
    Unknown,
}

impl From<matcher::Verdict<'_, resolve::Hit>> for Resolved {
    /// A verdict with no popularity policy applied. Used for the library's own
    /// history, where the songs are the guild's own and there is no ambiguity to
    /// settle by popularity.
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

/// The most-played recording among the options sharing the winner's title.
///
/// `auto` still settles only the question `decide` called ambiguous: one
/// title, several artists. Popularity may break that tie; it never makes a
/// different song win, and with no counts it leaves the matcher's order alone.
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

/// What the bot is going to ask, when it is going to ask.
///
/// Default `ask`. With `popular`, a title two artists have recorded is settled
/// by how much bigger one of them is, which is the behaviour the README
/// described before the catalog could tell two recordings of one title apart.
/// It is off by default because it is a guess: popularity is a fact about
/// YouTube and not about which of them the listener meant.
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
        // No rival has a popularity, so there is nothing to compare against and
        // nothing to be three times bigger than.
        return false;
    };
    top >= best_rival.saturating_mul(3)
}

/// Turns a spoken request into a song, or into a question, or into nothing.
///
/// Never fails. Every provider failing is the same outcome as no provider
/// finding anything: [`Resolved::Unknown`], which the caller reports rather
/// than playing whatever YouTube ranked first.
///
/// `rewrite` is an optional second transcript from the transcript-rewriting
/// stage. It is tried alongside the words that were heard and the better verdict
/// wins, but the rewrite can only ever *propose* a transcript: it is scored
/// against the same catalog hits by the same matcher, so it cannot bypass
/// verification.
///
/// An exact phrase the song memory knows is not looked up here: the caller
/// asks [`remembered`] first, because a remembered song is shown as one.
/// `library` only lends this the guild's play history.
pub async fn resolve_request(
    request: &PlayRequest,
    catalog: &resolve::Catalog,
    rewrite: Option<&str>,
    library: Option<&library::Library>,
    guild_id: serenity::model::id::GuildId,
    timing: &crate::timing::Timing,
) -> Resolved {
    let heard = request.query.clone();

    // The target carries every reading of the phrase, which is what the query
    // plan is built from; the caller keeps the request for its own text.
    let mut hits = timing
        .stage(
            "catalog.hits",
            catalog.hits(&resolve::Target::from_request(request)),
        )
        .await;

    // Songs this guild has played before are candidates too, judged by the same
    // matcher as the catalog's own. They are added *after* the catalog so a
    // remembered song can win, but it cannot make one up: it still has to
    // explain the request.
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
        // Only a *confident* answer from the rewrite displaces the words as
        // heard. If the heard transcript also resolved, that is evidence for
        // it, and the rewrite is the speculative one.
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

/// The song this exact phrase was taught to mean, as a resolved song.
///
/// An alias hit skips the catalog entirely: the same misheard phrase cost the
/// same lookup every time it was heard, and this is what stops it.
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
/// A confident catalog track makes the result confident only if the best upload
/// clears [`crate::config::DEFAULT_MATCH_THRESHOLD`]. That is deliberately
/// stricter than the catalog verdict: the catalog proved *which song*, and this
/// still has to find *which recording of it*. If nothing clears the bar the
/// caller offers the closest uploads of that song, so the user is choosing
/// between recordings of the right thing rather than between different songs.
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
    // The provider already named the upload. Searching again could only pick a
    // different one, so the catalog's video is played as-is.
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

/// Finds the uploads for a request that no catalog track answers to.
///
/// Only ever called when the operator has asked for it. Nothing found this way
/// is ever played on its own: without a catalog there is no verified running
/// time, so the scorer has a title similarity and a view count and nothing
/// else, which is how the wrong song gets played with no feedback.
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
    // Said here rather than left to the caller, because the rule is absolute:
    // these results were never verified against a catalog and are never
    // autoplayed, whatever the caller forgets to do about it.
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

/// Reads the video id out of a YouTube URL.
///
/// Handles the bare id that `--flat-playlist` search emits, the `watch?v=`
/// form, and the `youtu.be/` and `/shorts/` paths, so a thumbnail can be built
/// whichever shape a given yt-dlp version produces.
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
