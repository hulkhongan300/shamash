//! Links pasted into `/play`: one video, or a whole playlist or album.
//!
//! Only `/play` reads these. A spoken request never queues a playlist: the
//! bot cannot show what it is about to queue, and an album nobody meant to ask
//! for is twenty songs to skip.

use anyhow::Context as _;
use std::time::Duration;
use tokio::process::Command;

/// How long listing a playlist may take. A flat listing reads one page per
/// hundred entries, so even a long playlist answers in a few seconds.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// The YouTube hosts a link may name.
fn is_youtube_host(host: &str) -> bool {
    matches!(
        host,
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com"
    )
}

/// A video id: exactly eleven of `[A-Za-z0-9_-]`.
fn is_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// A playlist or album id. YouTube's are 13 to 72 characters (`PL…`,
/// `OLAK5uy_…`, `RD…`), always from the same alphabet as a video id.
fn is_list_id(id: &str) -> bool {
    (2..=128).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

fn parse(text: &str) -> Option<reqwest::Url> {
    let url = reqwest::Url::parse(text.trim()).ok()?;
    matches!(url.scheme(), "http" | "https").then_some(url)
}

/// The video a link points at, for a YouTube or YouTube Music link.
///
/// Links only: a bare word is never taken for a video id, so `/play dracula`
/// searches instead of trying to play a video called "dracula".
pub fn video_id(text: &str) -> Option<String> {
    let url = parse(text)?;
    let host = url.host_str()?;
    let id = if host == "youtu.be" {
        url.path_segments()?.next()?.to_string()
    } else if is_youtube_host(host) {
        let mut segments = url.path_segments()?;
        match segments.next()? {
            "watch" => url
                .query_pairs()
                .find(|(key, _)| key == "v")
                .map(|(_, v)| v.into_owned())?,
            "shorts" | "live" | "embed" => segments.next()?.to_string(),
            _ => return None,
        }
    } else {
        return None;
    };
    is_video_id(&id).then_some(id)
}

/// The playlist a link points at, as the URL to list it from.
///
/// A playlist page (`/playlist?list=…`, YouTube Music albums included) or a
/// YouTube Music album page (`/browse/MPREb…`). A *video* that happens to be
/// in a playlist (`watch?v=…&list=…`) is not one: that is somebody sharing a
/// song, and it plays as that song.
pub fn playlist_url(text: &str) -> Option<String> {
    let url = parse(text)?;
    if !is_youtube_host(url.host_str()?) {
        return None;
    }
    let mut segments = url.path_segments()?;
    match segments.next()? {
        "playlist" => {
            let list = url
                .query_pairs()
                .find(|(key, _)| key == "list")
                .map(|(_, v)| v.into_owned())?;
            is_list_id(&list).then(|| format!("https://www.youtube.com/playlist?list={list}"))
        }
        "browse" => {
            let id = segments.next()?;
            (id.starts_with("MPREb") && is_list_id(id))
                .then(|| format!("https://music.youtube.com/browse/{id}"))
        }
        _ => None,
    }
}

/// One track of a playlist, as the listing reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub video_id: String,
    pub title: String,
    pub duration_secs: Option<f64>,
    pub channel: Option<String>,
}

impl Track {
    pub fn url(&self) -> String {
        format!("https://www.youtube.com/watch?v={}", self.video_id)
    }
}

/// A listed playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct Playlist {
    pub title: String,
    /// The playable tracks, in playlist order.
    pub tracks: Vec<Track>,
    /// Entries left out because they cannot be played: private, deleted, or
    /// without an id.
    pub unavailable: usize,
}

/// Lists a playlist without downloading anything.
pub async fn list(url: &str) -> anyhow::Result<Playlist> {
    let mut command = Command::new("yt-dlp");
    command
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .args(["--flat-playlist", "--dump-single-json", "--no-warnings"])
        .arg(url);
    let output = tokio::time::timeout(LIST_TIMEOUT, command.output())
        .await
        .map_err(|_| anyhow::anyhow!("listing {url} took longer than {LIST_TIMEOUT:?}"))?
        .with_context(|| format!("could not run yt-dlp to list {url}"))?;
    anyhow::ensure!(
        output.status.success(),
        "yt-dlp could not list {url}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("yt-dlp printed something other than JSON")?;
    parse_listing(&json)
}

fn parse_listing(json: &serde_json::Value) -> anyhow::Result<Playlist> {
    let entries = json
        .get("entries")
        .and_then(|e| e.as_array())
        .context("that link is not a playlist")?;
    let mut tracks = Vec::new();
    let mut unavailable = 0;
    for entry in entries {
        let id = entry.get("id").and_then(|v| v.as_str()).unwrap_or_default();
        let title = entry
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        // A private or deleted video keeps its slot in the listing, with a
        // placeholder title and nothing behind it.
        let gone = matches!(title, "[Private video]" | "[Deleted video]");
        if !is_video_id(id) || gone {
            unavailable += 1;
            continue;
        }
        tracks.push(Track {
            video_id: id.to_string(),
            title: title.to_string(),
            duration_secs: entry.get("duration").and_then(|v| v.as_f64()),
            channel: entry
                .get("channel")
                .or_else(|| entry.get("uploader"))
                .and_then(|v| v.as_str())
                .map(str::to_string),
        });
    }
    let title = json
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty())
        .unwrap_or("the playlist")
        .to_string();
    Ok(Playlist {
        title,
        tracks,
        unavailable,
    })
}

/// How many of `listed` tracks fit, given how many songs already wait.
///
/// The playlist goes in whole when it fits, and is cut at the end when it
/// does not, so the album still starts at its first track.
pub fn fitting(listed: usize, waiting: usize, max_queue: usize) -> usize {
    listed.min(max_queue.saturating_sub(waiting))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_links_of_every_shape_are_read() {
        for link in [
            "https://www.youtube.com/watch?v=cuMuMnCRfqk",
            "https://youtube.com/watch?v=cuMuMnCRfqk&t=10",
            "https://m.youtube.com/watch?v=cuMuMnCRfqk",
            "https://music.youtube.com/watch?v=cuMuMnCRfqk&si=abc",
            "https://youtu.be/cuMuMnCRfqk?si=abc",
            "https://www.youtube.com/shorts/cuMuMnCRfqk",
            "https://www.youtube.com/live/cuMuMnCRfqk",
            "  https://www.youtube.com/watch?v=cuMuMnCRfqk  ",
        ] {
            assert_eq!(video_id(link).as_deref(), Some("cuMuMnCRfqk"), "{link}");
        }
    }

    /// The bug this replaced: any single word was taken for a video id, so
    /// `/play dracula` tried to play a video called "dracula".
    #[test]
    fn a_bare_word_is_a_search_not_a_video() {
        for text in [
            "dracula",
            "cuMuMnCRfqk",
            "xxxtentacion",
            "dracula tame impala",
        ] {
            assert_eq!(video_id(text), None, "{text}");
            assert_eq!(playlist_url(text), None, "{text}");
        }
    }

    #[test]
    fn other_sites_and_broken_ids_are_not_videos() {
        assert_eq!(video_id("https://example.com/watch?v=cuMuMnCRfqk"), None);
        assert_eq!(video_id("https://www.youtube.com/watch?v=short"), None);
        assert_eq!(video_id("https://www.youtube.com/@TameImpala"), None);
    }

    #[test]
    fn playlist_and_album_links_are_read() {
        let album =
            "https://www.youtube.com/playlist?list=OLAK5uy_kW9hN-oBmekJ06jhhfStpwRd5pcRKIztY";
        assert_eq!(playlist_url(album).as_deref(), Some(album));
        assert_eq!(
            playlist_url("https://music.youtube.com/playlist?list=PLabc123XYZ&si=x").as_deref(),
            Some("https://www.youtube.com/playlist?list=PLabc123XYZ")
        );
        assert_eq!(
            playlist_url("https://music.youtube.com/browse/MPREb_abc123DEF").as_deref(),
            Some("https://music.youtube.com/browse/MPREb_abc123DEF")
        );
    }

    /// Sharing one song from a playlist shares a link with both ids. It is
    /// that song, not the playlist.
    #[test]
    fn a_video_inside_a_playlist_is_that_video() {
        let link = "https://www.youtube.com/watch?v=cuMuMnCRfqk&list=OLAK5uy_kW9hN-oBmekJ06jhhfStpwRd5pcRKIztY";
        assert_eq!(playlist_url(link), None);
        assert_eq!(video_id(link).as_deref(), Some("cuMuMnCRfqk"));
    }

    #[test]
    fn a_listing_keeps_the_order_and_drops_what_cannot_play() {
        let json = serde_json::json!({
            "title": "Album - 17",
            "entries": [
                {"id": "pG-tbrzlMVU", "title": "The Explanation", "duration": 51.0},
                {"id": "AAAAAAAAAAA", "title": "[Private video]"},
                {"id": "C1D3G2VGQ_8", "title": "Jocelyn Flores", "duration": 120.0,
                 "channel": "XXXTENTACION"},
                {"title": "no id"},
            ]
        });
        let playlist = parse_listing(&json).unwrap();
        assert_eq!(playlist.title, "Album - 17");
        assert_eq!(playlist.unavailable, 2);
        let titles: Vec<_> = playlist.tracks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["The Explanation", "Jocelyn Flores"]);
        assert_eq!(playlist.tracks[1].channel.as_deref(), Some("XXXTENTACION"));
    }

    #[test]
    fn a_single_video_is_not_a_playlist() {
        let json = serde_json::json!({"id": "cuMuMnCRfqk", "title": "Dracula"});
        assert!(parse_listing(&json).is_err());
    }

    #[test]
    fn a_long_playlist_is_cut_to_the_room_left() {
        assert_eq!(fitting(11, 0, 20), 11);
        assert_eq!(fitting(11, 15, 20), 5);
        assert_eq!(fitting(50, 0, 20), 20);
        assert_eq!(fitting(11, 20, 20), 0);
        assert_eq!(fitting(11, 25, 20), 0);
    }

    /// Lists the album from the issue. Needs network and yt-dlp.
    #[tokio::test]
    #[ignore = "needs network and yt-dlp"]
    async fn the_live_album_lists_in_order() {
        let url = playlist_url(
            "https://www.youtube.com/playlist?list=OLAK5uy_kW9hN-oBmekJ06jhhfStpwRd5pcRKIztY",
        )
        .unwrap();
        let playlist = list(&url).await.expect("listing");
        assert_eq!(playlist.tracks.len(), 11, "{playlist:?}");
        assert_eq!(playlist.tracks[0].title, "The Explanation");
        assert_eq!(playlist.tracks[10].title, "Ayala (Outro)");
    }
}
