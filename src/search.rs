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
const NON_SONG_MARKERS: [&str; 15] = [
    "lyric",
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
    "full album",
    "compilation",
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
}

impl Candidate {
    pub fn title(&self) -> &str {
        self.title.as_deref().unwrap_or("unknown title")
    }

    pub fn channel(&self) -> &str {
        self.channel.as_deref().unwrap_or("unknown channel")
    }

    /// Whether the result looks like a song rather than a clip, cover, or mix.
    fn is_song(&self) -> bool {
        let plausible_length = match self.duration {
            Some(duration) => (MIN_DURATION_SECS..=MAX_DURATION_SECS).contains(&duration),
            // Unknown duration: let popularity decide.
            None => true,
        };
        let title = self
            .title
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        plausible_length && !NON_SONG_MARKERS.iter().any(|m| title.contains(m))
    }

    fn views(&self) -> u64 {
        self.view_count.unwrap_or(0)
    }
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

    select(&entries)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no playable song found for '{query}'"))
}

/// Picks the most-viewed song, ignoring non-song results when any song is
/// available and otherwise falling back to the most-viewed result at all.
fn select(entries: &[Candidate]) -> Option<&Candidate> {
    entries
        .iter()
        .filter(|entry| entry.is_song())
        .max_by_key(|entry| entry.views())
        .or_else(|| entries.iter().max_by_key(|entry| entry.views()))
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
        }
    }

    #[test]
    fn picks_most_viewed_song() {
        let entries = [
            candidate("song a", Some(1_000), Some(200.0)),
            candidate("song b", Some(900_000), Some(180.0)),
            candidate("song c", Some(5_000), Some(210.0)),
        ];
        assert_eq!(select(&entries).unwrap().title(), "song b");
    }

    #[test]
    fn skips_clips_and_mixes() {
        let entries = [
            candidate("song intro clip", Some(9_000_000), Some(20.0)),
            candidate("vlog about the song", Some(8_000_000), Some(600.0)),
            candidate("full album mix", Some(7_000_000), Some(3_600.0)),
            candidate("the real song", Some(50_000), Some(200.0)),
        ];
        assert_eq!(select(&entries).unwrap().title(), "the real song");
    }

    #[test]
    fn deprioritises_covers_remixes_and_lyric_videos() {
        let entries = [
            candidate("song (slowed + reverb)", Some(4_000_000), Some(200.0)),
            candidate("song (nightcore)", Some(3_000_000), Some(200.0)),
            candidate("song (lyric video)", Some(2_000_000), Some(210.0)),
            candidate("song", Some(100_000), Some(205.0)),
        ];
        assert_eq!(select(&entries).unwrap().title(), "song");
    }

    #[test]
    fn keeps_unknown_duration_candidates() {
        let entries = [
            candidate("no duration", Some(10), None),
            candidate("short but known", Some(99_999), Some(10.0)),
        ];
        assert_eq!(select(&entries).unwrap().title(), "no duration");
    }

    #[test]
    fn falls_back_when_everything_is_a_clip() {
        let entries = [
            candidate("clip", Some(100), Some(10.0)),
            candidate("long mix", Some(50), Some(4_000.0)),
        ];
        assert_eq!(select(&entries).unwrap().title(), "clip");
    }

    #[test]
    fn no_results_selects_nothing() {
        assert!(select(&[]).is_none());
    }

    /// Hits the real YouTube via yt-dlp. Ignored by default because it needs
    /// network access and the yt-dlp binary:
    /// `cargo test -- --ignored search`.
    #[tokio::test]
    #[ignore = "needs network and yt-dlp"]
    async fn finds_most_popular_song_for_real() {
        let found = most_popular_song("look at me", DEFAULT_CANDIDATES)
            .await
            .expect("search should find a song");
        println!("picked: {} ({:?})", found.title(), found.view_count);
        assert!(!found.url.is_empty());
    }
}
