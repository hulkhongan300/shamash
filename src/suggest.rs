//! Song suggestions for `/play`, as Discord autocomplete.
//!
//! As someone types, Discord asks for choices and shows them in a list over the
//! message box. The choices come from YouTube Music's song search, which
//! returns songs as the label spelled them, the artists credited, the running
//! time and, crucially, the video id of the audio track itself. A choice's value
//! carries that id, so picking one plays exactly that upload: there is nothing
//! left to search for or to guess.
//!
//! Typing something and sending it without picking still works, and asks the
//! catalog the way it always did; and a pasted YouTube link plays that video.

use crate::resolve::{CatalogTrack, SongRow, YtMusic};
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// Discord shows at most 25 choices.
pub const MAX_CHOICES: usize = 25;

/// Discord's limit on a choice's name and on its value, in characters.
const MAX_CHOICE_CHARS: usize = 100;

/// Prefix of a choice value that names a video.
const PICK_PREFIX: &str = "yt:";

/// How long a suggestion search may take. Discord gives an autocomplete
/// request three seconds in all, and an answer that misses it is thrown away.
const SEARCH_TIMEOUT: Duration = Duration::from_millis(2_200);

/// Searches remembered, so typing back over a query does not search again.
const QUERY_CACHE: usize = 200;

/// Suggested songs remembered by video id, so a pick can say what it is.
const PICKED_CACHE: usize = 2_000;

/// Fewer characters than this are not worth a search.
const MIN_QUERY_CHARS: usize = 2;

/// What a `/play` value asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wanted {
    /// A suggestion that was picked, or a pasted link: this exact video.
    Video(String),
    /// Free text, for the catalog.
    Search(String),
}

/// Reads a `/play` value.
pub fn wanted(value: &str) -> Wanted {
    let value = value.trim();
    if let Some(id) = value.strip_prefix(PICK_PREFIX).filter(|id| is_video_id(id)) {
        return Wanted::Video(id.to_string());
    }
    if let Some(id) = link_video_id(value) {
        return Wanted::Video(id);
    }
    Wanted::Search(value.to_string())
}

/// A YouTube video id: eleven URL-safe base64 characters.
fn is_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The video id in a YouTube or YouTube Music link, if `text` is one.
pub fn link_video_id(text: &str) -> Option<String> {
    let rest = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"))?;
    let (host, path) = rest.split_once('/')?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    let id = match host {
        "youtu.be" => path,
        "youtube.com" | "m.youtube.com" | "music.youtube.com" => {
            match path.strip_prefix("watch?") {
                Some(query) => query.split('&').find_map(|pair| pair.strip_prefix("v="))?,
                None => path
                    .strip_prefix("shorts/")
                    .or_else(|| path.strip_prefix("live/"))?,
            }
        }
        _ => return None,
    };
    let id = id.split(['?', '&', '#', '/']).next()?;
    is_video_id(id).then(|| id.to_string())
}

/// What a video is, asked of YouTube: for a pasted link, or a pick made before
/// a restart emptied the memory of what was suggested.
///
/// Falls back to a bare description rather than failing, because the video
/// can still be played without its name.
pub async fn describe(video_id: &str) -> CatalogTrack {
    let fallback = || CatalogTrack {
        artist: "YouTube".to_string(),
        title: format!("youtu.be/{video_id}"),
        duration_secs: None,
        id: video_id.to_string(),
        provider: "youtube",
        popularity: None,
        video_id: Some(video_id.to_string()),
    };
    let mut command = tokio::process::Command::new("yt-dlp");
    command
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .args([
            "--skip-download",
            "--no-warnings",
            "--no-playlist",
            "--print",
            "%(title)s\n%(channel)s\n%(duration)s",
            "--",
            video_id,
        ]);
    let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(15), command.output()).await
    else {
        return fallback();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    match (lines.next(), lines.next(), lines.next()) {
        (Some(title), Some(channel), duration) if output.status.success() => CatalogTrack {
            artist: channel.to_string(),
            title: title.to_string(),
            duration_secs: duration.and_then(|d| d.trim().parse().ok()),
            ..fallback()
        },
        _ => fallback(),
    }
}

/// One choice for Discord: what it shows, and what `/play` receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub name: String,
    pub value: String,
}

/// The line a song is shown as: `Title 🅴 – Artist [m:ss]`, cut to fit.
///
/// The title gives way first when it is too long, because the artist and the
/// running time are what tell two songs of the same name apart.
pub fn label(row: &SongRow) -> String {
    let track = &row.track;
    let explicit = if row.explicit { " 🅴" } else { "" };
    let time = track
        .duration_secs
        .map(|secs| {
            let secs = secs.round() as u64;
            format!("  [{}:{:02}]", secs / 60, secs % 60)
        })
        .unwrap_or_default();
    let tail = format!("{explicit} – {}{time}", row.credit);
    let room = MAX_CHOICE_CHARS.saturating_sub(tail.chars().count());
    let title = truncate(&track.title, room.max(12));
    truncate(&format!("{title}{tail}"), MAX_CHOICE_CHARS)
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Songs suggested to `/play`, and what they stand for once picked.
pub struct Suggestions {
    ytmusic: Option<YtMusic>,
    /// Recent searches and their rows, oldest first.
    queries: Mutex<VecDeque<(String, Vec<SongRow>)>>,
    /// Every suggested song by video id, oldest first.
    picked: Mutex<(HashMap<String, CatalogTrack>, VecDeque<String>)>,
}

impl Default for Suggestions {
    fn default() -> Self {
        Self {
            ytmusic: YtMusic::new().ok(),
            queries: Mutex::new(VecDeque::new()),
            picked: Mutex::new((HashMap::new(), VecDeque::new())),
        }
    }
}

impl Suggestions {
    /// The choices for what has been typed so far.
    ///
    /// Never fails: Discord needs an answer to every request, and an empty list
    /// is the honest one when the search is down or slow.
    pub async fn choices(&self, typed: &str) -> Vec<Choice> {
        let query = typed.trim().to_lowercase();
        if query.chars().count() < MIN_QUERY_CHARS || link_video_id(typed.trim()).is_some() {
            return Vec::new();
        }
        let rows = match self.cached(&query) {
            Some(rows) => rows,
            None => {
                let rows = self.search(&query).await;
                self.remember_query(&query, &rows);
                rows
            }
        };
        // One choice per song: YouTube Music lists a song once per release it
        // is on, and the same title by the same artists twice reads as a bug.
        let mut seen = std::collections::HashSet::new();
        rows.iter()
            .filter_map(|row| {
                let id = row.track.video_id.as_deref()?;
                let song = (
                    crate::matcher::normalize(&row.track.title),
                    crate::matcher::normalize(&row.credit),
                );
                seen.insert(song).then(|| Choice {
                    name: label(row),
                    value: format!("{PICK_PREFIX}{id}"),
                })
            })
            .take(MAX_CHOICES)
            .collect()
    }

    /// What a picked video was suggested as, if it was suggested here.
    pub fn track(&self, video_id: &str) -> Option<CatalogTrack> {
        self.picked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .0
            .get(video_id)
            .cloned()
    }

    async fn search(&self, query: &str) -> Vec<SongRow> {
        let Some(ytmusic) = &self.ytmusic else {
            return Vec::new();
        };
        match tokio::time::timeout(SEARCH_TIMEOUT, ytmusic.songs(query)).await {
            Ok(Ok(rows)) => rows,
            Ok(Err(e)) => {
                tracing::debug!(query, error = ?e, "song suggestions failed");
                Vec::new()
            }
            Err(_) => {
                tracing::debug!(query, "song suggestions timed out");
                Vec::new()
            }
        }
    }

    fn cached(&self, query: &str) -> Option<Vec<SongRow>> {
        self.queries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|(q, _)| q == query)
            .map(|(_, rows)| rows.clone())
    }

    fn remember_query(&self, query: &str, rows: &[SongRow]) {
        // A failed search is not remembered, so the next keystroke retries it.
        if rows.is_empty() {
            return;
        }
        {
            let mut queries = self.queries.lock().unwrap_or_else(PoisonError::into_inner);
            queries.push_back((query.to_string(), rows.to_vec()));
            while queries.len() > QUERY_CACHE {
                queries.pop_front();
            }
        }
        let mut picked = self.picked.lock().unwrap_or_else(PoisonError::into_inner);
        let (by_id, order) = &mut *picked;
        for row in rows {
            if let Some(id) = row.track.video_id.clone()
                && by_id.insert(id.clone(), row.track.clone()).is_none()
            {
                order.push_back(id);
            }
        }
        while order.len() > PICKED_CACHE {
            if let Some(old) = order.pop_front() {
                by_id.remove(&old);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(title: &str, artist: &str, secs: Option<f64>, explicit: bool, id: &str) -> SongRow {
        SongRow {
            track: CatalogTrack {
                artist: artist.to_string(),
                title: title.to_string(),
                duration_secs: secs,
                id: id.to_string(),
                provider: "ytmusic",
                popularity: None,
                video_id: Some(id.to_string()),
            },
            credit: artist.to_string(),
            explicit,
        }
    }

    #[test]
    fn a_song_reads_like_the_list_in_the_youtube_music_app() {
        assert_eq!(
            label(&row("Moonlight", "XXXTENTACION", Some(135.0), true, "a")),
            "Moonlight 🅴 – XXXTENTACION  [2:15]"
        );
        assert_eq!(
            label(&row("changes", "XXXTENTACION", Some(121.0), false, "a")),
            "changes – XXXTENTACION  [2:01]"
        );
        assert_eq!(
            label(&row("Untimed", "Someone", None, false, "a")),
            "Untimed – Someone"
        );
    }

    #[test]
    fn a_long_title_gives_way_to_the_artist_and_the_time() {
        let long = "A".repeat(150);
        let name = label(&row(
            &long,
            "Lil Peep, XXXTENTACION",
            Some(196.0),
            true,
            "a",
        ));
        assert!(name.chars().count() <= MAX_CHOICE_CHARS, "{}", name.len());
        assert!(name.ends_with("– Lil Peep, XXXTENTACION  [3:16]"), "{name}");
        assert!(name.contains('…'));
    }

    #[test]
    fn a_picked_suggestion_is_that_video() {
        assert_eq!(
            wanted("yt:cuMuMnCRfqk"),
            Wanted::Video("cuMuMnCRfqk".into())
        );
        // Not an id, so it is just words someone typed.
        assert_eq!(wanted("yt:hello"), Wanted::Search("yt:hello".into()));
        assert_eq!(
            wanted("  xxxtentacion "),
            Wanted::Search("xxxtentacion".into())
        );
    }

    #[test]
    fn a_pasted_link_is_that_video() {
        for link in [
            "https://www.youtube.com/watch?v=cuMuMnCRfqk",
            "https://youtube.com/watch?list=x&v=cuMuMnCRfqk&t=10",
            "https://youtu.be/cuMuMnCRfqk?si=abc",
            "https://music.youtube.com/watch?v=cuMuMnCRfqk&feature=share",
            "https://m.youtube.com/watch?v=cuMuMnCRfqk",
            "https://www.youtube.com/shorts/cuMuMnCRfqk",
        ] {
            assert_eq!(wanted(link), Wanted::Video("cuMuMnCRfqk".into()), "{link}");
        }
        assert_eq!(
            link_video_id("https://example.com/watch?v=cuMuMnCRfqk"),
            None
        );
        assert_eq!(link_video_id("https://www.youtube.com/@TameImpala"), None);
    }

    #[tokio::test]
    async fn suggestions_are_remembered_and_name_their_song() {
        let suggestions = Suggestions {
            ytmusic: None,
            queries: Mutex::new(VecDeque::new()),
            picked: Mutex::new((HashMap::new(), VecDeque::new())),
        };
        let rows = vec![
            row(
                "Moonlight",
                "XXXTENTACION",
                Some(135.0),
                true,
                "GX8Hg6kWQYI",
            ),
            // The same song from another release: listed once.
            row(
                "Moonlight",
                "XXXTENTACION",
                Some(135.0),
                true,
                "aaaaaaaaaaa",
            ),
            row("Hope", "XXXTENTACION", Some(110.0), true, "3c5RrPQ3w_8"),
        ];
        suggestions.remember_query("xxxtentacion", &rows);

        // Served from memory, with no search behind it, and one choice per song.
        let choices = suggestions.choices("XXXTentacion ").await;
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].value, "yt:GX8Hg6kWQYI");
        assert_eq!(
            suggestions.track("3c5RrPQ3w_8").map(|t| t.title),
            Some("Hope".to_string())
        );
    }

    #[tokio::test]
    async fn too_little_typed_or_a_link_asks_for_nothing() {
        let suggestions = Suggestions::default();
        assert!(suggestions.choices("x").await.is_empty());
        assert!(
            suggestions
                .choices("https://youtu.be/cuMuMnCRfqk")
                .await
                .is_empty()
        );
    }
}
