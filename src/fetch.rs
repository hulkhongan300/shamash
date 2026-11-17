//! Downloads a YouTube video's audio, or hands back the URL to play it from.
//!
//! Songbird's own [`YoutubeDl`](songbird::input::YoutubeDl) cannot be used
//! here. It hardcodes `-f "ba[abr>0][vcodec=none]/best"`, which on YouTube
//! resolves to format 251, WebM carrying Opus. Symphonia 0.5 ships no Opus
//! decoder at all, so the probe finds the container and then the codec refuses
//! the track, leaving the call silent. Its arguments also cannot be overridden,
//! because anything passed as user arguments is placed before that selector and
//! yt-dlp keeps the last one.
//!
//! So the stream is fetched here instead, preferring the AAC in an MP4
//! container that YouTube also publishes and that the `isomp4` and `aac`
//! handlers can decode. Only when a video offers nothing else is the best
//! stream downloaded and transcoded to WAV.
//!
//! The audio is *not* downloaded before it plays. One yt-dlp extraction says
//! which formats the video publishes and hands back their signed URLs, and the
//! one that can be decoded is played straight off YouTube while the rest of it
//! is written to the cache in the background. That is what moves the first
//! sample from "after the whole file" to "after the first few kilobytes". The
//! download path stays for the cases streaming cannot cover: a URL that has
//! expired, a connection that never delivers, a video with nothing but Opus.
//!
//! Every song gets its own file, named after its video id. A single shared
//! path cannot work: two commands handled at once, or a queue of more than one
//! song, would have one download overwrite the audio another track is reading,
//! and the song that was meant to play would be heard as the one before it.
//! Because the name is derived from the video, a file already at that path is
//! that song's audio, so it can be reused instead of downloaded again.

use crate::queue::Source;
use crate::timing::Timing;
use anyhow::{Context, bail};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, RANGE};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

/// Audio formats to try, best first.
///
/// `acodec^=mp4a` is the AAC audio YouTube serves inside an MP4 container, and
/// `ext=m4a` catches the same thing on videos that label it differently. The
/// last entry takes any audio-only stream that is not Opus, which covers formats
/// added after this was written.
pub const DECODABLE_FORMAT: &str = "bestaudio[acodec^=mp4a][vcodec=none]/\
     bestaudio[ext=m4a]/\
     bestaudio[acodec!=opus][vcodec=none]";

/// The selector for the fallback download, which is then transcoded.
const BEST_FORMAT: &str = "bestaudio";

/// How many downloaded files to keep per guild, beyond the ones still in use.
///
/// Each song is kept while it is playing or queued, and the newest few survive
/// afterwards so a repeated request does not download the same audio twice.
/// Old ones are pruned, because the songs the listener asks for are never the
/// same twice.
///
/// This is deliberately a *retention* rule, not a cache bound: it used to keep
/// the newest eight files outright, which is fewer than [`crate::queue::MAX_QUEUE`],
/// so with nine songs queued a ninth download deleted the audio of songs that
/// were about to play.
const KEEP_FILES: usize = 8;

/// Directory holding one guild's downloaded audio.
///
/// Under the user's cache directory (`$XDG_CACHE_HOME`, else `~/.cache`) rather
/// than `/tmp`, which many systems clear on every boot, so a song asked for
/// again after a restart is still instant. Falls back to the temp directory
/// when there is no home to put it in.
pub fn cache_dir(guild_id: u64) -> PathBuf {
    cache_root().join(format!("shamash-{guild_id}"))
}

fn cache_root() -> PathBuf {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    if let Some(xdg) = var("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("shamash");
    }
    if let Some(home) = var("HOME") {
        return PathBuf::from(home).join(".cache").join("shamash");
    }
    std::env::temp_dir()
}

/// Tries to delete a file, logging rather than failing when it cannot.
///
/// None of these deletions decide whether playback succeeds, so an error is
/// only worth a warning. It is logged rather than ignored because a cache that
/// cannot prune itself grows without bound, and a temporary file that cannot
/// be cleaned up accumulates in the working directory.
async fn remove_file(path: &Path, what: &str) {
    if let Err(e) = tokio::fs::remove_file(path).await {
        tracing::warn!("could not remove {what} {}: {e}", path.display());
    }
}

/// Downloads decodable audio for `video_id` into `dir` and returns where to
/// play it from.
///
/// `video_id` names the file, so a song already downloaded is reused. Otherwise
/// one yt-dlp extraction says what this video publishes: a format symphonia can
/// decode comes back as [`Source::Stream`] and plays while it arrives, and a
/// video with nothing decodable is downloaded and transcoded as before.
///
/// `in_use` names the songs that are playing or queued. Their audio is never
/// deleted, whatever the retention rule says.
pub async fn fetch(
    video_id: &str,
    url: &str,
    dir: &Path,
    in_use: &[PathBuf],
    timing: &Timing,
) -> anyhow::Result<Source> {
    let stem = file_stem(video_id);
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("could not create {}", dir.display()))?;

    if let Some(cached) = existing(&stem, dir).await {
        timing.record("fetch.cache_hit", Duration::ZERO);
        return Ok(Source::File(cached));
    }

    let stream = match extract(url, timing).await {
        Ok(json) => pick_streamable(&json),
        // No extraction is not a failure worth reporting: it only means this
        // song takes the download path, which is the behaviour that predates
        // streaming and works for every format.
        Err(e) => {
            tracing::debug!("could not extract {url}, so it will be downloaded: {e:#}");
            None
        }
    };
    let Some(stream) = stream else {
        let path = download_to_cache(url, dir, &stem, in_use, timing).await?;
        return Ok(Source::File(path));
    };

    Ok(Source::Stream {
        url: stream.url,
        headers: stream.headers,
        cache: dir.join(format!("{stem}.{}", stream.ext)),
        length: stream.length,
    })
}

/// What one extraction found for a video.
pub struct Streamable {
    /// The signed format URL, valid for hours rather than forever.
    pub url: String,
    /// The headers that URL must be requested with.
    pub headers: HeaderMap,
    /// The container, which is the cache file's extension.
    pub ext: String,
    /// How many bytes the format is, in bytes.
    ///
    /// Not a nicety: YouTube refuses the URL outright unless the request
    /// carries a *closed* `range`, `bytes=0-<length-1>`. An open-ended
    /// `bytes=0-` and a plain GET both answer 403, and the length is the only
    /// way to close it. Songbird takes the same number for the same reason.
    pub length: u64,
}

impl Streamable {
    /// The stream a queued song carries, if it is playing from one.
    ///
    /// The container and the length live in different fields of the queued
    /// source -- one names the cache file, the other closes the range -- so this
    /// is the one place that puts them back together.
    pub fn of(source: &Source) -> Option<Self> {
        let Source::Stream {
            url,
            headers,
            cache,
            length,
        } = source
        else {
            return None;
        };
        Some(Self {
            url: url.clone(),
            headers: headers.clone(),
            ext: cache
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or_default()
                .to_string(),
            length: *length,
        })
    }
}

/// The browser User-Agent YouTube's own player sends.
///
/// A media URL is signed against the client that asked for it, so a direct GET
/// that announces yt-dlp is the request that gets a 403.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// One yt-dlp extraction, and the JSON it printed.
///
/// A single run for the whole decision: this used to be a download that
/// resolved the format and moved the bytes in one subprocess, which cannot be
/// reused because the URL only exists inside it.
async fn extract(url: &str, timing: &Timing) -> anyhow::Result<serde_json::Value> {
    let mut command = Command::new("yt-dlp");
    command
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .args([
            "--dump-single-json",
            "--no-playlist",
            "--no-warnings",
            "--skip-download",
        ])
        .arg(url);

    let started = std::time::Instant::now();
    let output = run(
        command,
        &format!("yt-dlp extraction of {url}"),
        EXTRACT_TIMEOUT,
    )
    .await
    .with_context(|| format!("could not run yt-dlp to extract {url}"))?;
    timing.record("fetch.extract", started.elapsed());

    anyhow::ensure!(
        output.status.success(),
        "yt-dlp could not extract {url}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    serde_json::from_slice(&output.stdout).context("yt-dlp printed no usable json")
}

/// The best audio format symphonia can decode, with the headers it needs.
///
/// The preference order is [`DECODABLE_FORMAT`]'s, so the format played is the
/// one that would have been downloaded: AAC in MP4 first, then any m4a, then
/// any audio-only format that is not Opus. Ties go to the larger bitrate, which
/// is what `bestaudio` does.
fn pick_streamable(json: &serde_json::Value) -> Option<Streamable> {
    use serde_json::Value;

    /// 0 for the preferred codecs, 1 for anything else decodable, `None` for
    /// what symphonia cannot read.
    fn rank(format: &Value) -> Option<u8> {
        let acodec = format.get("acodec")?.as_str().unwrap_or("none");
        let ext = format.get("ext")?.as_str().unwrap_or_default();
        if acodec == "none" {
            return None;
        }
        if acodec.starts_with("mp4a") || ext == "m4a" {
            return Some(0);
        }
        if acodec != "opus" {
            return Some(1);
        }
        None
    }

    json.get("formats")?
        .as_array()?
        .iter()
        .filter_map(|format| {
            Some((
                rank(format)?,
                -format
                    .get("abr")
                    .or_else(|| format.get("tbr"))
                    .and_then(Value::as_f64)
                    .unwrap_or_default(),
                Streamable {
                    url: format.get("url")?.as_str()?.to_string(),
                    headers: header_map(format.get("http_headers")?),
                    ext: format.get("ext")?.as_str()?.to_string(),
                    length: length_of(format)?,
                },
            ))
        })
        .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)))
        .map(|(_, _, picked)| picked)
}

/// How long a format is, which the request cannot be made without.
///
/// `filesize_approx` is the one to fall back on: some extractors report an
/// estimate for an audio stream whose exact size was never declared, and a
/// range that overshoots the end is still a range YouTube honours.
fn length_of(format: &serde_json::Value) -> Option<u64> {
    format
        .get("filesize")
        .or_else(|| format.get("filesize_approx"))
        .and_then(serde_json::Value::as_u64)
        .filter(|length| *length > 0)
}

/// The headers a format URL was signed with, ready to replay.
///
/// Anything that is not a valid header is dropped rather than failing the whole
/// extraction: a format whose headers cannot be sent is not a format worth
/// streaming.
fn header_map(headers: &serde_json::Value) -> HeaderMap {
    headers
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, value)| {
            Some((
                HeaderName::try_from(name.as_str()).ok()?,
                HeaderValue::from_str(value.as_str()?).ok()?,
            ))
        })
        .collect()
}

/// How long a stream has to prove itself before the file is used instead.
///
/// Bounded because the alternative is silence: a song that starts and then
/// stops is worse than one that starts a moment late. Two and a half seconds is
/// far longer than a healthy connection needs for the first packet, so a
/// genuine stall is what gets caught rather than a slow moment.
pub const STREAM_BUDGET: Duration = Duration::from_millis(2500);

/// The `range` a stream's URL has to be asked for.
///
/// A *closed* range, and the whole file: YouTube serves this URL for that and
/// nothing else, so a check that asked for less would pass a URL that playback
/// then gets a 403 for. It costs one discarded response body and saves a song
/// that starts and then dies.
fn whole_stream(length: u64) -> String {
    format!("bytes=0-{}", length.saturating_sub(1))
}

/// Decides how a queued song actually plays: the stream if its URL still
/// resolves, the downloaded file if it does not.
///
/// The URL is signed and expires, so a song queued twenty minutes ago can be
/// refused with a 403, and a connection that never delivers is a stall rather
/// than an error. Both have to be caught before playback starts, which is what
/// the request below is for: it asks for the same range playback will ask for,
/// refuses a non-success status, and gives up at [`STREAM_BUDGET`].
pub async fn resolve(
    source: &Source,
    watch_url: &str,
    in_use: &[PathBuf],
    timing: &Timing,
) -> anyhow::Result<Source> {
    let Some(stream) = Streamable::of(source) else {
        return Ok(source.clone());
    };
    let Source::Stream { cache, .. } = source else {
        unreachable!("a stream always carries a cache path")
    };

    if stream_usable(&stream, timing).await {
        return Ok(source.clone());
    }
    tracing::info!("the stream for {watch_url} is unusable, so the file is used instead");
    let path = save(&stream, cache, watch_url, in_use, timing).await?;
    Ok(Source::File(path))
}

/// Whether a stream's URL still serves audio, inside [`STREAM_BUDGET`].
pub async fn stream_usable(stream: &Streamable, timing: &Timing) -> bool {
    usable_within(stream, STREAM_BUDGET, timing).await
}

/// [`stream_usable`] with the deadline written out, so the stall test can use a
/// short one instead of waiting the real budget out.
async fn usable_within(stream: &Streamable, budget: Duration, timing: &Timing) -> bool {
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(budget, async {
        let mut response = client()
            .get(&stream.url)
            .headers(stream.headers.clone())
            .header(RANGE, whole_stream(stream.length))
            .send()
            .await
            .with_context(|| format!("the stream for {} did not answer", stream.url))?;
        anyhow::ensure!(
            response.status().is_success(),
            "the stream for {} answered {}",
            stream.url,
            response.status()
        );
        // Reading one chunk is what separates "answered" from "delivering": a
        // connection can be established and then say nothing at all.
        response
            .chunk()
            .await
            .context("the stream ended before its first bytes")?
            .context("the stream sent no bytes")?;
        anyhow::Ok(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            timing.record("fetch.stream_probe", started.elapsed());
            true
        }
        Ok(Err(e)) => {
            tracing::debug!("cannot stream this audio: {e:#}");
            timing.record("fetch.stream_probe_failed", started.elapsed());
            false
        }
        Err(_) => {
            tracing::debug!("the stream stalled for longer than {budget:?}");
            timing.record("fetch.stream_probe_failed", budget);
            false
        }
    }
}

/// Puts a song's audio in the cache file, ready to be played or reused.
///
/// Runs twice: in the background while a stream plays, and again when a stream
/// turns out to be unusable. The direct URL is tried first because it is one
/// GET of bytes yt-dlp would otherwise download for us; yt-dlp is the fallback,
/// so a URL that has expired between queueing and playing still resolves into a
/// file.
pub async fn save(
    stream: &Streamable,
    cache: &Path,
    watch_url: &str,
    in_use: &[PathBuf],
    timing: &Timing,
) -> anyhow::Result<PathBuf> {
    if let Some(there) = existing_at(cache).await {
        return Ok(there);
    }
    // Written beside the target and renamed into place, so a reader never sees
    // a half-written file and `existing` never mistakes one for audio.
    let part = cache.with_extension("part");
    match fetch_to_file(stream, &part).await {
        Ok(()) => {
            tokio::fs::rename(&part, cache)
                .await
                .with_context(|| format!("could not rename {}", part.display()))?;
            prune(cache.parent().unwrap_or(Path::new(".")), in_use).await;
            Ok(cache.to_path_buf())
        }
        Err(e) => {
            tracing::debug!("could not save the stream for {watch_url}: {e:#}");
            let _ = tokio::fs::remove_file(&part).await;
            // The cache path is `{dir}/{stem}.{ext}`, so the two yt-dlp wants to
            // write to are recoverable from it.
            let dir = cache.parent().context("the cache path has no directory")?;
            let stem = cache
                .file_stem()
                .and_then(|stem| stem.to_str())
                .context("the cache path has no file stem")?;
            download_to_cache(watch_url, dir, stem, in_use, timing).await
        }
    }
}

/// Writes the stream's audio to `dest`.
///
/// A whole-file read rather than a streamed copy: these are a few megabytes and
/// the buffered body is one allocation instead of a read loop that has to be
/// correct about partial writes.
async fn fetch_to_file(stream: &Streamable, dest: &Path) -> anyhow::Result<()> {
    let response = client()
        .get(&stream.url)
        .headers(stream.headers.clone())
        .header(RANGE, whole_stream(stream.length))
        // Covers the body too: a stalled transfer must fall back to yt-dlp, not
        // hold up the request (or the background cache task) forever.
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("could not GET {}", stream.url))?;
    anyhow::ensure!(
        response.status().is_success(),
        "{} answered {}",
        stream.url,
        response.status()
    );
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("could not read {}", stream.url))?;
    anyhow::ensure!(!bytes.is_empty(), "{} sent no audio", stream.url);
    tokio::fs::write(dest, &bytes)
        .await
        .with_context(|| format!("could not write {}", dest.display()))
}

/// The http client every direct request goes through.
///
/// `rustls-tls` is already on: the project depends on reqwest with it, so the
/// media URLs get the same TLS stack as the rest of the bot rather than pulling
/// in a second one.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .unwrap_or_default()
}

/// The audio at `path`, if there is any worth reading.
async fn existing_at(path: &Path) -> Option<PathBuf> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    (metadata.len() > 0).then(|| path.to_path_buf())
}

/// Downloads decodable audio through yt-dlp, transcoding if it must, and
/// prunes the cache.
///
/// The path streaming cannot cover, and the fallback inside [`save`]: a video
/// with no decodable format, or a URL that has stopped working.
pub async fn download_to_cache(
    url: &str,
    dir: &Path,
    stem: &str,
    in_use: &[PathBuf],
    timing: &Timing,
) -> anyhow::Result<PathBuf> {
    let path = match download(url, dir, stem, DECODABLE_FORMAT, timing, "ytdlp").await {
        Ok(path) => path,
        Err(no_decodable) => {
            // Recorded as its own stage because it is the expensive failure
            // mode: a video with no AAC stream costs two extra yt-dlp runs and
            // an ffmpeg pass, and the rate of it is the only thing that says
            // whether DECODABLE_FORMAT is still the right selector.
            tracing::debug!("no decodable stream for {url}: {no_decodable:#}");
            let source = download(url, dir, stem, BEST_FORMAT, timing, "ytdlp_fallback")
                .await
                .context("yt-dlp could not download the audio")?;
            let transcoded = dir.join(format!("{stem}.wav"));
            timing
                .stage("fetch.transcode", transcode(&source, &transcoded))
                .await?;
            remove_file(&source, "the intermediate download").await;
            transcoded
        }
    };

    prune(dir, in_use).await;
    Ok(path)
}

/// A file name safe to build from a video id.
///
/// Video ids are URL-safe already, but the id reaches this from a search
/// result, so anything unexpected is replaced rather than trusted as a path.
fn file_stem(video_id: &str) -> String {
    let cleaned: String = video_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if cleaned.is_empty() {
        "track".to_string()
    } else {
        cleaned
    }
}

/// The audio already downloaded for this song, if there is any.
///
/// A `.part` file is ignored: it is a download that never finished, so it is
/// not audio that can be played.
async fn existing(stem: &str, dir: &Path) -> Option<PathBuf> {
    let mut entries = tokio::fs::read_dir(dir).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "part") {
            continue;
        }
        if path.file_stem().and_then(|s| s.to_str()) == Some(stem)
            && entry
                .metadata()
                .await
                .ok()
                .is_some_and(|meta| meta.len() > 0)
        {
            return Some(path);
        }
    }
    None
}

/// The yt-dlp arguments used for every download.
///
/// `--force-overwrites` matters when a previous download of this song left a
/// file behind that was too small to be usable. yt-dlp's default is
/// `--no-force-overwrites`, and that skips a download whose output already
/// exists, printing the path and exiting 0, so the caller could not tell a
/// fresh download from a skipped one.
///
/// `--no-part` is deliberately absent. With the default `.part` file the new
/// audio is renamed into place, so a track reading a file of the same name
/// keeps the old inode and never sees a half-written file.
fn download_args(url: &str, dir: &Path, stem: &str, format: &str) -> Vec<OsString> {
    let output = dir.join(format!("{stem}.%(ext)s"));
    [
        "--no-playlist",
        "--no-warnings",
        "--no-simulate",
        "--force-overwrites",
    ]
    .iter()
    .map(OsString::from)
    .chain([
        OsString::from("--format"),
        OsString::from(format),
        OsString::from("--output"),
        output.into_os_string(),
        // The marker comes first in the argument list but last in the output:
        // yt-dlp resolves the format and only then starts transferring bytes,
        // so this line separates the metadata cost from the transfer cost of
        // one subprocess. Without it the two are indistinguishable from inside.
        OsString::from("--print"),
        OsString::from(format!("before_dl:{BEFORE_DOWNLOAD_MARKER}")),
        OsString::from("--print"),
        OsString::from("after_move:filepath"),
        OsString::from(url),
    ])
    .collect()
}

/// What yt-dlp prints on stdout once the format is resolved and bytes are about
/// to flow. A literal, so it cannot be confused with the file path the other
/// `--print` emits.
const BEFORE_DOWNLOAD_MARKER: &str = "shamash-formats-resolved";

/// How long one yt-dlp download may take before it is treated as failed.
///
/// A few minutes for a slow connection on a long track, and bounded regardless:
/// the download runs while a request is in flight, and an unbounded yt-dlp would
/// leave the request unresolved rather than reporting a failure the listener
/// could hear about.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(90);

/// How long one yt-dlp extraction may take.
///
/// Shorter than a download because it moves no bytes, and a speaker waiting for
/// a song is waiting on it: this is the whole cost before playback starts on
/// the streaming path.
pub const EXTRACT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long one ffmpeg transcode may take.
///
/// Shorter than the download because it is only a CPU-bound local rewrite of a
/// file that has already arrived, so ninety seconds of that means something is
/// wrong rather than slow.
pub const TRANSCODE_TIMEOUT: Duration = Duration::from_secs(60);

/// Runs `command` to completion under a deadline, killing it if it overruns.
///
/// Every subprocess this crate spawns goes through here. Two properties matter
/// and neither comes from the timeout alone: `kill_on_drop` so a cancelled or
/// timed-out call does not leave the child running with the guild's ordering
/// still held, and a named command in the error so a stalled yt-dlp can be told
/// from a stalled ffmpeg in the log.
async fn run(
    mut command: Command,
    what: &str,
    budget: Duration,
) -> anyhow::Result<std::process::Output> {
    let output = tokio::time::timeout(budget, command.output())
        .await
        .map_err(|_| anyhow::anyhow!("{what} took longer than {budget:?}"))??;
    Ok(output)
}

/// Downloads one format into `dir` and returns the path yt-dlp wrote.
///
/// `stage` prefixes the recorded timings, so the fallback download is
/// distinguishable from the first attempt in the request's summary line.
async fn download(
    url: &str,
    dir: &Path,
    stem: &str,
    format: &str,
    timing: &Timing,
    stage: &str,
) -> anyhow::Result<PathBuf> {
    let started = std::time::Instant::now();
    let mut command = Command::new("yt-dlp");
    command
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .args(download_args(url, dir, stem, format));
    let mut child = command
        .spawn()
        .with_context(|| format!("could not run yt-dlp to fetch {url}"))?;

    let stderr = child.stderr.take().context("yt-dlp stderr was not piped")?;
    // Collected on its own task so a chatty stderr cannot fill its pipe and
    // wedge yt-dlp, and so the failure message still has something to say.
    let complaints = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = BufReader::new(stderr).read_to_end(&mut buf).await;
        buf
    });

    let stdout = child.stdout.take().context("yt-dlp stdout was not piped")?;
    let mut lines = BufReader::new(stdout).lines();
    let deadline = tokio::time::Instant::now() + DOWNLOAD_TIMEOUT;
    let mut downloaded: Vec<PathBuf> = Vec::new();
    let mut transfer_started: Option<std::time::Instant> = None;
    let mut resolved = Duration::ZERO;
    let mut transferred = Duration::ZERO;
    loop {
        let next = tokio::time::timeout_at(deadline, lines.next_line())
            .await
            .map_err(|_| {
                anyhow::anyhow!("yt-dlp download of {url} took longer than {DOWNLOAD_TIMEOUT:?}")
            })?;
        let Some(line) = next.context("reading what yt-dlp reported")? else {
            break;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == BEFORE_DOWNLOAD_MARKER {
            resolved = started.elapsed();
            transfer_started = Some(std::time::Instant::now());
        } else {
            transferred += transfer_started
                .take()
                .map_or(Duration::ZERO, |at| at.elapsed());
            downloaded.push(PathBuf::from(line));
        }
    }

    let status = tokio::time::timeout_at(deadline, child.wait())
        .await
        .map_err(|_| {
            anyhow::anyhow!("yt-dlp download of {url} took longer than {DOWNLOAD_TIMEOUT:?}")
        })??;
    timing.record(&format!("{stage}.extract"), resolved);
    timing.record(&format!("{stage}.download"), transferred);

    anyhow::ensure!(
        status.success(),
        "yt-dlp failed for {url}: {}",
        String::from_utf8_lossy(&complaints.await.unwrap_or_default()).trim()
    );

    // `--print` emits the final path, one per line; later lines win so a
    // resumed or retried download reports the file that is actually in place.
    let path = downloaded
        .pop()
        .context("yt-dlp reported no downloaded file")?;

    let metadata = tokio::fs::metadata(&path)
        .await
        .with_context(|| format!("yt-dlp reported {} but it is not there", path.display()))?;
    anyhow::ensure!(
        metadata.len() > 0,
        "yt-dlp downloaded an empty {}",
        path.display()
    );

    Ok(path)
}

/// Rewrites `source` as 16-bit PCM WAV at `dest`.
///
/// Songbird resamples to its own 48 kHz stereo output, so the sample rate and
/// channel count are left as they are and only the codec is replaced.
async fn transcode(source: &Path, dest: &Path) -> anyhow::Result<()> {
    let mut command = Command::new("ffmpeg");
    command
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .args(["-nostdin", "-loglevel", "error", "-y"])
        .arg("-i")
        .arg(source)
        .args(["-c:a", "pcm_s16le"])
        .arg(dest);
    let output = run(
        command,
        &format!("ffmpeg transcode of {}", source.display()),
        TRANSCODE_TIMEOUT,
    )
    .await
    .context("could not run ffmpeg; is it installed and on PATH?")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        bail!(
            "ffmpeg failed to transcode {}: {}",
            source.display(),
            stderr.trim()
        );
    }
    if stderr.contains("Output file is empty") {
        bail!("ffmpeg produced no audio from {}", source.display());
    }

    Ok(())
}

/// Deletes downloads that are neither in use nor among the newest
/// [`KEEP_FILES`].
///
/// `in_use` is the audio of every song playing or waiting in the queue. Those
/// are never deleted, whatever their age: keeping "the newest eight files"
/// outright is fewer than a full queue, so with nine songs queued a ninth
/// download used to delete the audio of a song about to play.
async fn prune(dir: &Path, in_use: &[PathBuf]) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "part") {
            continue;
        }
        if in_use
            .iter()
            .any(|kept| kept.file_name() == path.file_name())
        {
            continue;
        }
        let Ok(modified) = entry.metadata().await.and_then(|meta| meta.modified()) else {
            continue;
        };
        files.push((modified, path));
    }
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in files.into_iter().skip(KEEP_FILES) {
        let _ = tokio::fs::remove_file(&path).await;
    }
}

#[cfg(test)]
#[path = "tests/fetch.rs"]
mod tests;
