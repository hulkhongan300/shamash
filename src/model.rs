//! Locating the GGUF that the ASR engines load.
//!
//! Both engines want the same model file, but they want it named differently:
//! the direct transcribe.cpp engine takes a filesystem path, while the Handy
//! subprocess is handed a catalogue id like
//! `handy-computer/<repo>/<file>.gguf` and resolves it against its own Hugging
//! Face cache. Keeping the default in one place stops the two paths drifting
//! onto different models, and [`resolve`] accepts either spelling so
//! `REQUEST_MODEL` (formerly `PARAKEET_MODEL`) takes either spelling.

use std::path::{Path, PathBuf};

/// The request model: Whisper large-v3-turbo, Q8_0.
///
/// Multilingual and able to detect the language itself, and the one model
/// family the engine lets us *prompt* (see [`REQUEST_PROMPT`]). The prompt is
/// what wins it the job: on five synthesised English requests naming artists
/// that are not in the prompt, Whisper with it spelled all five correctly
/// ("The Weeknd", "Mr. Brightside", "Dua Lipa"), where Parakeet 0.6B v2 got
/// two and heard one "play" as "place". The cost is latency: ~1 s per request on the
/// GPU, ~17 s on the CPU, which is why `transcribe-cpp`'s `vulkan` feature is
/// on. Whisper also invents text on audio that is mostly silence, which the
/// voice detector in [`crate::segment`] keeps away from it.
pub const DEFAULT_MODEL_ID: &str =
    "handy-computer/whisper-large-v3-turbo-gguf/whisper-large-v3-turbo-Q8_0.gguf";

/// The fast alternative for English: Parakeet TDT 0.6B v2, Q8_0.
///
/// About ten times quicker than Whisper turbo on the same GPU (~0.1 s against
/// ~1.1 s per request) and it does not hallucinate on near-silence, but it is
/// English only and cannot be prompted, so it spells names worse. Set
/// `REQUEST_MODEL` to this if latency matters more than names.
pub const ENGLISH_MODEL_ID: &str =
    "handy-computer/parakeet-tdt-0.6b-v2-gguf/parakeet-tdt-0.6b-v2-Q8_0.gguf";

/// The fast-loading model: the always-on wake model, and the default for the
/// Handy engine, which pays to load a model per utterance.
pub const SMALL_MODEL_ID: &str =
    "handy-computer/parakeet-tdt_ctc-110m-gguf/parakeet-tdt_ctc-110m-Q8_0.gguf";

/// What the request model is prompted with, for the models that take a prompt.
///
/// Whisper continues the style of its prompt, so an example of a request
/// biases it towards transcribing one: names capitalised as names, "by"
/// between title and artist.
pub const REQUEST_PROMPT: &str =
    "Bot, play Dracula by Tame Impala. Play Bohemian Rhapsody by Queen.";

/// Turns a model spec into a path this process can open.
///
/// Accepts, in order: a real path, a Hugging Face catalogue id
/// (`handy-computer/<repo>/<file>.gguf`), or a bare filename to look up in the
/// cache by name.
pub fn resolve(spec: &str) -> anyhow::Result<PathBuf> {
    resolve_in(&hf_cache_dir(), spec)
}

fn resolve_in(cache: &Path, spec: &str) -> anyhow::Result<PathBuf> {
    let direct = Path::new(spec);
    if direct.is_file() {
        return Ok(direct.to_path_buf());
    }

    let (repo, file) = match spec.split_once('/') {
        // `handy-computer/<repo>/<file>.gguf`, or a bare `<file>.gguf`.
        Some((org, rest)) => match rest.split_once('/') {
            Some((repo, file)) => (format!("{org}/{repo}"), file),
            None => (String::new(), rest),
        },
        None => (String::new(), spec),
    };

    let candidates: Vec<PathBuf> = if repo.is_empty() {
        // No repo given: search every Handy snapshot for a file of this name.
        snapshot_dirs(cache)
            .into_iter()
            .map(|d| d.join(file))
            .collect()
    } else {
        vec![
            cache
                .join(format!("models--{}", repo.replace('/', "--")))
                .join("snapshots")
                .join(current_revision(cache, &repo).unwrap_or_default())
                .join(file),
        ]
    };

    if let Some(path) = candidates.iter().find(|p| p.is_file()) {
        return Ok(path.clone());
    }
    // A stale `refs/main` can point at a revision that is no longer on disk, so
    // fall back to scanning every snapshot for that repo.
    if !repo.is_empty() {
        let base = cache.join(format!("models--{}", repo.replace('/', "--")));
        for snapshot in snapshot_dirs(&base) {
            let path = snapshot.join(file);
            if path.is_file() {
                return Ok(path);
            }
        }
    }

    anyhow::bail!(
        "could not find model '{spec}'; give a path to the .gguf file or install it with the \
         Handy app so it lands in {}",
        cache.display()
    )
}

/// The Hugging Face cache root, honouring `HF_HOME` the way the library does.
fn hf_cache_dir() -> PathBuf {
    if let Some(home) = std::env::var("HF_HOME").ok().filter(|h| !h.is_empty()) {
        return PathBuf::from(home).join("hub");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home).join(".cache/huggingface/hub")
}

/// The revision `refs/main` names for a cached repo, if it is recorded.
fn current_revision(cache: &Path, repo: &str) -> Option<String> {
    std::fs::read_to_string(
        cache
            .join(format!("models--{}", repo.replace('/', "--")))
            .join("refs/main"),
    )
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
}

/// Every `snapshots/<revision>` directory under a cache repo, sorted so the
/// result does not depend on directory order.
fn snapshot_dirs(repo_cache: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(repo_cache.join("snapshots"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// Where a `org/repo/file.gguf` model id is downloaded from, and where it goes.
///
/// The file lands in the Hugging Face cache layout [`resolve`] already reads,
/// under a `main` snapshot, so a downloaded model and one fetched with
/// `huggingface-cli` are found the same way. `None` for a path or a bare file
/// name, which say nothing about where to fetch them from.
fn download_target(cache: &Path, spec: &str) -> Option<(String, PathBuf)> {
    let mut parts = spec.splitn(3, '/');
    let (org, repo, file) = (parts.next()?, parts.next()?, parts.next()?);
    let safe = |part: &str| {
        !part.is_empty() && part != ".." && !part.starts_with('.') && !part.contains('\\')
    };
    if !safe(org) || !safe(repo) || !file.ends_with(".gguf") || !file.split('/').all(safe) {
        return None;
    }
    let url = format!("https://huggingface.co/{org}/{repo}/resolve/main/{file}");
    let dest = cache
        .join(format!("models--{org}--{repo}"))
        .join("snapshots/main")
        .join(file);
    Some((url, dest))
}

/// Makes sure `spec` is on disk, downloading it from Hugging Face if it is a
/// model id that is not in the cache yet.
///
/// This is what lets a fresh install, or a fresh container, start with nothing
/// but a `.env`: the defaults are model ids, and the first start fetches them.
/// `MODEL_DOWNLOAD=off` turns it off, for a machine that must not reach out.
pub async fn ensure(spec: &str) -> anyhow::Result<PathBuf> {
    ensure_in(&hf_cache_dir(), spec).await
}

async fn ensure_in(cache: &Path, spec: &str) -> anyhow::Result<PathBuf> {
    if let Ok(path) = resolve_in(cache, spec) {
        return Ok(path);
    }
    let off = std::env::var("MODEL_DOWNLOAD")
        .is_ok_and(|v| ["off", "false", "0", "no"].contains(&v.trim().to_lowercase().as_str()));
    let Some((url, dest)) = download_target(cache, spec).filter(|_| !off) else {
        // Let the loader report it, with the message it already has.
        return resolve_in(cache, spec);
    };
    download(&url, &dest).await?;
    resolve_in(cache, spec)
}

/// Streams `url` to `dest`, through a `.part` file so an interrupted download
/// is never mistaken for a model.
async fn download(url: &str, dest: &Path) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use tokio::io::AsyncWriteExt as _;

    let parent = dest.parent().context("model path has no directory")?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("could not create {}", parent.display()))?;
    let client = reqwest::Client::builder()
        .user_agent(crate::config::HTTP_USER_AGENT)
        .connect_timeout(std::time::Duration::from_secs(20))
        .build()?;
    let mut response = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("could not download {url}"))?;
    let total = response.content_length();
    tracing::info!(
        url,
        size_mb = total.map(|b| b / 1_000_000),
        to = %dest.display(),
        "downloading a model; this happens once"
    );

    let part = dest.with_extension("gguf.part");
    let mut file = tokio::fs::File::create(&part)
        .await
        .with_context(|| format!("could not write {}", part.display()))?;
    let mut written: u64 = 0;
    let mut reported = 0;
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("the download of {url} broke off"))?
    {
        file.write_all(&chunk).await?;
        written += chunk.len() as u64;
        if let Some(total) = total.filter(|t| *t > 0) {
            let percent = written * 100 / total;
            if percent >= reported + 10 {
                reported = percent - percent % 10;
                tracing::info!(percent = reported, "downloading the model");
            }
        }
    }
    file.flush().await?;
    drop(file);
    if let Some(total) = total {
        anyhow::ensure!(
            written == total,
            "the download of {url} stopped at {written} of {total} bytes"
        );
    }
    tokio::fs::rename(&part, dest).await?;
    tracing::info!(path = %dest.display(), "model downloaded");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_id_downloads_into_the_cache_layout_resolve_reads() {
        let cache = std::env::temp_dir().join(format!("shamash-dl-{}", std::process::id()));
        let spec = "handy-computer/parakeet-tdt_ctc-110m-gguf/parakeet-tdt_ctc-110m-Q8_0.gguf";
        let (url, dest) = download_target(&cache, spec).expect("a model id");
        assert_eq!(
            url,
            "https://huggingface.co/handy-computer/parakeet-tdt_ctc-110m-gguf/resolve/main/parakeet-tdt_ctc-110m-Q8_0.gguf"
        );
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"gguf").unwrap();
        // No refs/main at all, as after a first download.
        assert_eq!(resolve_in(&cache, spec).unwrap(), dest);
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// Downloads the 130 MB wake model into a scratch cache.
    #[tokio::test]
    #[ignore = "downloads 130 MB from Hugging Face"]
    async fn a_missing_model_is_downloaded_and_then_found() {
        let cache = std::env::temp_dir().join(format!("shamash-live-{}", std::process::id()));
        let path = ensure_in(&cache, SMALL_MODEL_ID).await.expect("download");
        assert!(std::fs::metadata(&path).unwrap().len() > 100_000_000);
        // A second call finds it rather than downloading again.
        assert_eq!(ensure_in(&cache, SMALL_MODEL_ID).await.unwrap(), path);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn only_a_model_id_is_downloaded() {
        let cache = Path::new("/cache");
        assert!(download_target(cache, "/models/whisper.gguf").is_none());
        assert!(download_target(cache, "whisper.gguf").is_none());
        assert!(download_target(cache, "org/repo/notes.txt").is_none());
        assert!(download_target(cache, "org/../x.gguf").is_none());
        assert!(download_target(cache, "org/repo/../../x.gguf").is_none());
    }

    #[test]
    fn an_existing_path_is_returned_unchanged() {
        let path = std::env::current_exe().unwrap();
        assert_eq!(resolve(path.to_str().unwrap()).unwrap(), path);
    }

    #[test]
    fn a_catalogue_id_becomes_a_hugging_face_cache_path() {
        let cache = hf_cache_dir().join("models--handy-computer--demo-gguf");
        let snapshot = cache.join("snapshots/abc123");
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(snapshot.join("demo-Q8_0.gguf"), b"x").unwrap();
        // SAFETY: single-threaded test process; no other test mutates HF_HOME.
        unsafe { std::env::set_var("HF_HOME", hf_cache_dir().parent().unwrap()) };

        let found = resolve("handy-computer/demo-gguf/demo-Q8_0.gguf").unwrap();
        assert!(found.ends_with("demo-Q8_0.gguf"), "got {found:?}");
        assert!(found.is_file());

        std::fs::remove_dir_all(&cache).ok();
    }

    #[test]
    fn a_missing_model_names_the_cache_it_looked_in() {
        let err = resolve("handy-computer/not-a-real-repo-gguf/nope-Q8_0.gguf")
            .unwrap_err()
            .to_string();
        assert!(err.contains("could not find model"), "got {err}");
        assert!(err.contains(".cache/huggingface"), "got {err}");
    }
}
