//! Locating the GGUF the ASR engines load, by path or Hugging Face catalogue id.

use std::path::{Path, PathBuf};

/// Whisper large-v3-turbo Q8_0: promptable, so it spells artist names best
/// (~1 s on GPU, ~17 s on CPU, hence the `vulkan` feature).
pub const DEFAULT_MODEL_ID: &str =
    "handy-computer/whisper-large-v3-turbo-gguf/whisper-large-v3-turbo-Q8_0.gguf";

/// Parakeet TDT 0.6B v2 Q8_0: ~10x faster, English only, worse at names.
pub const ENGLISH_MODEL_ID: &str =
    "handy-computer/parakeet-tdt-0.6b-v2-gguf/parakeet-tdt-0.6b-v2-Q8_0.gguf";

/// Fast-loading wake model, also the Handy engine's default (it loads per utterance).
pub const SMALL_MODEL_ID: &str =
    "handy-computer/parakeet-tdt_ctc-110m-gguf/parakeet-tdt_ctc-110m-Q8_0.gguf";

/// Whisper copies its prompt's style, so an example request biases it toward one.
pub const REQUEST_PROMPT: &str =
    "Groovy, play Dracula by Tame Impala. Play Bohemian Rhapsody by Queen.";

/// Resolves a path, a catalogue id (`org/<repo>/<file>.gguf`), or a bare cached filename.
pub fn resolve(spec: &str) -> anyhow::Result<PathBuf> {
    resolve_in(&hf_cache_dir(), spec)
}

fn resolve_in(cache: &Path, spec: &str) -> anyhow::Result<PathBuf> {
    let direct = Path::new(spec);
    if direct.is_file() {
        return Ok(direct.to_path_buf());
    }

    let (repo, file) = match spec.split_once('/') {
        Some((org, rest)) => match rest.split_once('/') {
            Some((repo, file)) => (format!("{org}/{repo}"), file),
            None => (String::new(), rest),
        },
        None => (String::new(), spec),
    };

    let candidates: Vec<PathBuf> = if repo.is_empty() {
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
    // `refs/main` may name a revision no longer on disk.
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

fn hf_cache_dir() -> PathBuf {
    if let Some(home) = std::env::var("HF_HOME").ok().filter(|h| !h.is_empty()) {
        return PathBuf::from(home).join("hub");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home).join(".cache/huggingface/hub")
}

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

/// Sorted, so the pick does not depend on directory order.
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

/// URL and cache destination (the layout [`resolve`] reads) for a model id; `None` otherwise.
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

/// Downloads an uncached model id on first start; `MODEL_DOWNLOAD=off` disables it.
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
        // Reuse resolve's error message.
        return resolve_in(cache, spec);
    };
    download(&url, &dest).await?;
    resolve_in(cache, spec)
}

/// Writes via a `.part` file so a broken download is never taken for a model.
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
        // Per read, not total: a stalled connection fails instead of hanging.
        .read_timeout(std::time::Duration::from_secs(60))
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
#[path = "tests/model.rs"]
mod tests;
