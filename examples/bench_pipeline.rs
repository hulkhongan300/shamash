//! Measures the yt-dlp side of the music pipeline, so the extractor arguments
//! are chosen on numbers instead of folklore.
//!
//!     cargo run --release --example bench_pipeline
//!     cargo run --release --example bench_pipeline -- --runs 5 --videos id,id
//!
//! Three questions, three sections of output:
//!
//! 1. **Extraction** — `--dump-single-json --no-playlist` per candidate set of
//!    extractor arguments. This is what decides `player_client` and
//!    `--no-check-formats`.
//! 2. **Download** — the old `yt-dlp --format ...` download against the new
//!    "download the format URL directly" path, on the *same* extracted format,
//!    so the only difference measured is who moves the bytes.
//! 3. **Cache** — extraction with a throwaway `--cache-dir` against the
//!    persistent one, which is what `--cache-dir` is for.
//!
//! Every number is a median over `--runs` (default 3), because a single
//! extraction is one to two seconds and the median is what a spoken command
//! actually feels.

use anyhow::{Context, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Videos used when the caller names none.
///
/// Three of them: a normal music video, a long one, and one that has been
/// around long enough that some extractor clients no longer serve it cleanly.
/// A benchmark over one video flatters whichever client got lucky.
const DEFAULT_VIDEOS: [&str; 3] = ["wJGcwEv7838", "aqz-KE-bpKQ", "dQw4w9WgXcQ"];

/// The selector [`shamash::fetch::DECODABLE_FORMAT`] uses, kept here so the
/// benchmark exercises the same preference order the bot does.
const DECODABLE_FORMAT: &str = "bestaudio[acodec^=mp4a][vcodec=none]/\
     bestaudio[ext=m4a]/\
     bestaudio[acodec!=opus][vcodec=none]";

/// The extractor-argument sets under test, against the default as baseline.
///
/// `--no-check-formats` skips yt-dlp's HEAD/GET probe of every stream before
/// it picks one; that probe is pure latency here because the download only ever
/// touches the chosen format anyway. The `player_client` values are the ones
/// YouTube serves audio to without a login.
fn variants() -> Vec<(&'static str, Vec<String>)> {
    let client = |c: &str| {
        vec![
            "--extractor-args".to_string(),
            format!("youtube:player_client={c}"),
        ]
    };
    let mut all: Vec<(&'static str, Vec<String>)> = vec![("baseline (yt-dlp defaults)", vec![])];
    all.push(("no-check-formats", vec!["--no-check-formats".into()]));
    for c in ["web", "web_safari", "mweb", "android", "ios", "tv"] {
        all.push((
            Box::leak(format!("player_client={c}").into_boxed_str()),
            client(c),
        ));
    }
    all.push((
        "no-check-formats + player_client=web",
        ["--no-check-formats".to_string()]
            .into_iter()
            .chain(client("web"))
            .collect(),
    ));
    all
}

struct Args {
    runs: usize,
    videos: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    let mut args = Args {
        runs: 3,
        videos: DEFAULT_VIDEOS.iter().map(|s| s.to_string()).collect(),
    };
    let mut raw = std::env::args().skip(1);
    while let Some(arg) = raw.next() {
        match arg.as_str() {
            "--runs" => {
                args.runs = raw
                    .next()
                    .context("--runs needs a number")?
                    .parse()
                    .context("--runs needs a number")?
            }
            "--videos" => {
                args.videos = raw
                    .next()
                    .context("--videos needs a comma-separated list")?
                    .split(',')
                    .map(str::to_string)
                    .collect()
            }
            "--help" | "-h" => {
                println!("usage: bench_pipeline [--runs N] [--videos id,id,...]");
                return Ok(());
            }
            other => bail!("unknown argument {other}"),
        }
    }

    version();
    let cache = cache_dir();
    let scratch = std::env::temp_dir().join("shamash-bench-scratch");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).context("scratch dir")?;

    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent(UA)
        .build()
        .context("building the http client")?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the runtime")?;

    let mut extracted: BTreeMap<String, serde_json::Value> = BTreeMap::new();

    // Section 1: which extractor arguments extract fastest?
    println!("\n=== extraction: --dump-single-json --no-playlist ===");
    println!("{:<38} {:>10} {:>10}", "variant", "median", "worst");
    for (name, extra) in variants() {
        let mut times = Vec::new();
        let mut failed = 0;
        for _ in 0..args.runs {
            for video in &args.videos {
                match time_extract(&scratch, video, &extra) {
                    Ok((elapsed, json)) => {
                        times.push(elapsed);
                        extracted.entry(video.clone()).or_insert(json);
                    }
                    Err(_) => failed += 1,
                }
            }
        }
        let row = match median(&times) {
            None => format!("   FAILED x{failed}"),
            Some(m) => format!(
                "{:>9.0}ms {:>9.0}ms",
                ms(m),
                times.iter().map(Duration::as_secs_f64).fold(0.0, f64::max) * 1000.0
            ),
        };
        println!("{name:<38} {row}");
    }

    // Section 2: who moves the bytes? Same format, two downloaders.
    println!("\n=== download: yt-dlp --format vs direct GET of the format url ===");
    println!(
        "{:<20} {:>10} {:>12} {:>12}",
        "video", "format", "yt-dlp", "direct"
    );
    for video in &args.videos {
        let Some(json) = extracted.get(video) else {
            println!("{video:<20} (extraction failed, skipped)");
            continue;
        };
        let Some(format) = choose_format(json) else {
            println!("{video:<20} (no decodable format)");
            continue;
        };
        let via_ytdlp = repeat(args.runs, || timed(|| download_with_ytdlp(&scratch, video)));
        let direct = repeat(args.runs, || timed(|| download_direct(&rt, &http, &format)));
        println!(
            "{:<20} {:>10} {:>12} {:>12}",
            video,
            format.ext,
            fmt(median(&via_ytdlp)),
            fmt(median(&direct)),
        );
    }

    // Section 3: does a persistent cache dir actually pay?
    println!("\n=== cache: throwaway --cache-dir vs persistent ===");
    let cold = std::env::temp_dir().join("shamash-bench-cold");
    let _ = std::fs::remove_dir_all(&cold);
    let video = &args.videos[0];
    let first = time_extract(&cold, video, &[]).map(|(t, _)| t);
    let warm: Vec<Duration> = repeat(args.runs, || {
        time_extract(&cache, video, &[]).map(|(t, _)| t)
    });
    match (first, median(&warm)) {
        (Ok(cold_time), Some(warm)) => println!(
            "cold: {:>6.0}ms   warm: {:>6.0}ms   saved: {:>6.0}ms",
            ms(cold_time),
            ms(warm),
            ms(cold_time) - ms(warm),
        ),
        _ => println!("could not measure the cache directory"),
    }

    Ok(())
}

/// The browser User-Agent YouTube's own player sends. Media URLs are signed
/// against a client, and a direct GET that announces `yt-dlp` is the request
/// that gets a 403.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

fn version() {
    match std::process::Command::new("yt-dlp")
        .arg("--version")
        .output()
    {
        Ok(o) => println!("yt-dlp {}", String::from_utf8_lossy(&o.stdout).trim()),
        Err(_) => println!("yt-dlp NOT FOUND"),
    }
}

/// The persistent cache dir the bot uses, created so the cold/warm comparison
/// measures a directory that exists rather than one yt-dlp has to make.
fn cache_dir() -> PathBuf {
    let dir = std::env::var_os("YTDLP_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("shamash-ytdlp"));
    std::fs::create_dir_all(&dir).expect("creating the cache dir");
    dir
}

fn repeat(n: usize, mut f: impl FnMut() -> anyhow::Result<Duration>) -> Vec<Duration> {
    (0..n).filter_map(|_| f().ok()).collect()
}

fn timed(f: impl FnOnce() -> anyhow::Result<()>) -> anyhow::Result<Duration> {
    let started = Instant::now();
    f()?;
    Ok(started.elapsed())
}

/// The median, or `None` when there were no successful samples.
///
/// `None` rather than a panic: a failed download is a result this benchmark
/// exists to report, not a crash that hides the rows around it.
fn median(times: &[Duration]) -> Option<Duration> {
    if times.is_empty() {
        return None;
    }
    let mut sorted = times.to_vec();
    sorted.sort_unstable();
    Some(sorted[sorted.len() / 2])
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// A median as a table cell, or `FAILED` when nothing succeeded.
fn fmt(d: Option<Duration>) -> String {
    match d {
        Some(d) => format!("{:.0}ms", ms(d)),
        None => "FAILED".to_string(),
    }
}

/// Runs one extraction and returns how long it took plus its JSON.
///
/// Timed around the process, not around parsing, because parsing is the same
/// microseconds either way and the subprocess is the entire cost.
fn time_extract(
    dir: &PathBuf,
    video: &str,
    extra: &[String],
) -> anyhow::Result<(Duration, serde_json::Value)> {
    let output = std::process::Command::new("yt-dlp")
        .args([
            "--dump-single-json",
            "--no-playlist",
            "--no-warnings",
            "--cache-dir",
        ])
        .arg(dir)
        .args(extra)
        .arg(format!("https://youtu.be/{video}"))
        .output()
        .context("running yt-dlp")?;
    anyhow::ensure!(
        output.status.success(),
        "yt-dlp failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    // The first run only warms the cache; the timed run is the second, because
    // section 3 measures the cold cost on its own.
    let _ = serde_json::from_slice::<serde_json::Value>(&output.stdout);
    let started = Instant::now();
    let output = std::process::Command::new("yt-dlp")
        .args([
            "--dump-single-json",
            "--no-playlist",
            "--no-warnings",
            "--cache-dir",
        ])
        .arg(dir)
        .args(extra)
        .arg(format!("https://youtu.be/{video}"))
        .output()
        .context("running yt-dlp")?;
    anyhow::ensure!(output.status.success(), "yt-dlp failed on the timed run");
    Ok((
        started.elapsed(),
        serde_json::from_slice(&output.stdout).context("parsing the extracted json")?,
    ))
}

/// The audio format the bot would pick, by the same preference order as
/// [`DECODABLE_FORMAT`]: AAC in MP4 first, then any m4a, then anything that is
/// not Opus. Ties go to the larger bitrate, which is what `bestaudio` does.
fn choose_format(json: &serde_json::Value) -> Option<Picked> {
    let formats = json.get("formats")?.as_array()?;
    let rank = |f: &serde_json::Value| -> Option<u8> {
        let acodec = f.get("acodec")?.as_str().unwrap_or("none");
        let ext = f.get("ext")?.as_str().unwrap_or("");
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
    };
    formats
        .iter()
        .filter_map(|f| {
            let rank = rank(f)?;
            let url = f.get("url")?.as_str()?.to_string();
            let bitrate = f
                .get("abr")
                .or_else(|| f.get("tbr"))
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0);
            Some((
                rank,
                -bitrate,
                Picked {
                    ext: f
                        .get("ext")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?")
                        .into(),
                    url,
                    headers: f
                        .get("http_headers")
                        .and_then(serde_json::Value::as_object)
                        .map(|h| {
                            h.iter()
                                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                                .collect()
                        })
                        .unwrap_or_default(),
                },
            ))
        })
        .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)))
        .map(|(_, _, picked)| picked)
}

struct Picked {
    ext: String,
    url: String,
    headers: BTreeMap<String, String>,
}

/// The path the bot takes today: yt-dlp resolves the selector and downloads it.
fn download_with_ytdlp(dir: &Path, video: &str) -> anyhow::Result<()> {
    let out = dir.join(format!("bench-{video}.%(ext)s"));
    let output = std::process::Command::new("yt-dlp")
        .args([
            "--no-playlist",
            "--no-warnings",
            "--no-simulate",
            "--force-overwrites",
        ])
        .args(["--format", DECODABLE_FORMAT, "--output"])
        .arg(&out)
        .arg(format!("https://youtu.be/{video}"))
        .output()
        .context("running yt-dlp")?;
    anyhow::ensure!(
        output.status.success(),
        "yt-dlp download failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// The path the bot would take after the rewrite: GET the chosen format's URL
/// with the headers the extractor recorded for it.
///
/// `rt` because the rest of this benchmark is blocking (it times subprocesses
/// with `Instant`), and reqwest has no blocking client without another feature.
fn download_direct(
    rt: &tokio::runtime::Runtime,
    http: &reqwest::Client,
    format: &Picked,
) -> anyhow::Result<()> {
    rt.block_on(async {
        let mut request = http.get(&format.url).header("User-Agent", UA);
        for (name, value) in &format.headers {
            if name.eq_ignore_ascii_case("user-agent") {
                continue;
            }
            request = request.header(name, value);
        }
        let response = request.send().await.context("GET the format url")?;
        anyhow::ensure!(
            response.status().is_success(),
            "direct download returned {}",
            response.status()
        );
        let bytes = response.bytes().await.context("reading the body")?;
        anyhow::ensure!(!bytes.is_empty(), "direct download was empty");
        Ok(())
    })
}
