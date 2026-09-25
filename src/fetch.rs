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

use anyhow::{Context, bail};
use std::path::{Path, PathBuf};
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

/// Directory holding one guild's downloaded audio.
///
/// A fixed path per guild keeps the temporary directory from growing without
/// bound. yt-dlp downloads to a `.part` file and renames it into place, so
/// replacing the file never disturbs a track that is still playing.
pub fn cache_dir(guild_id: u64) -> PathBuf {
    std::env::temp_dir().join(format!("shamash-{guild_id}"))
}

/// Downloads decodable audio for `url` into `dir` and returns the file to play.
///
/// The file is either the stream YouTube already publishes in a decodable
/// format, or a WAV transcoded from the best stream it offers.
pub async fn fetch(url: &str, dir: &Path) -> anyhow::Result<PathBuf> {
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("could not create {}", dir.display()))?;

    match download(url, dir, DECODABLE_FORMAT).await {
        Ok(path) => Ok(path),
        Err(no_decodable) => {
            tracing::debug!("no decodable stream for {url}: {no_decodable:#}");
            let source = download(url, dir, BEST_FORMAT)
                .await
                .context("yt-dlp could not download the audio")?;
            let transcoded = dir.join("track.wav");
            transcode(&source, &transcoded).await?;
            let _ = tokio::fs::remove_file(&source).await;
            Ok(transcoded)
        }
    }
}

/// Downloads one format into `dir` and returns the path yt-dlp wrote.
async fn download(url: &str, dir: &Path, format: &str) -> anyhow::Result<PathBuf> {
    let output = Command::new("yt-dlp")
        .args([
            "--no-playlist",
            "--no-warnings",
            "--no-simulate",
            "--no-part",
        ])
        .args(["--format", format])
        .arg("--output")
        .arg(dir.join("track.%(ext)s"))
        .arg("--print")
        .arg("after_move:filepath")
        .arg(url)
        .output()
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

    let size = tokio::fs::metadata(&path)
        .await
        .with_context(|| format!("yt-dlp reported {} but it is not there", path.display()))?
        .len();
    anyhow::ensure!(size > 0, "yt-dlp downloaded an empty {}", path.display());

    Ok(path)
}

/// Rewrites `source` as 16-bit PCM WAV at `dest`.
///
/// Songbird resamples to its own 48 kHz stereo output, so the sample rate and
/// channel count are left as they are and only the codec is replaced.
async fn transcode(source: &Path, dest: &Path) -> anyhow::Result<()> {
    let output = Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-y"])
        .arg("-i")
        .arg(source)
        .args(["-c:a", "pcm_s16le"])
        .arg(dest)
        .output()
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
    fn each_guild_gets_its_own_directory() {
        assert_ne!(cache_dir(1), cache_dir(2));
        assert!(cache_dir(42).ends_with("shamash-42"));
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
        let path = fetch("https://youtu.be/wJGcwEv7838", &dir)
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
