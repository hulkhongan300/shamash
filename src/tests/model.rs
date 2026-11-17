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
