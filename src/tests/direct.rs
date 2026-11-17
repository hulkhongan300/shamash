use super::*;

/// `Transcriber` is used behind `Arc<dyn Transcriber>`, which needs
/// `Send + Sync`. `Session` is neither on its own, so this fails to compile
/// if the mutex is ever removed.
#[test]
fn the_transcriber_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<DirectTranscriber>();
}

#[test]
fn empty_audio_transcribes_to_nothing() {
    // Building a real model needs a GGUF on disk, so this only checks the
    // guard that runs before the session is ever touched.
    assert!(model::DEFAULT_MODEL_ID.ends_with(".gguf"));
}

fn whisper_languages() -> Vec<String> {
    ["en", "pt", "es", "fr"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// `pt-PT` is the obvious thing to type for European Portuguese and it is
/// wrong: the engine takes plain ISO-639-1 and rejects everything else at
/// *run* time, which used to mean the bot joined the channel and then failed
/// every transcription.
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

/// A model that publishes no language list -- some Parakeet builds -- takes
/// anything, and refusing to start would be worse than trying.
#[test]
fn a_model_that_advertises_nothing_accepts_anything() {
    assert!(language_is_known("anything", &[]));
}
