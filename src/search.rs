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
/// unverified best result is therefore never played on its own unless
/// [`crate::config::Config::allow_unverified_autoplay`] says otherwise.
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
    Unknown {
        /// What was heard, echoed back so the listener can say it better.
        heard: String,
    },
}

impl std::fmt::Display for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Track { track, .. } => write!(f, "{}", track.label()),
            Self::Ask(tracks) => write!(f, "{} songs to choose from", tracks.len()),
            Self::Unknown { .. } => f.write_str("nothing"),
        }
    }
}

impl Resolved {}

impl From<matcher::Verdict<'_, resolve::Hit>> for Resolved {
    /// A verdict with no popularity policy applied. Used for the library's own
    /// history, where the songs are the guild's own and there is no ambiguity to
    /// settle by popularity.
    fn from(verdict: matcher::Verdict<'_, resolve::Hit>) -> Self {
        match verdict {
            matcher::Verdict::NoMatch => Self::Unknown {
                heard: String::new(),
            },
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
pub async fn resolve_request(
    request: &PlayRequest,
    catalog: &resolve::Catalog,
    rewrite: Option<&str>,
    library: Option<&library::Library>,
    guild_id: serenity::model::id::GuildId,
) -> Resolved {
    let heard = request.query.clone();

    // An exact alias hit skips the catalog entirely: the same misheard phrase
    // cost the same lookup every time it was heard, and this is what stops it.
    if let Some(track) = library.and_then(|library| library.recall(&heard)) {
        tracing::info!(said = %heard, track = %track.label(), "recalled from the song library");
        return Resolved::Track {
            track: track.to_catalog_track(),
            durations: track.duration_secs.into_iter().collect(),
        };
    }

    // The target carries every reading of the phrase, which is what the query
    // plan is built from; the caller keeps the request for its own text.
    let mut hits = catalog.hits(&resolve::Target::from_request(request)).await;

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
        && let Some((rewritten, rewritten_hits)) = try_rewrite(catalog, rewrite).await
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

/// Runs the rewrite as a second request and returns its hits.
async fn try_rewrite(
    catalog: &resolve::Catalog,
    rewrite: Option<&str>,
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
    let hits = catalog
        .hits(&resolve::Target::from_request(&alternative))
        .await;
    Some((rewritten.to_string(), hits))
}

/// Turns a matcher verdict into what the bot will do.
fn decide(verdict: &matcher::Verdict<'_, resolve::Hit>) -> Resolved {
    match verdict {
        matcher::Verdict::NoMatch => Resolved::Unknown {
            // The caller has the request in hand and echoes its own text back;
            // carrying it here as well would be a second copy to keep in step.
            heard: String::new(),
        },
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
) -> anyhow::Result<Choice> {
    let Resolved::Track { track, durations } = resolved else {
        anyhow::bail!("find_video needs a resolved song, not a question or nothing");
    };
    let target = Target::from_track(track, durations.clone(), requested);
    let query = target.search_query();
    let candidates = searcher
        .candidates(&query, candidate_count)
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
) -> anyhow::Result<Choice> {
    let target = Target::from_request(request);
    let query = target.search_query();
    let candidates = searcher
        .candidates(&query, candidate_count)
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
fn url_video_id(url: &str) -> Option<&str> {
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
mod tests {
    use super::*;
    use crate::config::DEFAULT_MATCH_THRESHOLD;

    fn candidate(title: &str, views: Option<u64>, duration: Option<f64>) -> Candidate {
        Candidate {
            title: Some(title.to_string()),
            url: format!("https://youtu.be/{title}"),
            view_count: views,
            duration,
            channel: Some("someone".to_string()),
            id: None,
        }
    }

    fn on_channel(mut entry: Candidate, channel: &str) -> Candidate {
        entry.channel = Some(channel.to_string());
        entry
    }

    /// The catalog said *Dracula* by Tame Impala, 205 s.
    fn catalogued(requested: &str) -> Target {
        Target {
            artist: Some("Tame Impala".to_string()),
            title: Some("Dracula".to_string()),
            durations: vec![205.0],
            requested: requested.to_string(),
            catalogued: true,
        }
    }

    /// The title of whichever candidate wins, for readable assertions.
    fn best_title(candidates: &[Candidate], target: &Target) -> String {
        pick(candidates, target, DEFAULT_MATCH_THRESHOLD)
            .best()
            .expect("at least one candidate should survive")
            .candidate
            .title()
            .to_string()
    }

    /// The catalog's names are what the YouTube query is built from. Getting
    /// them right is the whole reason the catalog is called at all: a
    /// misheard "hoolig" would otherwise be searched as those letters and come
    /// back as whatever matched them.
    /// A flat yt-dlp listing carries both `url` and `webpage_url`. Reading
    /// either one as an alias for the other makes serde see a duplicate field
    /// and fail every result, which showed up as "failed to parse yt-dlp JSON
    /// output" on every search.
    #[test]
    fn a_flat_result_with_both_url_spellings_still_parses() {
        let line = r#"{
            "url": "https://www.youtube.com/watch?v=I0FKqmoK56I",
            "webpage_url": "https://www.youtube.com/watch?v=I0FKqmoK56I",
            "title": "Look At Me!",
            "channel": "XXXTENTACION",
            "duration": 126
        }"#;
        let candidate: Candidate = serde_json::from_str(line).expect("a flat result parses");
        assert_eq!(candidate.video_id(), Some("I0FKqmoK56I"));
        assert_eq!(candidate.duration, Some(126.0));
    }

    #[test]
    fn the_query_uses_the_catalog_names() {
        let track = resolve::CatalogTrack {
            artist: "BTS".to_string(),
            title: "Hooligan".to_string(),
            duration_secs: Some(182.0),
            id: "1".to_string(),
            provider: "catalog",
            popularity: None,
        };
        let target = Target::from_track(&track, vec![182.0], "hoolig by bts");
        assert_eq!(target.search_query(), "BTS Hooligan");
    }

    /// The bug the whole module exists for, as a live search saw it.
    ///
    /// Every result names the requested song and is roughly its length. The
    /// 75-million-view video is a different cut, and ranking by views -- what
    /// the old selector did -- picks it. Only the running time separates them,
    /// and only the penalty separates the two 206-second uploads.
    #[test]
    fn a_remix_with_twenty_times_the_views_still_loses_to_the_right_recording() {
        let entries = [
            candidate(
                "Tame Impala - Dracula (Official Video)",
                Some(75_531_372),
                Some(234.0),
            ),
            candidate(
                "Tame Impala - Dracula (Lyrics)",
                Some(24_438_628),
                Some(206.0),
            ),
            candidate(
                "Tame Impala and JENNIE - Dracula (JENNIE Remix - Official Lyric Video)",
                Some(50_831_672),
                Some(211.0),
            ),
            candidate("Tame Impala - Dracula", Some(2_777_206), Some(206.0)),
        ];
        assert_eq!(
            best_title(&entries, &catalogued("dracula tame impala")),
            "Tame Impala - Dracula"
        );
    }

    /// The tolerance is four seconds either way, so a video that padded the
    /// start is not treated as a different recording.
    #[test]
    fn a_length_within_tolerance_counts_as_a_match() {
        let target = catalogued("dracula");
        for duration in [201.0, 205.0, 209.0] {
            let entry = candidate("Dracula", Some(1000), Some(duration));
            assert_eq!(
                score(&entry, &target).duration,
                DURATION_EXACT,
                "{duration}s should be within tolerance"
            );
        }
    }

    #[test]
    fn a_length_outside_tolerance_is_penalised_proportionally() {
        let target = catalogued("dracula");
        // 210 s is 5 s out, so 1 s past the tolerance is charged.
        assert_eq!(
            score(&candidate("Dracula", None, Some(210.0)), &target).duration,
            -3.0
        );
        // 234 s is 29 s out, and 25 of those are past the tolerance.
        assert_eq!(
            score(&candidate("Dracula", None, Some(234.0)), &target).duration,
            -75.0
        );
    }

    /// A ten-minute mix is not a wrong answer, it is a different product, and
    /// the penalty has to be capped so it cannot swamp the arithmetic.
    #[test]
    fn the_length_penalty_is_capped() {
        let target = catalogued("dracula");
        let parts = score(&candidate("1 Hour Mix", Some(10), Some(3_600.0)), &target);
        assert_eq!(parts.duration, -DURATION_PENALTY_MAX);
    }

    /// A duration-less upload cannot be scored on length and must not be
    /// assumed wrong for it.
    #[test]
    fn an_unknown_length_is_neither_rewarded_nor_punished() {
        let target = catalogued("dracula");
        assert_eq!(
            score(&candidate("Dracula", None, None), &target).duration,
            0.0
        );
    }

    #[test]
    fn a_topic_channel_is_the_strongest_provenance_signal() {
        let target = catalogued("dracula");
        let parts = score(
            &on_channel(candidate("Dracula", None, None), "Tame Impala - Topic"),
            &target,
        );
        // A topic channel is YouTube's channel for the artist, so it earns
        // both provenance bonuses; it is the best evidence available.
        assert_eq!(parts.channel, CHANNEL_TOPIC + CHANNEL_ARTIST);
        assert!(parts.channel > CHANNEL_ARTIST);
    }

    #[test]
    fn the_artists_own_channel_is_rewarded() {
        let target = catalogued("dracula");
        let parts = score(
            &on_channel(candidate("Dracula", None, None), "TameImpala"),
            &target,
        );
        assert_eq!(parts.channel, CHANNEL_ARTIST);
    }

    #[test]
    fn a_vevo_channel_is_rewarded() {
        let target = catalogued("dracula");
        let parts = score(
            &on_channel(candidate("Dracula", None, None), "TameImpalaVEVO"),
            &target,
        );
        // A VEVO channel is the artist's channel under a label handle, so it
        // earns both provenance bonuses.
        assert_eq!(parts.channel, CHANNEL_VEVO + CHANNEL_ARTIST);
    }

    /// An unrelated channel earns nothing, which is the point: the boost is for
    /// provenance, not for having a channel at all.
    #[test]
    fn an_unrelated_channel_earns_nothing() {
        let target = catalogued("dracula");
        let parts = score(
            &on_channel(candidate("Dracula", None, None), "7clouds"),
            &target,
        );
        assert_eq!(parts.channel, 0.0);
    }

    /// Every version the spec lists, checked against the title that would carry
    /// it.
    #[test]
    fn every_version_marker_is_penalised() {
        let target = catalogued("dracula");
        for (title, expected) in [
            ("Dracula (Cover)", "cover"),
            ("Dracula (Live)", "live"),
            ("Dracula (Remix)", "remix"),
            ("Dracula (Lyrics)", "lyric"),
            ("Dracula (Lyric Video)", "lyric"),
            ("Dracula (Karaoke)", "karaoke"),
            ("Dracula (Sped Up)", "sped up"),
            ("Dracula (Slowed)", "slowed"),
            ("Dracula (Reverb)", "reverb"),
            ("Dracula (Instrumental)", "instrumental"),
            ("Dracula 1 Hour Mix", "1 hour"),
            ("Dracula Mix", "mix"),
            ("Dracula (Reaction)", "reaction"),
            ("Dracula (Tutorial)", "tutorial"),
        ] {
            let parts = score(&candidate(title, None, None), &target);
            assert!(
                parts.penalties.contains(&expected),
                "{title:?} should be penalised for {expected:?}, got {:?}",
                parts.penalties
            );
        }
    }

    /// The listener is the reason for the conditional. Asking for the live
    /// version and being given the studio cut is a worse failure than either
    /// one alone.
    #[test]
    fn a_marker_the_request_asked_for_is_not_penalised() {
        let target = catalogued("dracula live");
        for title in ["Dracula (Live)", "Dracula - Live at Glastonbury"] {
            let parts = score(&candidate(title, None, None), &target);
            assert!(
                parts.penalties.is_empty(),
                "{title:?} was asked for: {parts:?}"
            );
        }
    }

    /// "remix" contains "mix", so a naive substring test would bill the same
    /// title twice for one fault.
    #[test]
    fn mix_does_not_also_fire_on_remix() {
        let target = catalogued("dracula");
        let parts = score(&candidate("Dracula (Remix)", None, None), &target);
        assert_eq!(parts.penalties, vec!["remix"]);
    }

    /// "live" is a whole word, so a title that merely contains the letters is
    /// not mistaken for a live recording.
    #[test]
    fn markers_only_match_whole_words() {
        let target = catalogued("delivery");
        assert!(
            score(&candidate("Dracula (Delivery)", None, None), &target)
                .penalties
                .is_empty()
        );
    }

    /// One fault, one charge, however many ways the title spells it.
    #[test]
    fn a_repeated_marker_is_charged_once() {
        let target = catalogued("dracula");
        let parts = score(
            &candidate("Dracula (Lyrics) [Lyric Video]", None, None),
            &target,
        );
        assert_eq!(parts.penalties, vec!["lyric"]);
    }

    /// Views are a tie-breaker, not a ranking. 75 million views must not be
    /// worth more than being the right length.
    #[test]
    fn views_cannot_outvote_a_length_mismatch() {
        let target = catalogued("dracula");
        let viral_mismatch = score(
            &candidate("Dracula (Official Video)", Some(75_000_000), Some(240.0)),
            &target,
        );
        let quiet_match = score(&candidate("Dracula", Some(900), Some(205.0)), &target);
        assert!(quiet_match.total() > viral_mismatch.total());
    }

    #[test]
    fn views_break_ties_between_equally_good_uploads() {
        let target = catalogued("dracula");
        let popular = score(&candidate("Dracula", Some(9_000_000), Some(205.0)), &target);
        let obscure = score(&candidate("Dracula", Some(300), Some(205.0)), &target);
        assert!(popular.total() > obscure.total());
    }

    /// An upload reporting zero views is odd, not infinitely bad.
    #[test]
    fn a_zero_view_count_does_not_produce_a_negative_infinity() {
        let target = catalogued("dracula");
        let parts = score(&candidate("Dracula", Some(0), Some(205.0)), &target);
        assert!(parts.views.is_finite());
        assert_eq!(parts.views, 0.0);
    }

    /// The threshold is the whole point of the chooser: a real match plays, a
    /// near miss asks.
    #[test]
    fn a_confident_match_is_not_offered_as_a_choice() {
        let entries = [on_channel(
            candidate("Dracula", Some(2_777_206), Some(206.0)),
            "Tame Impala - Topic",
        )];
        let choice = pick(&entries, &catalogued("dracula"), DEFAULT_MATCH_THRESHOLD);
        assert!(choice.confident, "{:?}", choice.best().unwrap().parts);
    }

    /// Nothing close, nothing to play: the top three go to the user.
    #[test]
    fn a_low_score_offers_a_choice_instead_of_guessing() {
        let entries = [
            candidate("completely unrelated one", Some(1_000), Some(200.0)),
            candidate("completely unrelated two", Some(9_000), Some(200.0)),
            candidate("completely unrelated three", Some(5_000), Some(200.0)),
        ];
        let choice = pick(&entries, &catalogued("dracula"), DEFAULT_MATCH_THRESHOLD);
        assert!(!choice.confident);
        let offered: Vec<&str> = choice
            .top(3)
            .iter()
            .map(|scored| scored.candidate.title())
            .collect();
        assert_eq!(
            offered,
            vec![
                "completely unrelated two",
                "completely unrelated three",
                "completely unrelated one"
            ]
        );
    }

    /// A request the catalog could not resolve has no length to score on, so
    /// holding it to the full threshold would ask about every song forever.
    ///
    /// What it may no longer assert is that a good hit plays. This case encoded
    /// the shared-token floor that 1.2 removed: relevance used to be
    /// `similarity("hooligan", "bts 방탄소년단 hooligan") * 25`, and a pair
    /// sharing any one whole word was floored at 0.72 — so an upload whose
    /// title carries the artist name *in front of* the song scored as well as
    /// an exact title. That is the mechanism behind "hello by adele" answering
    /// with *Hello Brother*, and it is exactly what item 1.3 now covers from the
    /// other side: an unresolved request is offered as a choice, never played
    /// on its own.
    #[test]
    fn an_unresolved_request_is_judged_on_a_lower_bar_but_never_autoplayed() {
        let target = Target {
            artist: Some("BTS".to_string()),
            title: Some("Hooligan".to_string()),
            durations: Vec::new(),
            requested: "bts hoolig".to_string(),
            catalogued: false,
        };
        let entry = [on_channel(
            candidate("BTS 방탄소년단 'Hooligan'", Some(178_000), Some(182.0)),
            "BANGTANTV",
        )];
        let choice = pick(&entry, &target, DEFAULT_MATCH_THRESHOLD);
        assert!(
            !choice.confident,
            "an unverified pick is not played on its own"
        );
        assert!(
            choice.best().is_some(),
            "but it is still offered: {:?}",
            choice.best().map(|b| &b.parts)
        );
        assert!(target.effective_threshold(DEFAULT_MATCH_THRESHOLD) < DEFAULT_MATCH_THRESHOLD);
    }

    /// The list is already ordered best first, so the best result is index zero
    /// and the alternatives follow it.
    #[test]
    fn the_ranking_is_ordered_best_first() {
        let entries = [
            candidate("Dracula (Lyrics)", Some(24_000_000), Some(206.0)),
            candidate("Dracula", Some(2_777_206), Some(206.0)),
            candidate("Dracula (Official Video)", Some(75_531_372), Some(234.0)),
        ];
        let choice = pick(&entries, &catalogued("dracula"), DEFAULT_MATCH_THRESHOLD);
        let titles: Vec<&str> = choice
            .ranked
            .iter()
            .map(|scored| scored.candidate.title())
            .collect();
        assert_eq!(
            titles,
            vec!["Dracula", "Dracula (Lyrics)", "Dracula (Official Video)"]
        );
    }

    /// A remix of something nobody asked for is not a near miss, it is a
    /// different song, so it is dropped before it is scored.
    #[test]
    fn an_unrequested_remix_is_dropped_rather_than_ranked() {
        let entries = [
            candidate("Dracula (Remix)", Some(50_000_000), Some(211.0)),
            candidate("Dracula", Some(2_777_206), Some(206.0)),
        ];
        let choice = pick(&entries, &catalogued("dracula"), DEFAULT_MATCH_THRESHOLD);
        assert_eq!(choice.ranked.len(), 1);
        assert_eq!(choice.ranked[0].candidate.title(), "Dracula");
    }

    /// The query is the corrected one. Searching the transcript is what
    /// produced the wrong song in the first place.
    #[test]
    fn the_query_uses_the_canonical_names() {
        let target = catalogued("hoolig bts");
        assert_eq!(target.search_query(), "Tame Impala Dracula");
    }

    #[test]
    fn an_unresolved_request_searches_what_was_heard() {
        let target = Target {
            artist: None,
            title: None,
            durations: Vec::new(),
            requested: "the name of that one song".to_string(),
            catalogued: false,
        };
        assert_eq!(target.search_query(), "the name of that one song");
    }

    /// Search results are not curated: a ten-second clip and a six-hour upload
    /// are not songs, whatever they score.
    #[test]
    fn clips_and_long_mixes_are_excluded() {
        let entries = [
            candidate("clip", Some(100), Some(10.0)),
            candidate("long mix", Some(50), Some(4_000.0)),
        ];
        assert!(
            pick(&entries, &catalogued("dracula"), DEFAULT_MATCH_THRESHOLD)
                .best()
                .is_none()
        );
    }

    /// The pre-catalog ranking still had to cope with junk arriving from the
    /// search, so it degrades to popularity when nothing is relevant.
    #[test]
    fn an_unresolvable_request_falls_back_to_popularity() {
        let entries = [
            candidate("completely unrelated one", Some(1_000), Some(200.0)),
            candidate("completely unrelated two", Some(9_000), Some(200.0)),
        ];
        let target = Target {
            artist: None,
            title: Some("hooligan".to_string()),
            durations: Vec::new(),
            requested: "hooligan".to_string(),
            catalogued: false,
        };
        assert_eq!(best_title(&entries, &target), "completely unrelated two");
    }

    #[test]
    fn no_results_selects_nothing() {
        assert!(
            pick(&[], &catalogued("dracula"), DEFAULT_MATCH_THRESHOLD)
                .best()
                .is_none()
        );
    }

    /// A title claiming both is a video; the video markers have to win.
    #[test]
    fn media_rank_orders_audio_above_video() {
        let rank = |title: &str| candidate(title, Some(1), Some(200.0)).media_rank();
        assert_eq!(rank("Song (Official Audio)"), 3);
        assert_eq!(rank("Song (Lyrics)"), 2);
        assert_eq!(rank("Song"), 1);
        assert_eq!(rank("Song (Official MV)"), 0);
        assert_eq!(rank("Song (Visualizer)"), 0);
        assert_eq!(rank("Song (Dance Practice)"), 0);
    }

    /// A video marker wins over an audio marker in the same title, so
    /// "Official Audio [Official MV]" is still treated as a video.
    #[test]
    fn a_video_marker_beats_an_audio_marker_in_the_same_title() {
        let entry = candidate("Song (Official Audio) [Official MV]", Some(1), Some(200.0));
        assert_eq!(entry.media_rank(), 0);
    }

    /// Speech-to-text drops letters from proper nouns, so a request that is
    /// nearly a title still has to match it.
    ///
    /// This test encoded the shared-token floor item 1.2 removed, and it shows
    /// exactly why the floor had to go: relevance is a whole-string
    /// `jaro_winkler`, and an upload whose title leads with the artist
    /// ("BTS 방탄소년단 'Hooligan'") shares no *prefix* with the bare word
    /// "hooligan", so it scores zero for the title it is named after and loses
    /// to a wrong song on view count. The floor papered over that by crediting
    /// any shared word, which is the same mechanism that answered "hello by
    /// adele" with *Hello Brother*. Both cases are handled properly by
    /// `matcher::best_window`, which matches the title against a window of the
    /// spoken words and so ignores a leading artist.
    #[test]
    fn a_misspelled_request_scores_badly_against_a_title_that_leads_with_the_artist() {
        let target = Target {
            artist: Some("BTS".to_string()),
            title: Some("Hooligan".to_string()),
            durations: Vec::new(),
            requested: "bts hoolig".to_string(),
            catalogued: false,
        };
        // Not zero, but far below what the title deserves. The old floor made
        // any pair sharing one word worth 0.72 of the 25 available, which is
        // what let a wrong song clear the bar on a single shared word.
        let relevance =
            score(&candidate("BTS 방탄소년단 'Hooligan'", None, None), &target).relevance;
        assert!(
            relevance < RELEVANCE_MAX * 0.6,
            "a whole-string comparison cannot see through the leading artist: {relevance}"
        );
        // Which is why an unverified result is offered as a choice rather than
        // played: relevance is all the scorer has left without a catalog (see
        // `unverified_results_are_never_confident`).
    }

    #[test]
    fn reads_the_video_id_from_every_url_shape() {
        // --flat-playlist search reports these in different shapes depending
        // on version, and the dashboard needs the id for the thumbnail.
        for (url, expected) in [
            ("https://www.youtube.com/watch?v=09839DpTctU", "09839DpTctU"),
            ("https://youtube.com/watch?v=09839DpTctU", "09839DpTctU"),
            ("https://youtu.be/09839DpTctU", "09839DpTctU"),
            ("https://www.youtube.com/shorts/09839DpTctU", "09839DpTctU"),
            (
                "https://www.youtube.com/watch?v=09839DpTctU&t=42s",
                "09839DpTctU",
            ),
            ("09839DpTctU", "09839DpTctU"),
        ] {
            assert_eq!(url_video_id(url), Some(expected), "from {url}");
        }
    }

    #[test]
    fn an_unrecognised_url_has_no_video_id() {
        for url in ["", "https://example.com/watch", "https://vimeo.com/12345"] {
            assert_eq!(url_video_id(url), None, "from {url}");
        }
    }

    #[test]
    fn a_reported_id_beats_parsing_the_url() {
        let mut entry = candidate("song", Some(1), Some(200.0));
        entry.id = Some("reported-id".to_string());
        assert_eq!(entry.video_id(), Some("reported-id"));
    }

    #[test]
    fn the_video_id_falls_back_to_the_url() {
        let entry = candidate("09839DpTctU", Some(1), Some(200.0));
        assert_eq!(entry.video_id(), Some("09839DpTctU"));
    }

    #[test]
    fn an_empty_reported_id_falls_back_to_the_url() {
        let mut entry = candidate("09839DpTctU", Some(1), Some(200.0));
        entry.id = Some(String::new());
        assert_eq!(entry.video_id(), Some("09839DpTctU"));
    }

    // ---- the resolution contract ------------------------------------------

    /// A catalog that hands back whatever the request asked for, without any
    /// network. Keeps the catalog half of these tests out of the picture.
    struct Stub {
        hits: Vec<resolve::CatalogTrack>,
    }

    #[async_trait::async_trait]
    impl resolve::CatalogProvider for Stub {
        fn name(&self) -> &'static str {
            "stub"
        }
        fn rate(&self) -> resolve::Rate {
            resolve::Rate::new(20.0, 20.0)
        }
        async fn search(
            &self,
            _q: &resolve::CatalogQuery,
        ) -> Result<Vec<resolve::CatalogTrack>, resolve::ProviderError> {
            Ok(self.hits.clone())
        }
    }

    /// A catalog that finds nothing at all.
    struct Empty;

    #[async_trait::async_trait]
    impl resolve::CatalogProvider for Empty {
        fn name(&self) -> &'static str {
            "empty"
        }
        async fn search(
            &self,
            _q: &resolve::CatalogQuery,
        ) -> Result<Vec<resolve::CatalogTrack>, resolve::ProviderError> {
            Ok(Vec::new())
        }
    }

    fn catalog_track(artist: &str, title: &str, duration: Option<f64>) -> resolve::CatalogTrack {
        resolve::CatalogTrack {
            artist: artist.to_string(),
            title: title.to_string(),
            duration_secs: duration,
            id: "1".to_string(),
            provider: "stub",
            popularity: None,
        }
    }

    fn stub_catalog(hits: Vec<resolve::CatalogTrack>) -> resolve::Catalog {
        resolve::Catalog::with_budget(vec![Box::new(Stub { hits })], 0.72, 2_500)
    }

    /// Records the queries it is asked for and answers with a fixed list.
    struct FakeSearch {
        candidates: Vec<Candidate>,
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl FakeSearch {
        fn with(candidates: Vec<Candidate>) -> Self {
            Self {
                candidates,
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl VideoSearch for FakeSearch {
        async fn candidates(&self, query: &str, _count: usize) -> anyhow::Result<Vec<Candidate>> {
            self.seen.lock().unwrap().push(query.to_string());
            Ok(self.candidates.clone())
        }
    }

    fn request(query: &str) -> PlayRequest {
        PlayRequest {
            query: query.to_string(),
            title: None,
            artist: None,
            splits: crate::parser::readings(query)
                .into_iter()
                .filter_map(|(title, artist)| Some((title?, artist?)))
                .collect(),
        }
    }

    /// The ordinary case: a request that names a song and an artist resolves to
    /// that song, and the running time comes with it for the upload search.
    #[tokio::test]
    async fn a_named_song_resolves_to_one_track() {
        let catalog = stub_catalog(vec![catalog_track("Tame Impala", "Dracula", Some(205.0))]);
        let resolved = resolve_request(
            &request("drakula by tame impala"),
            &catalog,
            None,
            None,
            GuildId::new(1),
        )
        .await;
        let track = resolved.as_track().expect("one song");
        assert_eq!(track.artist, "Tame Impala");
        assert_eq!(track.title, "Dracula");
        assert_eq!(track.duration_secs, Some(205.0));
    }

    /// Two artists, one title, nothing said to tell them apart: the bot must
    /// ask rather than pick. This is the README's "Look at Me" case.
    #[tokio::test]
    async fn a_title_two_artists_share_is_asked_about() {
        let catalog = stub_catalog(vec![
            catalog_track("Ben E. King", "Stand By Me", Some(191.0)),
            catalog_track("Oasis", "Stand By Me", Some(254.0)),
        ]);
        match resolve_request(
            &request("stand by me"),
            &catalog,
            None,
            None,
            GuildId::new(1),
        )
        .await
        {
            Resolved::Ask(tracks) => {
                assert_eq!(tracks.len(), 2);
                assert!(tracks.iter().any(|t| t.artist == "Ben E. King"));
                assert!(tracks.iter().any(|t| t.artist == "Oasis"));
            }
            other => panic!("{other:?}"),
        }
    }

    /// Everything failing is not an error. It is the same outcome as nothing
    /// matching, and the caller reports it rather than playing something.
    #[tokio::test]
    async fn a_catalog_that_finds_nothing_is_unknown_not_an_error() {
        let catalog = resolve::Catalog::with_budget(vec![Box::new(Empty)], 0.72, 2_500);
        assert!(matches!(
            resolve_request(
                &request("drakula tame impala"),
                &catalog,
                None,
                None,
                GuildId::new(1)
            )
            .await,
            Resolved::Unknown { .. }
        ));
    }

    /// A provider that fails outright must not stop the request. Before the
    /// fan-out, `?` on a provider error aborted everything: one Deezer quota
    /// body in a HTTP 200 and the listener heard nothing at all.
    #[tokio::test]
    async fn a_failing_provider_leaves_the_request_alone() {
        struct Broken;
        #[async_trait::async_trait]
        impl resolve::CatalogProvider for Broken {
            fn name(&self) -> &'static str {
                "broken"
            }
            async fn search(
                &self,
                _q: &resolve::CatalogQuery,
            ) -> Result<Vec<resolve::CatalogTrack>, resolve::ProviderError> {
                Err(resolve::ProviderError::Unavailable)
            }
        }
        let catalog = resolve::Catalog::with_budget(vec![Box::new(Broken)], 0.72, 2_500);
        let resolved = resolve_request(
            &request("drakula tame impala"),
            &catalog,
            None,
            None,
            GuildId::new(1),
        )
        .await;
        assert!(matches!(resolved, Resolved::Unknown { .. }));
    }

    /// The rewriting stage is a second *candidate*, not a replacement: it can
    /// only ever propose a transcript, which then has to clear the same catalog
    /// verification as the words that were heard.
    #[tokio::test]
    async fn a_rewrite_can_rescue_a_request_the_words_could_not() {
        // The words as heard match nothing; the rewrite names the song.
        let catalog = stub_catalog(vec![catalog_track("Tame Impala", "Dracula", Some(205.0))]);
        let resolved = resolve_request(
            &request("drakoola tame implala"),
            &catalog,
            Some("dracula tame impala"),
            None,
            GuildId::new(1),
        )
        .await;
        assert_eq!(
            resolved.as_track().map(|t| t.title.as_str()),
            Some("Dracula")
        );
    }

    /// ...and it cannot override the words as heard when those already
    /// resolved. The heard transcript is the evidence; the rewrite is the
    /// speculation.
    #[tokio::test]
    async fn a_rewrite_does_not_override_words_that_already_resolved() {
        let catalog = stub_catalog(vec![
            catalog_track("Tame Impala", "Dracula", Some(205.0)),
            catalog_track("Queen", "Bohemian Rhapsody", Some(354.0)),
        ]);
        let resolved = resolve_request(
            &request("drakula tame impala"),
            &catalog,
            Some("bohemian rhapsody queen"),
            None,
            GuildId::new(1),
        )
        .await;
        assert_eq!(
            resolved.as_track().map(|t| t.artist.as_str()),
            Some("Tame Impala"),
            "the words as heard win when they resolve"
        );
    }

    /// A rewrite that is nonsense resolves to nothing and changes nothing.
    #[tokio::test]
    async fn a_useless_rewrite_changes_nothing() {
        let catalog = stub_catalog(vec![catalog_track("Tame Impala", "Dracula", Some(205.0))]);
        let resolved = resolve_request(
            &request("drakula tame impala"),
            &catalog,
            Some("hello there"),
            None,
            GuildId::new(1),
        )
        .await;
        assert_eq!(
            resolved.as_track().map(|t| t.title.as_str()),
            Some("Dracula")
        );
    }

    /// ---- the upload search ------------------------------------------------
    ///
    /// The catalog names the song; the upload search finds the recording. Both
    /// have to agree before anything plays.
    #[tokio::test]
    async fn a_confident_song_is_searched_for_by_its_canonical_names() {
        let resolved = Resolved::Track {
            track: catalog_track("Tame Impala", "Dracula", Some(205.0)),
            durations: vec![205.0],
        };
        let searcher = FakeSearch::with(vec![candidate(
            "Tame Impala - Dracula",
            Some(2_777_206),
            Some(206.0),
        )]);
        let choice = find_video(
            &resolved,
            "drakula tame impala",
            &searcher,
            8,
            DEFAULT_MATCH_THRESHOLD,
        )
        .await
        .expect("a result");
        assert_eq!(
            searcher.seen.lock().unwrap().as_slice(),
            ["Tame Impala Dracula"]
        );
        assert!(choice.confident);
        assert_eq!(
            choice.resolved.map(|t| t.label()),
            Some("Tame Impala — Dracula".to_string())
        );
    }

    /// Providers disagree about the length by a few seconds; an upload matching
    /// any of them is the right recording, so the closest of the set decides.
    #[tokio::test]
    async fn an_upload_matching_any_reported_length_is_the_right_recording() {
        let resolved = Resolved::Track {
            track: catalog_track("Tame Impala", "Dracula", Some(205.0)),
            durations: vec![205.0, 209.0, 241.0],
        };
        let entries = [
            candidate(
                "Tame Impala - Dracula (JENNIE Remix)",
                Some(50_000_000),
                Some(241.0),
            ),
            candidate("Tame Impala - Dracula", Some(2_777_206), Some(209.0)),
        ];
        let searcher = FakeSearch::with(entries.to_vec());
        let choice = find_video(
            &resolved,
            "drakula tame impala",
            &searcher,
            8,
            DEFAULT_MATCH_THRESHOLD,
        )
        .await
        .expect("a result");
        assert_eq!(
            choice.best().unwrap().candidate.title(),
            "Tame Impala - Dracula",
            "the shorter recording wins; 241 s is the remix"
        );
    }

    /// Nothing that clears the bar means the *song* is known but the *upload*
    /// is not, which is a far narrower question to ask.
    #[tokio::test]
    async fn no_convincing_upload_means_offering_recordings_not_songs() {
        let resolved = Resolved::Track {
            track: catalog_track("Tame Impala", "Dracula", Some(205.0)),
            durations: vec![205.0],
        };
        let entries = [
            candidate("a completely unrelated upload", Some(1_000), Some(1200.0)),
            candidate("another unrelated one", Some(9_000), Some(90.0)),
        ];
        let searcher = FakeSearch::with(entries.to_vec());
        let choice = find_video(
            &resolved,
            "drakula tame impala",
            &searcher,
            8,
            DEFAULT_MATCH_THRESHOLD,
        )
        .await
        .expect("a result");
        assert!(
            !choice.confident,
            "the song is known but this upload is not"
        );
        assert_eq!(
            choice.resolved.map(|t| t.label()),
            Some("Tame Impala — Dracula".to_string()),
            "the caller still knows which song these uploads are meant to be"
        );
    }

    /// Unverified results are never played on their own. There is no catalog
    /// behind them, so the scorer has a title similarity and a view count and
    /// nothing else, and `find_unverified` says so rather than trusting the
    /// caller to remember.
    #[tokio::test]
    async fn unverified_results_are_never_confident() {
        let entries = [on_channel(
            candidate("BTS 방탄소년단 'Hooligan'", Some(178_000), Some(182.0)),
            "BANGTANTV",
        )];
        let searcher = FakeSearch::with(entries.to_vec());
        let choice = find_unverified(&request("hoolig bts"), &searcher, 8, 0.0)
            .await
            .expect("a result");
        assert!(!choice.confident);
        assert!(choice.best().is_some(), "but it is still offered");
    }

    /// An empty result list is a failure of the search itself, not a ranking
    /// result, so it stays an error the caller reports.
    #[tokio::test]
    async fn no_results_from_the_search_is_an_error() {
        let resolved = Resolved::Track {
            track: catalog_track("Tame Impala", "Dracula", Some(205.0)),
            durations: vec![205.0],
        };
        let searcher = FakeSearch::with(Vec::new());
        let err = find_video(&resolved, "drakula", &searcher, 8, DEFAULT_MATCH_THRESHOLD)
            .await
            .expect_err("an empty search is a failure")
            .to_string();
        assert!(err.contains("no results"), "unhelpful: {err}");
    }

    /// A search that fails is reported with the query in the message, because
    /// the query is what has to be logged when it happens.
    #[tokio::test]
    async fn a_failing_search_names_the_query() {
        struct Broken;
        #[async_trait::async_trait]
        impl VideoSearch for Broken {
            async fn candidates(&self, _query: &str, _n: usize) -> anyhow::Result<Vec<Candidate>> {
                anyhow::bail!("yt-dlp is not installed")
            }
        }
        let resolved = Resolved::Track {
            track: catalog_track("BTS", "Hooligan", Some(182.0)),
            durations: vec![182.0],
        };
        let err = find_video(&resolved, "hoolig bts", &Broken, 8, DEFAULT_MATCH_THRESHOLD)
            .await
            .expect_err("a failed search is a failure")
            .to_string();
        assert_eq!(err, "could not search YouTube for 'BTS Hooligan'");
    }

    /// `find_video` needs a song. Asking it to find an upload of a *question*
    /// would silently pick one of the offered songs for the user.
    #[tokio::test]
    async fn a_question_cannot_be_turned_into_a_video() {
        let asked = Resolved::Ask(vec![catalog_track("Oasis", "Stand By Me", Some(254.0))]);
        let searcher = FakeSearch::with(vec![candidate("Stand By Me", Some(1), Some(254.0))]);
        let err = find_video(&asked, "stand by me", &searcher, 8, DEFAULT_MATCH_THRESHOLD)
            .await
            .expect_err("a question is not a song")
            .to_string();
        assert!(err.contains("a resolved song"), "unhelpful: {err}");
        assert!(
            searcher.seen.lock().unwrap().is_empty(),
            "and nothing was searched for"
        );
    }

    // ---- the popularity policy -------------------------------------------
    /// `AMBIGUOUS_POLICY=popular` is a documented escape hatch, and it is
    /// deliberately timid: the popular one has to be *three times* the other.
    #[test]
    fn popularity_only_settles_a_tie_when_it_is_overwhelming() {
        let big = resolve::CatalogTrack {
            popularity: Some(900),
            ..catalog_track("A", "Shared", Some(200.0))
        };
        let small = resolve::CatalogTrack {
            popularity: Some(100),
            ..catalog_track("B", "Shared", Some(200.0))
        };
        assert!(popular_beats_a_rival(&big, std::slice::from_ref(&small)));
        assert!(!popular_beats_a_rival(&small, std::slice::from_ref(&big)));

        // Barely more than twice as big is not enough.
        let middling = resolve::CatalogTrack {
            popularity: Some(400),
            ..catalog_track("B", "Shared", Some(200.0))
        };
        assert!(!popular_beats_a_rival(&big, &[middling]));
    }

    /// Two recordings by the *same* artist are not a choice between artists,
    /// and a rival with no popularity gives nothing to compare against.
    #[test]
    fn popularity_never_settles_a_question_within_one_artist() {
        let big = resolve::CatalogTrack {
            popularity: Some(900),
            ..catalog_track("A", "Shared", Some(200.0))
        };
        let same_artist = resolve::CatalogTrack {
            popularity: Some(10),
            ..catalog_track("A", "Shared", Some(240.0))
        };
        assert!(!popular_beats_a_rival(&big, &[same_artist]));

        let unknown = catalog_track("B", "Shared", Some(200.0));
        assert!(!popular_beats_a_rival(&big, &[unknown]));
        assert!(!popular_beats_a_rival(
            &catalog_track("A", "Shared", None),
            &[big]
        ));
    }

    // ---- live --------------------------------------------------------------

    /// The whole path, against the real catalog and the real YouTube.
    ///
    /// Ignored by default because it needs network access, the yt-dlp binary
    /// and the Deezer API: `cargo test -- --ignored search`.
    #[tokio::test]
    #[ignore = "needs network, yt-dlp and the Deezer API"]
    async fn the_whole_path_picks_the_requested_recording() {
        // Built directly rather than through `Config`, which would also want a
        // Discord token and a voice channel id that a test has no use for.
        let catalog = resolve::Catalog::with_budget(
            vec![Box::new(resolve::Deezer::new().expect("HTTP client"))],
            crate::config::DEFAULT_CATALOG_MATCH_THRESHOLD,
            2_500,
        );
        for (heard, expected) in [
            // Misheard proper nouns, and a video that outranks the real track.
            ("drakula tame impala", "Dracula"),
            ("hoolig bts", "Hooligan"),
        ] {
            let resolved =
                resolve_request(&request(heard), &catalog, None, None, GuildId::new(1)).await;
            assert!(resolved.as_track().is_some(), "{heard}: {resolved:?}");
            let choice = find_video(&resolved, heard, &YtDlpSearch, 8, DEFAULT_MATCH_THRESHOLD)
                .await
                .unwrap_or_else(|e| panic!("{heard}: {e:#}"));
            for scored in &choice.ranked {
                println!(
                    "{:7.1} {:7.1}s {}",
                    scored.score(),
                    scored.candidate.duration.unwrap_or(0.0),
                    scored.candidate.title()
                );
            }
            let best = choice.best().expect("a result");
            assert!(
                choice.confident,
                "{heard:?} was not confident: {:?}",
                best.parts
            );
            assert!(
                best.candidate
                    .title()
                    .to_lowercase()
                    .contains(&expected.to_lowercase()),
                "{heard:?} picked {:?}, expected something naming {expected:?}",
                best.candidate.title()
            );
        }
    }

    /// Hits the real YouTube via yt-dlp. Ignored by default because it needs
    /// network access and the yt-dlp binary.
    #[tokio::test]
    #[ignore = "needs network and yt-dlp"]
    async fn ranks_a_live_search_against_the_catalog() {
        // The search that motivated all of this, and its results as observed.
        let resolved = Resolved::Track {
            track: resolve::CatalogTrack {
                artist: "Tame Impala".to_string(),
                title: "Dracula".to_string(),
                duration_secs: Some(205.0),
                id: "1".to_string(),
                provider: "catalog",
                popularity: None,
            },
            durations: vec![205.0],
        };
        let target = Target::from_track(
            resolved.as_track().expect("a song"),
            vec![205.0],
            "dracula tame impala",
        );
        let entries = youtube_candidates(&target.search_query(), 8).await.unwrap();
        println!("searched: {}", target.search_query());
        let choice = pick(&entries, &target, DEFAULT_MATCH_THRESHOLD);
        for scored in &choice.ranked {
            println!("{:7.1} {}", scored.score(), scored.parts.explain());
        }
        let best = choice.best().unwrap();
        assert_eq!(best.candidate.duration, Some(206.0));
        assert!(
            choice.confident,
            "the requested recording should have been recognised: {:?}",
            best.parts
        );
    }
}
