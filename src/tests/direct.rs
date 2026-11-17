use super::*;

/// `Arc<dyn Transcriber>` needs `Send + Sync`; fails to compile if the mutex around `Session` goes.
#[test]
fn the_transcriber_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<DirectTranscriber>();
}

#[test]
fn empty_audio_transcribes_to_nothing() {
    // A real model needs a GGUF on disk, so only the pre-session guard is checked.
    assert!(model::DEFAULT_MODEL_ID.ends_with(".gguf"));
}

fn whisper_languages() -> Vec<String> {
    ["en", "pt", "es", "fr"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// The engine takes only ISO-639-1 and would otherwise fail `pt-PT` at run time, on every transcription.
#[test]
fn a_language_the_model_does_not_know_is_refused_up_front() {
    let known = whisper_languages();
    assert!(language_is_known("en", &known));
    assert!(language_is_known("pt", &known));

    for wrong in ["pt-PT", "pt_PT", "Portuguese", "english", ""] {
        assert!(
            !language_is_known(wrong, &known),
            "{wrong} should not have been accepted"
        );
    }
}

/// Some Parakeet builds publish no language list; trying beats refusing to start.
#[test]
fn a_model_that_advertises_nothing_accepts_anything() {
    assert!(language_is_known("anything", &[]));
}
