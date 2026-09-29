//! Finds the YouTube video to play for a spoken request.
//!
//! A YouTube search returns whatever matched the words: clips, live
//! performances, lyric videos, 10-minute mixes. This module keeps the
//! results that look like actual songs and picks the most-viewed one, so
//! "play look at me" starts the popular track rather than a random upload.

use anyhow::Context;
use serde::Deserialize;
use tokio::process::Command;

/// How many search results to consider.
pub const DEFAULT_CANDIDATES: usize = 10;
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

/// Title words that carry no meaning for matching, so they are not required to
/// appear in a result.
const TITLE_STOPWORDS: [&str; 13] = [
    "the", "a", "an", "of", "by", "and", "ft", "feat", "official", "music", "video", "mv", "m/v",
];

/// One entry of a YouTube search result.
#[derive(Debug, Clone, Deserialize)]
pub struct Candidate {
    /// Video title as shown on YouTube.
    pub title: Option<String>,
    /// Canonical watch URL.
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
    fn is_song(&self) -> bool {
        let plausible_length = match self.duration {
            Some(duration) => (MIN_DURATION_SECS..=MAX_DURATION_SECS).contains(&duration),
            // Unknown duration: let popularity decide.
            None => true,
        };
        let title = self.title_lowercase();
        plausible_length && !NON_SONG_MARKERS.iter().any(|m| title.contains(m))
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

    /// How many of the words in the request this result's title accounts for.
    ///
    /// Speech-to-text mangles proper nouns, so "bts hoolig" has to match
    /// "Hooligan". A title word counts as matching a request word when either
    /// starts the other, which covers both a dropped letter and a truncated
    /// word without admitting the loose matches that a real edit distance
    /// would.
    fn matched_terms(&self, terms: &[String]) -> usize {
        let title: Vec<String> = self
            .title_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect();
        terms
            .iter()
            .filter(|term| {
                title
                    .iter()
                    .any(|word| word.starts_with(term.as_str()) || term.starts_with(word.as_str()))
            })
            .count()
    }
}

/// The words of a spoken request that a result's title has to account for.
///
/// Words carrying no meaning for matching are dropped, so a request for
/// "official music by tame impala" still only requires the name.
fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|w| !w.is_empty() && !TITLE_STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// Searches YouTube via `yt-dlp` and returns the most-viewed song.
pub async fn most_popular_song(query: &str, candidates: usize) -> anyhow::Result<Candidate> {
    anyhow::ensure!(candidates > 0, "need at least one search candidate");
    let playlist_end = candidates.to_string();
    let search = format!("ytsearch{candidates}:{query}");

    let output = Command::new("yt-dlp")
        .args(["-j", "--flat-playlist", "--no-warnings", "--playlist-end"])
        .arg(playlist_end)
        .arg(&search)
        .output()
        .await
        .context("failed to run yt-dlp; is it installed and on PATH?")?;
    anyhow::ensure!(
        output.status.success(),
        "yt-dlp search failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );

    let entries: Vec<Candidate> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).context("failed to parse yt-dlp JSON output"))
        .collect::<Result<_, _>>()?;

    select(&entries, query)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no playable song found for '{query}'"))
}

/// Picks the result that best answers the request.
///
/// Order of preference, most significant first:
///
/// 1. **Relevance.** A result has to account for the words in the request.
///    This is what stops a popular but unrelated video winning: asked for
///    "bts hoolig", the official video for a different song can have twice
///    the views and a title that never mentions the request at all.
/// 2. **Audio over video.** Among relevant results, one that is the audio on
///    its own beats an official MV.
/// 3. **Views.** Popularity breaks the remaining ties.
///
/// Results that do not look like songs at all are only considered if nothing
/// relevant survives, so a niche song with a modest upload still plays.
fn select<'a>(entries: &'a [Candidate], query: &str) -> Option<&'a Candidate> {
    let terms = query_terms(query);
    let songs: Vec<&Candidate> = entries.iter().filter(|entry| entry.is_song()).collect();

    // A request of only filler words ("play something") leaves nothing to
    // match on, so relevance cannot discriminate and popularity decides.
    if !terms.is_empty() {
        let best = songs
            .iter()
            .map(|entry| (entry.matched_terms(&terms), entry))
            .filter(|(matched, _)| *matched > 0)
            .max_by_key(|(matched, entry)| (*matched, entry.media_rank(), entry.views()));
        if let Some((_, entry)) = best {
            return Some(entry);
        }
    }

    songs
        .iter()
        .max_by_key(|entry| (entry.media_rank(), entry.views()))
        .copied()
        .or_else(|| entries.iter().max_by_key(|entry| entry.views()))
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

    #[test]
    fn picks_most_viewed_song() {
        let entries = [
            candidate("song a", Some(1_000), Some(200.0)),
            candidate("song b", Some(900_000), Some(180.0)),
            candidate("song c", Some(5_000), Some(210.0)),
        ];
        assert_eq!(select(&entries, "song").unwrap().title(), "song b");
    }

    #[test]
    fn skips_clips_and_mixes() {
        let entries = [
            candidate("song intro clip", Some(9_000_000), Some(20.0)),
            candidate("vlog about the song", Some(8_000_000), Some(600.0)),
            candidate("full album mix", Some(7_000_000), Some(3_600.0)),
            candidate("the real song", Some(50_000), Some(200.0)),
        ];
        assert_eq!(select(&entries, "song").unwrap().title(), "the real song");
    }

    #[test]
    fn deprioritises_covers_remixes_and_karaoke() {
        let entries = [
            candidate("song (slowed + reverb)", Some(4_000_000), Some(200.0)),
            candidate("song (nightcore)", Some(3_000_000), Some(200.0)),
            candidate("song (Karaoke Version)", Some(2_000_000), Some(210.0)),
            candidate("song", Some(100_000), Some(205.0)),
        ];
        assert_eq!(select(&entries, "song").unwrap().title(), "song");
    }

    /// The reported bug: asked for one song, the most-viewed result was a
    /// different song entirely, because nothing compared the title against the
    /// request and "2.0" outranked the real track.
    #[test]
    fn a_popular_unrelated_video_does_not_win() {
        let entries = [
            candidate(
                "BTS (방탄소년단) '2.0' Official MV",
                Some(185_000_000),
                Some(235.0),
            ),
            candidate(
                "BTS 방탄소년단 'Hooligan' Official Audio",
                Some(178_000),
                Some(182.0),
            ),
        ];
        assert_eq!(
            select(&entries, "bts hoolig").unwrap().title(),
            "BTS 방탄소년단 'Hooligan' Official Audio"
        );
    }

    /// The listener asked for a song, so the audio upload wins over the
    /// official music video however much more popular the video is.
    #[test]
    fn official_audio_beats_the_official_music_video() {
        let entries = [
            candidate("Hooligan Official MV", Some(95_000_000), Some(244.0)),
            candidate("Hooligan Official Audio", Some(178_000), Some(182.0)),
        ];
        assert_eq!(
            select(&entries, "hooligan").unwrap().title(),
            "Hooligan Official Audio"
        );
    }

    /// Speech-to-text drops letters from proper nouns, so a request that is
    /// nearly a title still has to match it.
    #[test]
    fn a_misspelled_request_still_matches_the_song() {
        let entries = [
            candidate(
                "BTS (방탄소년단) '2.0' Official MV",
                Some(185_000_000),
                Some(235.0),
            ),
            candidate("BTS 방탄소년단 'Hooligan'", Some(178_000), Some(182.0)),
        ];
        assert!(
            select(&entries, "hooligan")
                .unwrap()
                .title()
                .contains("Hooligan")
        );
    }

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

    /// A video marker wins over an audio marker when a title claims both, so
    /// "Official Audio [Official MV]" is still treated as a video.
    #[test]
    fn a_video_marker_beats_an_audio_marker_in_the_same_title() {
        let entry = candidate("Song (Official Audio) [Official MV]", Some(1), Some(200.0));
        assert_eq!(entry.media_rank(), 0);
    }

    /// When no result mentions the request, the most-viewed plausible upload
    /// still plays rather than the search failing outright.
    #[test]
    fn falls_back_to_popularity_when_nothing_is_relevant() {
        let entries = [
            candidate("completely unrelated one", Some(1_000), Some(200.0)),
            candidate("completely unrelated two", Some(9_000), Some(200.0)),
        ];
        assert_eq!(
            select(&entries, "hooligan").unwrap().title(),
            "completely unrelated two"
        );
    }

    /// A request of only filler words leaves nothing to match on, so
    /// popularity decides rather than relevance excluding everything.
    #[test]
    fn filler_only_requests_leave_popularity_to_decide() {
        let entries = [
            candidate("anything at all", Some(10), Some(200.0)),
            candidate("something else", Some(500), Some(200.0)),
        ];
        assert_eq!(
            select(&entries, "the official music").unwrap().title(),
            "something else"
        );
    }

    #[test]
    fn stopwords_are_not_required_to_appear_in_a_title() {
        let entries = [
            candidate("something else entirely", Some(9_000_000), Some(200.0)),
            candidate("Tame Impala - Dracula", Some(50_000), Some(200.0)),
        ];
        assert_eq!(
            select(&entries, "official music by tame impala")
                .unwrap()
                .title(),
            "Tame Impala - Dracula"
        );
    }

    #[test]
    fn keeps_unknown_duration_candidates() {
        let entries = [
            candidate("no duration", Some(10), None),
            candidate("short but known", Some(99_999), Some(10.0)),
        ];
        assert_eq!(select(&entries, "song").unwrap().title(), "no duration");
    }

    #[test]
    fn falls_back_when_everything_is_a_clip() {
        let entries = [
            candidate("clip", Some(100), Some(10.0)),
            candidate("long mix", Some(50), Some(4_000.0)),
        ];
        assert_eq!(select(&entries, "song").unwrap().title(), "clip");
    }

    #[test]
    fn no_results_selects_nothing() {
        assert!(select(&[], "song").is_none());
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

    /// Hits the real YouTube via yt-dlp. Ignored by default because it needs
    /// network access and the yt-dlp binary:
    /// `cargo test -- --ignored search`.
    #[tokio::test]
    #[ignore = "needs network and yt-dlp"]
    async fn finds_official_audio_for_real() {
        for (query, expect) in [
            ("bts hoolig", "Hooligan"),
            ("dracula tame impala", "Dracula"),
        ] {
            let found = most_popular_song(query, DEFAULT_CANDIDATES)
                .await
                .expect("search should find a song");
            println!(
                "{query:>24} -> {} | {:?} | rank {}",
                found.title(),
                found.channel(),
                found.media_rank()
            );
            assert!(
                found
                    .title()
                    .to_lowercase()
                    .contains(&expect.to_lowercase()),
                "{query:?} picked {:?}, expected something naming {expect:?}",
                found.title()
            );
        }
    }

    #[tokio::test]
    #[ignore = "needs network and yt-dlp"]
    async fn finds_most_popular_song_for_real() {
        let found = most_popular_song("look at me", DEFAULT_CANDIDATES)
            .await
            .expect("search should find a song");
        println!("picked: {} ({:?})", found.title(), found.view_count);
        assert!(!found.url.is_empty());
        assert!(
            found.video_id().is_some(),
            "a search hit must yield a thumbnail id"
        );
    }
}
