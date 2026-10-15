//! Time to first audio, streaming against downloading, over ten songs.
//!
//!     cargo run --release --example bench_stream
//!     cargo run --release --example bench_stream -- --videos id,id,...
//!
//! One question, measured twice on the same ten videos:
//!
//! * **before** — one yt-dlp download, then decode. What the bot did until the
//!   audio stopped being fetched before it played.
//! * **after** — one yt-dlp extraction, then the format URL played as it
//!   arrives, through the same fallback the bot uses: if the URL will not
//!   serve, the download happens instead and the song is reported as `file`.
//!
//! "First audio" is not the whole download finishing. It is the first audio
//! packet the decoder can hand the mixer, which for a stream is a few kilobytes
//! behind the front of the response rather than the last one. Both paths are
//! measured the same way -- through symphonia, to the first decoded sample, over
//! a source that cannot seek -- so the numbers are comparable and neither is
//! flattered by stopping earlier than the other.
//!
//! Cold only: each video is fetched into an empty cache, so this is what a
//! listener hears the first time they ask for a song.

use anyhow::{Context, bail};
use shamash::fetch;
use shamash::queue::Source;
use shamash::timing::Timing;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use symphonia::core::io::MediaSource;

/// The ten videos, chosen to look like a playlist rather than a best case.
///
/// The first three are the ones the other benchmarks use; the rest are ordinary
/// songs of ordinary length, because streaming's advantage shrinks as the track
/// gets longer relative to its start-up -- a ten-second clip flatters it.
const DEFAULT_VIDEOS: [&str; 10] = [
    "wJGcwEv7838",
    "aqz-KE-bpKQ",
    "dQw4w9WgXcQ",
    "kJQP7kiw5Fk",
    "JGwWNGJdvx8",
    "RgKAFK5djSk",
    "OPf0YbXqDm0",
    "CevxZvSJLk8",
    "hT_nvWreIhg",
    "YQHsXMglC9A",
];

/// One song's time to first audio.
struct Measured {
    took: Duration,
    /// `stream` or `file`: which of the two paths this song actually took.
    route: &'static str,
    /// How much of the audio had to arrive before the first sample came out.
    bytes: u64,
}

fn main() -> anyhow::Result<()> {
    let mut videos: Vec<String> = DEFAULT_VIDEOS.iter().map(|s| s.to_string()).collect();
    let mut raw = std::env::args().skip(1);
    while let Some(arg) = raw.next() {
        match arg.as_str() {
            "--videos" => {
                videos = raw
                    .next()
                    .context("--videos needs a comma-separated list")?
                    .split(',')
                    .map(str::to_string)
                    .collect()
            }
            "--help" | "-h" => {
                println!("usage: bench_stream [--videos id,id,...]");
                return Ok(());
            }
            other => bail!("unknown argument {other}"),
        }
    }

    println!("yt-dlp {}", yt_dlp_version());
    println!(
        "\n{:<13} {:>10} {:>10} {:>7} {:>9} {:>11}",
        "video", "download", "stream", "route", "saved", "for audio"
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the runtime")?;

    let mut downloads = Vec::new();
    let mut streams = Vec::new();

    for video in &videos {
        let url = format!("https://youtu.be/{video}");
        let root = scratch(video);

        let before = rt.block_on(measure(
            async {
                fetch::download_to_cache(
                    &url,
                    &root.join("download"),
                    video,
                    &[],
                    &Timing::default(),
                )
                .await
                .map(Source::File)
            },
            None,
        ));
        let after = rt.block_on(measure(
            fetch::fetch(video, &url, &root.join("stream"), &[], &Timing::default()),
            Some(&url),
        ));

        for measured in [&before, &after] {
            match measured {
                Ok(Measured {
                    route: "file",
                    took,
                    ..
                }) => downloads.push(*took),
                Ok(Measured {
                    route: "stream",
                    took,
                    ..
                }) => streams.push(*took),
                _ => {}
            }
        }

        let row = |m: &anyhow::Result<Measured>,
                   what: &str|
         -> (Option<Duration>, &'static str, Option<u64>) {
            match m {
                Ok(m) => (Some(m.took), m.route, Some(m.bytes)),
                Err(e) => {
                    println!("{video:<13} {what} failed: {e:#}");
                    (None, "-", None)
                }
            }
        };
        let (before_ms, _, _) = row(&before, "download");
        let (after_ms, route, bytes) = row(&after, "stream");
        let saved = match (before_ms, after_ms) {
            (Some(before), Some(after)) => Some(before.saturating_sub(after)),
            _ => None,
        };
        println!(
            "{:<13} {:>10} {:>10} {:>7} {:>9} {:>11}",
            video,
            ms(before_ms),
            ms(after_ms),
            route,
            saved.map_or("-".to_string(), |s| format!("-{}", ms(Some(s)))),
            bytes.map_or("-".to_string(), |b| format!("{:.0}KiB", b as f64 / 1024.0)),
        );
    }

    println!(
        "\nmedian time to first audio:  download {}  stream {}",
        ms(median(&downloads)),
        ms(median(&streams))
    );
    Ok(())
}

/// Runs the real fetch, then decodes until there is audio to play.
///
/// `watch_url` is `Some` on the streaming path, and is what makes this the
/// production path rather than a shorter one: the stream's URL is checked for
/// 403 and for a stall first, exactly as playback does, and a song that fails
/// that check is measured on the download path instead.
async fn measure<F>(fetch: F, watch_url: Option<&str>) -> anyhow::Result<Measured>
where
    F: std::future::Future<Output = anyhow::Result<Source>>,
{
    let timing = Timing::default();
    let started = Instant::now();
    let source = fetch.await?;
    let source = match watch_url {
        Some(url) => fetch::resolve(&source, url, &[], &timing).await?,
        None => source,
    };

    match source {
        Source::File(path) => {
            let bytes = tokio::fs::metadata(&path)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            decode_first_sample(&path)?;
            Ok(Measured {
                took: started.elapsed(),
                route: "file",
                bytes,
            })
        }
        Source::Stream {
            url,
            headers,
            length,
            ..
        } => {
            let bytes = stream_first_audio(&url, &headers, length).await?;
            Ok(Measured {
                took: started.elapsed(),
                route: "stream",
                bytes,
            })
        }
    }
}

/// The bytes behind the first audio packet of a stream.
///
/// Reads the format URL as the player does: through symphonia, over a source
/// that cannot seek. Whether an MP4 can be demuxed at all while it arrives is
/// exactly whether the demuxer finds its sample tables before the audio it is
/// demuxing, so a source that could seek would answer a different question.
async fn stream_first_audio(
    url: &str,
    headers: &reqwest::header::HeaderMap,
    length: u64,
) -> anyhow::Result<u64> {
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;
    use symphonia::default::{get_codecs, get_probe};

    let response = reqwest::Client::builder()
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                     (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        )
        .build()?
        .get(url)
        .headers(headers.clone())
        // A closed range, or YouTube answers 403: see `Streamable::length`.
        .header("range", format!("bytes=0-{}", length.saturating_sub(1)))
        .send()
        .await?;
    anyhow::ensure!(response.status().is_success(), "{}", response.status());

    // Symphonia's reader is synchronous and this response is not, so the decode
    // runs where blocking is allowed and each read parks the runtime.
    let consumed = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&consumed);
    tokio::task::block_in_place(move || {
        let source =
            MediaSourceStream::new(Box::new(Piped::new(response, counter)), Default::default());
        let mut probed = get_probe().format(
            &Hint::new(),
            source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )?;
        let track = probed.format.default_track().context("no audio track")?;
        let mut decoder = get_codecs().make(&track.codec_params, &Default::default())?;

        // Symphonia 0.5's reader yields a packet per call and ends by failing,
        // so the loop is over the decode rather than over an iterator.
        loop {
            let packet: symphonia::core::formats::Packet = probed.format.next_packet()?;
            if decoder.decode(&packet)?.frames() > 0 {
                return anyhow::Ok(());
            }
        }
    })?;
    Ok(consumed.load(Ordering::Relaxed))
}

/// Decodes a downloaded file until the first sample, so both paths are measured
/// to the same point rather than one of them being allowed to stop earlier.
fn decode_first_sample(path: &Path) -> anyhow::Result<()> {
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;
    use symphonia::default::{get_codecs, get_probe};

    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let source = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), Default::default());
    let mut probed = get_probe().format(
        &Hint::new(),
        source,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    )?;
    let track = probed.format.default_track().context("no audio track")?;
    let mut decoder = get_codecs().make(&track.codec_params, &Default::default())?;
    loop {
        let packet = probed.format.next_packet()?;
        if decoder.decode(&packet)?.frames() > 0 {
            return Ok(());
        }
    }
}

/// An HTTP response as a media source: forward-only, and no seeking.
///
/// Chunk by chunk, because buffering the whole response first would measure the
/// download again and hide the thing being measured.
struct Piped {
    response: reqwest::Response,
    unread: std::io::Cursor<Vec<u8>>,
    consumed: Arc<AtomicU64>,
}

impl Piped {
    fn new(response: reqwest::Response, consumed: Arc<AtomicU64>) -> Self {
        Self {
            response,
            unread: std::io::Cursor::new(Vec::new()),
            consumed,
        }
    }

    /// The next chunk of the response, from the runtime thread this runs on.
    async fn chunk(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        self.response
            .chunk()
            .await
            .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
            .map_err(std::io::Error::other)
    }
}

impl std::io::Read for Piped {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // Whatever has arrived but not been demuxed yet is served first: a chunk
        // that has landed is not dropped.
        let read = self.unread.read(buf)?;
        if read > 0 {
            return Ok(read);
        }
        self.unread.set_position(0);
        let Some(chunk) = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.chunk())
        })?
        else {
            return Ok(0);
        };
        self.consumed
            .fetch_add(chunk.len() as u64, Ordering::Relaxed);
        let len = buf.len().min(chunk.len());
        buf[..len].copy_from_slice(&chunk[..len]);
        self.unread = std::io::Cursor::new(chunk);
        self.unread.set_position(len as u64);
        Ok(len)
    }
}

impl std::io::Seek for Piped {
    fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
}

impl MediaSource for Piped {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        self.response.content_length()
    }
}

fn ms(duration: Option<Duration>) -> String {
    match duration {
        Some(took) => format!("{:.0}ms", took.as_secs_f64() * 1000.0),
        None => "-".to_string(),
    }
}

fn median(times: &[Duration]) -> Option<Duration> {
    if times.is_empty() {
        return None;
    }
    let mut sorted = times.to_vec();
    sorted.sort_unstable();
    Some(sorted[sorted.len() / 2])
}

fn scratch(video: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("shamash-bench-stream-{video}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn yt_dlp_version() -> String {
    std::process::Command::new("yt-dlp")
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "NOT FOUND".to_string())
}
