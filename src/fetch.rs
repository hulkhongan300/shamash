//! Downloads a YouTube video's audio, or hands back the URL to play it from.
//!
//! Songbird's `YoutubeDl` forces Opus, which Symphonia 0.5 cannot decode, so we
//! pick AAC/MP4 ourselves and stream it while caching in the background; download
//! and transcode is the fallback. One file per video id, so tracks never overwrite
//! each other and a cached file can be reused.

use crate::queue::Source;
use crate::timing::Timing;
use anyhow::{Context, bail};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, RANGE};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

/// Audio formats to try, best first: AAC in MP4, any m4a, then any non-Opus audio.
pub const DECODABLE_FORMAT: &str = "bestaudio[acodec^=mp4a][vcodec=none]/\
     bestaudio[ext=m4a]/\
     bestaudio[acodec!=opus][vcodec=none]";

/// The selector for the fallback download, which is then transcoded.
const BEST_FORMAT: &str = "bestaudio";

/// Downloaded files kept per guild beyond those in use; a retention rule, not a cache bound.
const KEEP_FILES: usize = 8;

/// Directory holding one guild's downloaded audio; under the cache dir so it survives reboots.
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

/// Tries to delete a file, warning on failure so unbounded cache growth is visible.
async fn remove_file(path: &Path, what: &str) {
    if let Err(e) = tokio::fs::remove_file(path).await {
        tracing::warn!("could not remove {what} {}: {e}", path.display());
    }
}

/// Returns where to play `video_id` from: cached file, [`Source::Stream`], or a fresh download.
/// Songs in `in_use` are never pruned.
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
        // No extraction just means the download path.
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
    /// The signed format URL; expires after hours.
    pub url: String,
    /// The headers that URL must be requested with.
    pub headers: HeaderMap,
    /// The container, which is the cache file's extension.
    pub ext: String,
    /// Size in bytes; YouTube 403s anything but a closed `range`, which needs it.
    pub length: u64,
}

impl Streamable {
    /// The stream a queued song carries, if it is playing from one.
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

/// The browser User-Agent YouTube's player sends; announcing yt-dlp gets a 403.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// One yt-dlp extraction, and the JSON it printed.
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

/// The best decodable format, in [`DECODABLE_FORMAT`]'s order; ties go to bitrate.
fn pick_streamable(json: &serde_json::Value) -> Option<Streamable> {
    use serde_json::Value;

    /// 0 preferred, 1 other decodable, `None` undecodable.
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

/// A format's byte length; `filesize_approx` is fine since YouTube honours an overshooting range.
fn length_of(format: &serde_json::Value) -> Option<u64> {
    format
        .get("filesize")
        .or_else(|| format.get("filesize_approx"))
        .and_then(serde_json::Value::as_u64)
        .filter(|length| *length > 0)
}

/// The headers a format URL was signed with; invalid ones are dropped.
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

/// How long a stream has to deliver before the file is used instead.
pub const STREAM_BUDGET: Duration = Duration::from_millis(2500);

/// The closed whole-file `range` YouTube requires; probing with less would miss 403s.
fn whole_stream(length: u64) -> String {
    format!("bytes=0-{}", length.saturating_sub(1))
}

/// Plays the stream if its URL still serves (it may have expired or stall), else the file.
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

/// [`stream_usable`] with an explicit deadline, for tests.
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
        // A connection can answer and then send nothing; require a chunk.
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

/// Puts a song's audio in the cache file: direct URL first, yt-dlp if it has expired.
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
    // Write then rename, so nobody sees a half-written file.
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
            // The cache path is `{dir}/{stem}.{ext}`.
            let dir = cache.parent().context("the cache path has no directory")?;
            let stem = cache
                .file_stem()
                .and_then(|stem| stem.to_str())
                .context("the cache path has no file stem")?;
            download_to_cache(watch_url, dir, stem, in_use, timing).await
        }
    }
}

/// Writes the stream's audio to `dest`; whole-body read, since files are a few MB.
async fn fetch_to_file(stream: &Streamable, dest: &Path) -> anyhow::Result<()> {
    let response = client()
        .get(&stream.url)
        .headers(stream.headers.clone())
        .header(RANGE, whole_stream(stream.length))
        // Covers the body too, so a stalled transfer falls back to yt-dlp.
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

/// Downloads via yt-dlp (transcoding if needed) and prunes; the fallback when streaming can't work.
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
            // Its own stage: the expensive path, and its rate tells whether DECODABLE_FORMAT still fits.
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

/// A file name safe to build from a video id, which comes from untrusted search results.
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

/// The audio already downloaded for this song, ignoring unfinished `.part` files.
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
/// `--force-overwrites` so a stale file is not silently "skipped"; no `--no-part`,
/// so a playing track keeps its old inode.
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
        // Printed between format resolution and transfer, to time them separately.
        OsString::from("--print"),
        OsString::from(format!("before_dl:{BEFORE_DOWNLOAD_MARKER}")),
        OsString::from("--print"),
        OsString::from("after_move:filepath"),
        OsString::from(url),
    ])
    .collect()
}

/// What yt-dlp prints once the format is resolved and bytes are about to flow.
const BEFORE_DOWNLOAD_MARKER: &str = "shamash-formats-resolved";

/// How long one yt-dlp download may take; bounded so a request always resolves.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(90);

/// How long one yt-dlp extraction may take; short because it moves no bytes.
pub const EXTRACT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long one ffmpeg transcode may take (local and CPU-bound).
pub const TRANSCODE_TIMEOUT: Duration = Duration::from_secs(60);

/// Runs `command` under a deadline, killing it on overrun or cancellation.
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
    // Drained on its own task so a full pipe cannot wedge yt-dlp.
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

    // Last printed path wins (retries print again).
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

/// Rewrites `source` as 16-bit PCM WAV at `dest`; songbird resamples it later.
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

/// Deletes downloads that are neither in `in_use` nor among the newest [`KEEP_FILES`].
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
