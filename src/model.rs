//! Locating the GGUF that the ASR engines load.
//!
//! Both engines want the same model file, but they want it named differently:
//! the direct transcribe.cpp engine takes a filesystem path, while the Handy
//! subprocess is handed a catalogue id like
//! `handy-computer/<repo>/<file>.gguf` and resolves it against its own Hugging
//! Face cache. Keeping the default in one place stops the two paths drifting
//! onto different models, and [`resolve`] accepts either spelling so
//! `PARAKEET_MODEL` keeps working unchanged for people who set it for Handy.

use std::path::{Path, PathBuf};

/// The model both engines default to.
///
/// Whisper large-v3-turbo, Q8_0. It is the strongest recogniser the Handy
/// catalogue offers for general speech and it detects the language itself, so
/// `ASR_LANGUAGE` is not needed for it. The previous default, Parakeet TDT
/// 0.6B v2, was English-only and had to be chosen for being the best *English*
/// model available.
///
/// The cost is size and, without a GPU, latency: ~870 MB of weights against
/// the Parakeet's ~600 MB, and a transformer decode rather than a
/// frame-aligned one. On the CPU that is ~17 s for a short utterance, which is
/// why `transcribe-cpp`'s `vulkan` feature is on; on the GPU it is ~1 s. It is
/// still far quicker than the old `whisper-rs` route either way, because
/// `ASR_ENGINE=direct` holds the model resident rather than reloading it per
/// utterance.
///
/// `ASR_ENGINE=handy` runs whatever `PARAKEET_MODEL` names, so point that at
/// [`SMALL_MODEL_ID`] when using it: the load cost is paid per utterance.
pub const DEFAULT_MODEL_ID: &str =
    "handy-computer/whisper-large-v3-turbo-gguf/whisper-large-v3-turbo-Q8_0.gguf";

/// The fast-loading model, for the engine that pays to load one per utterance.
pub const SMALL_MODEL_ID: &str =
    "handy-computer/parakeet-tdt_ctc-110m-gguf/parakeet-tdt_ctc-110m-Q8_0.gguf";

/// Turns a model spec into a path this process can open.
///
/// Accepts, in order: a real path, a Hugging Face catalogue id
/// (`handy-computer/<repo>/<file>.gguf`), or a bare filename to look up in the
/// cache by name.
pub fn resolve(spec: &str) -> anyhow::Result<PathBuf> {
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

    let cache = hf_cache_dir();
    let candidates: Vec<PathBuf> = if repo.is_empty() {
        // No repo given: search every Handy snapshot for a file of this name.
        snapshot_dirs(&cache)
            .into_iter()
            .map(|d| d.join(file))
            .collect()
    } else {
        vec![
            cache
                .join(format!("models--{}", repo.replace('/', "--")))
                .join("snapshots")
                .join(current_revision(&cache, &repo).unwrap_or_default())
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

#[cfg(test)]
mod tests {
    use super::*;

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
