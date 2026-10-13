//! Downloads a YouTube video's audio to a local file the decoder can read.
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
//! Every song gets its own file, named after its video id. A single shared
//! path cannot work: two commands handled at once, or a queue of more than one
//! song, would have one download overwrite the audio another track is reading,
//! and the song that was meant to play would be heard as the one before it.
//! Because the name is derived from the video, a file already at that path is
//! that song's audio, so it can be reused instead of downloaded again.

use anyhow::{Context, bail};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;
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
pub fn cache_dir(guild_id: u64) -> PathBuf {
    std::env::temp_dir().join(format!("shamash-{guild_id}"))
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

/// Downloads decodable audio for `video_id` into `dir` and returns the file to
/// play.
///
/// `video_id` names the file, so a song already downloaded is reused. The file
/// is either the stream YouTube publishes in a decodable format, or a WAV
/// transcoded from the best stream it offers.
///
/// `in_use` names the songs that are playing or queued. Their audio is never
/// deleted, whatever the retention rule says.
pub async fn fetch(
    video_id: &str,
    url: &str,
    dir: &Path,
    in_use: &[PathBuf],
) -> anyhow::Result<PathBuf> {
    let stem = file_stem(video_id);
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("could not create {}", dir.display()))?;

    if let Some(cached) = existing(&stem, dir).await {
        return Ok(cached);
    }

    let path = match download(url, dir, &stem, DECODABLE_FORMAT).await {
        Ok(path) => path,
        Err(no_decodable) => {
            tracing::debug!("no decodable stream for {url}: {no_decodable:#}");
            let source = download(url, dir, &stem, BEST_FORMAT)
                .await
                .context("yt-dlp could not download the audio")?;
            let transcoded = dir.join(format!("{stem}.wav"));
            transcode(&source, &transcoded).await?;
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
        OsString::from("--print"),
        OsString::from("after_move:filepath"),
        OsString::from(url),
    ])
    .collect()
}

/// How long one yt-dlp download may take before it is treated as failed.
///
/// A few minutes for a slow connection on a long track, and bounded regardless:
/// the download runs while a request is in flight, and an unbounded yt-dlp would
/// leave the request unresolved rather than reporting a failure the listener
/// could hear about.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(90);

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
async fn download(url: &str, dir: &Path, stem: &str, format: &str) -> anyhow::Result<PathBuf> {
    let mut command = Command::new("yt-dlp");
    command
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .args(download_args(url, dir, stem, format));
    let output = run(
        command,
        &format!("yt-dlp download of {url}"),
        DOWNLOAD_TIMEOUT,
    )
    .await
    .with_context(|| format!("could not run yt-dlp to fetch {url}"))?;

    anyhow::ensure!(
        output.status.success(),
        "yt-dlp failed for {url}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );

    // `--print` emits the final path, one per line; later lines win so a
    // resumed or retried download reports the file that is actually in place.
    let path = String::from_utf8_lossy(&output.stdout)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(PathBuf::from)
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
mod tests {
    use super::*;

    #[test]
    fn decodable_format_avoids_opus_and_prefers_aac() {
        // Opus has no symphonia decoder, so no branch of the selector may
        // resolve to it.
        assert!(DECODABLE_FORMAT.contains("acodec!=opus"));
        assert!(!DECODABLE_FORMAT.contains("[acodec=opus]"));
        assert!(DECODABLE_FORMAT.starts_with("bestaudio[acodec^=mp4a]"));
        // Every branch must be audio-only: a video stream would be silent.
        for branch in DECODABLE_FORMAT.split('/') {
            assert!(
                branch.contains("bestaudio"),
                "selector branch is not audio-only: {branch}"
            );
        }
    }

    #[test]
    fn each_song_downloads_to_its_own_file() {
        // The bug this prevents: one shared path per guild, so a second
        // request overwrote the audio a track was reading and the song that
        // was asked for was heard as the one before it.
        let one = download_args(
            "https://youtu.be/abc",
            Path::new("/tmp/shamash-1"),
            "abc",
            DECODABLE_FORMAT,
        );
        let two = download_args(
            "https://youtu.be/xyz",
            Path::new("/tmp/shamash-1"),
            "xyz",
            DECODABLE_FORMAT,
        );
        let output_of = |args: &[OsString]| {
            args.iter()
                .find(|a| a.to_string_lossy().contains("%(ext)s"))
                .map(|a| a.to_string_lossy().into_owned())
                .expect("an output path")
        };
        assert_ne!(
            output_of(&one),
            output_of(&two),
            "two songs must not share an output path"
        );
        assert!(output_of(&one).ends_with("abc.%(ext)s"));
    }

    #[test]
    fn each_download_forces_a_real_one() {
        // yt-dlp's default skips a download whose output file already exists,
        // printing the path and exiting 0, so a stale file too small to play
        // would be handed to the player as if it were fresh audio.
        let args = download_args(
            "https://youtu.be/abc",
            Path::new("/tmp/shamash-1"),
            "abc",
            DECODABLE_FORMAT,
        );
        let args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.iter().any(|a| a == "--force-overwrites"),
            "a stale file would be replayed: {args:?}"
        );
        // Writing straight to the output would truncate the file a playing
        // track is reading, so the default `.part` file must be left alone.
        assert!(
            !args.iter().any(|a| a == "--no-part"),
            "the output must be renamed into place, not written in place: {args:?}"
        );
        assert_eq!(args.last().unwrap(), "https://youtu.be/abc");
    }

    #[test]
    fn each_guild_gets_its_own_directory() {
        assert_ne!(cache_dir(1), cache_dir(2));
        assert!(cache_dir(42).ends_with("shamash-42"));
    }

    #[test]
    fn a_video_id_cannot_escape_the_cache_directory() {
        // The id comes from a search result, so it must not be able to name a
        // path outside the cache.
        for id in ["../../etc/passwd", "a/b", "..", "with space"] {
            let stem = file_stem(id);
            assert!(
                !stem.contains('/') && !stem.contains('\\'),
                "id {id} produced a path: {stem}"
            );
            assert!(!stem.contains(".."), "id {id} produced {stem}");
        }
        assert_eq!(file_stem(""), "track");
        assert_eq!(file_stem("dQw4w9WgXcQ"), "dQw4w9WgXcQ");
    }

    #[tokio::test]
    async fn audio_already_downloaded_is_reused() {
        // A file named after the video is that song's audio, so asking for
        // the same song again must not download it a second time.
        let dir = std::env::temp_dir().join("shamash-existing-test");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.expect("create dir");
        tokio::fs::write(dir.join("abc.m4a"), b"audio")
            .await
            .expect("write");

        assert_eq!(existing("abc", &dir).await, Some(dir.join("abc.m4a")));
        assert_eq!(existing("other", &dir).await, None);
    }

    #[tokio::test]
    async fn a_half_downloaded_file_is_not_reused() {
        // A `.part` file is a download that never finished. Reusing it would
        // play silence or a fragment.
        let dir = std::env::temp_dir().join("shamash-part-test");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.expect("create dir");
        tokio::fs::write(dir.join("abc.m4a.part"), b"half")
            .await
            .expect("write");

        assert_eq!(existing("abc", &dir).await, None);
    }

    #[tokio::test]
    async fn an_empty_file_is_not_reused() {
        let dir = std::env::temp_dir().join("shamash-empty-test");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.expect("create dir");
        tokio::fs::write(dir.join("abc.m4a"), b"")
            .await
            .expect("write");

        assert_eq!(existing("abc", &dir).await, None);
    }

    #[tokio::test]
    async fn a_subprocess_that_never_finishes_is_killed_at_the_deadline() {
        // The point of the deadlines: `Player::play` holds the guild's request
        // ordering while a fetch runs, so a subprocess that hangs forever must
        // become an error the listener can be told about rather than a request
        // that never completes.
        let budget = Duration::from_millis(200);
        let mut command = Command::new("sh");
        command.kill_on_drop(true).arg("-c").arg("sleep 30");
        let started = std::time::Instant::now();
        let err = run(command, "a sleeping test process", budget)
            .await
            .expect_err("the sleep outlives the budget");
        assert!(
            err.to_string().contains("longer than"),
            "the error must say it timed out: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deadline did not apply"
        );
    }

    /// A command that finishes inside the budget is not disturbed by the
    /// wrapper, or every normal fetch would report a timeout.
    #[tokio::test]
    async fn a_subprocess_that_finishes_in_time_is_not_disturbed() {
        let mut command = Command::new("sh");
        command.kill_on_drop(true).arg("-c").arg("exit 0");
        let output = run(command, "a test process", Duration::from_secs(30))
            .await
            .expect("a quick process must be allowed through");
        assert!(output.status.success());
    }

    /// The durations are the documented contract for how long a spoken command
    /// can wait on a subprocess, so they are pinned here rather than only in
    /// the README.
    #[test]
    fn every_subprocess_has_a_bounded_budget() {
        assert_eq!(DOWNLOAD_TIMEOUT, Duration::from_secs(90));
        assert_eq!(TRANSCODE_TIMEOUT, Duration::from_secs(60));
    }

    #[tokio::test]
    async fn pruning_keeps_the_newest_downloads() {
        let dir = std::env::temp_dir().join("shamash-prune-test");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.expect("create dir");
        for i in 0..(KEEP_FILES + 4) {
            tokio::fs::write(dir.join(format!("song{i}.m4a")), b"audio")
                .await
                .expect("write");
            // Make the order of the files unambiguous.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        prune(&dir, &[]).await;

        let left = names_in(&dir).await;
        assert!(
            left.contains(&"song11.m4a".to_string()),
            "newest kept: {left:?}"
        );
        // The 12 files written are song0..song11, and the newest 8 are kept, so
        // the four oldest are what goes.
        for pruned in ["song0.m4a", "song1.m4a", "song2.m4a", "song3.m4a"] {
            assert!(
                !left.contains(&pruned.to_string()),
                "{pruned} should have been pruned: {left:?}"
            );
        }
        assert_eq!(left.len(), KEEP_FILES, "the cache must stay bounded");
    }

    /// The bug this exists for: `prune` kept the newest eight files outright,
    /// which is fewer than `MAX_QUEUE`, so with a dozen songs queued a
    /// thirteenth download deleted the audio of songs that were about to play.
    /// Nothing in use is ever deleted, however old it is.
    #[tokio::test]
    async fn pruning_never_deletes_a_queued_songs_audio() {
        let dir = std::env::temp_dir().join("shamash-prune-queued-test");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.expect("create dir");

        let queued: Vec<PathBuf> = (0..12)
            .map(|i| dir.join(format!("queued{i}.m4a")))
            .collect();
        for path in &queued {
            tokio::fs::write(path, b"audio").await.expect("write");
            // Deliberately old: pruning is by age, and these must survive it.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // Then the thirteenth, which is what triggers the prune.
        tokio::fs::write(dir.join("thirteenth.m4a"), b"audio")
            .await
            .expect("write");

        prune(&dir, &queued).await;

        let left = names_in(&dir).await;
        for path in &queued {
            assert!(
                left.contains(&path.file_name().unwrap().to_string_lossy().into_owned()),
                "{} was queued and must survive: {left:?}",
                path.display()
            );
        }
        assert_eq!(left.len(), 13, "nothing was in use to prune: {left:?}");
    }

    /// Once the songs are gone the cache shrinks again, so a queue that was
    /// full of one-off songs does not keep their audio forever.
    #[tokio::test]
    async fn pruning_resumes_once_the_songs_are_no_longer_queued() {
        let dir = std::env::temp_dir().join("shamash-prune-after-test");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.expect("create dir");
        for i in 0..(KEEP_FILES + 4) {
            tokio::fs::write(dir.join(format!("song{i}.m4a")), b"audio")
                .await
                .expect("write");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        prune(&dir, &[]).await;
        assert_eq!(names_in(&dir).await.len(), KEEP_FILES);
    }

    /// The file names left in a directory, for readable assertions.
    async fn names_in(dir: &Path) -> Vec<String> {
        let mut left: Vec<String> = Vec::new();
        let mut entries = tokio::fs::read_dir(dir).await.expect("read dir");
        while let Ok(Some(entry)) = entries.next_entry().await {
            left.push(entry.file_name().to_string_lossy().into_owned());
        }
        left.sort();
        left
    }

    /// Checks the fallback used when YouTube offers nothing but Opus: the
    /// stream is transcoded to WAV, which the `wav` and `pcm` handlers read.
    /// Ignored by default because it needs the ffmpeg binary.
    #[tokio::test]
    #[ignore = "needs ffmpeg"]
    async fn transcodes_opus_to_a_decodable_wav() {
        let dir = std::env::temp_dir().join("shamash-transcode-test");
        tokio::fs::create_dir_all(&dir).await.expect("create dir");

        // Stand in for the Opus download that triggers the fallback.
        let source = dir.join("src.webm");
        let made = Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-y", "-f", "lavfi"])
            .args(["-i", "sine=frequency=440:duration=2", "-c:a", "libopus"])
            .arg(&source)
            .output()
            .await
            .expect("run ffmpeg");
        assert!(made.status.success(), "could not build the test source");

        let dest = dir.join("track.wav");
        transcode(&source, &dest).await.expect("transcode");

        let header = tokio::fs::read(&dest).await.expect("read");
        assert_eq!(&header[0..4], b"RIFF", "the result must be a WAV file");
        assert_eq!(&header[8..12], b"WAVE");
        assert!(header.len() > 44, "the WAV must contain audio");
    }

    /// Downloads two different videos and checks each keeps its own audio,
    /// which is the bug behind hearing the previous song again. Ignored by
    /// default because it needs network access and the yt-dlp binary.
    #[tokio::test]
    #[ignore = "needs network and yt-dlp"]
    async fn two_songs_keep_separate_audio() {
        let dir = std::env::temp_dir().join("shamash-two-songs-test");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.expect("create dir");

        let first = fetch("wJGcwEv7838", "https://youtu.be/wJGcwEv7838", &dir, &[])
            .await
            .expect("fetch the first");
        let second = fetch(
            "aqz-KE-bpKQ",
            "https://youtu.be/aqz-KE-bpKQ",
            &dir,
            std::slice::from_ref(&first),
        )
        .await
        .expect("fetch the second");

        assert_ne!(first, second, "each song needs its own file");
        assert_ne!(
            tokio::fs::metadata(&first).await.expect("stat").len(),
            tokio::fs::metadata(&second).await.expect("stat").len(),
            "the files must hold different audio"
        );
    }

    /// Downloads a real video and checks the result is something the decoder
    /// can read. Ignored by default because it needs network access and the
    /// yt-dlp binary.
    #[tokio::test]
    #[ignore = "needs network and yt-dlp"]
    async fn fetches_audio_the_decoder_accepts() {
        use symphonia::core::formats::FormatOptions;
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::meta::MetadataOptions;
        use symphonia::core::probe::Hint;
        use symphonia::default::{get_codecs, get_probe};

        let dir = std::env::temp_dir().join("shamash-fetch-test");
        let path = fetch("wJGcwEv7838", "https://youtu.be/wJGcwEv7838", &dir, &[])
            .await
            .expect("fetch must succeed");
        let bytes = std::fs::read(&path).expect("read");
        assert!(!bytes.is_empty(), "the download must not be empty");

        let source =
            MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), Default::default());
        let mut probed = get_probe()
            .format(
                &Hint::new(),
                source,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .expect("the probe must recognise the download");
        let track = probed
            .format
            .default_track()
            .expect("the download must expose a track");
        get_codecs()
            .make(&track.codec_params, &Default::default())
            .expect("the track's codec must be registered");
        assert!(
            probed.format.next_packet().is_ok(),
            "packets must be readable"
        );
    }
}
