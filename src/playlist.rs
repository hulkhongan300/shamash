//! Links pasted into `/play`: one video, or a whole playlist or album.
//!
//! A spoken request never names a link, but it can name an album, and the
//! album is then listed from here too: see [`crate::album`].

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
#[path = "tests/playlist.rs"]
mod tests;
