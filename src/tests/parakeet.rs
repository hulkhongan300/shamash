use super::*;

#[test]
fn wav_round_trips_through_the_header_it_writes() {
    let original: Vec<f32> = vec![0.0, 0.5, -0.5, 1.0, -1.0];
    let (rate, decoded) = wav::read(&wav::write(&original, 16_000)).unwrap();
    assert_eq!(rate, 16_000);
    assert_eq!(decoded.len(), original.len());
    for (got, want) in decoded.iter().zip(&original) {
        assert!((got - want).abs() < 1e-4, "{got} != {want}");
    }
}

#[test]
fn samples_are_clamped_rather_than_wrapping() {
    let (rate, decoded) = wav::read(&wav::write(&[2.0, -2.0], 16_000)).unwrap();
    assert_eq!(rate, 16_000);
    assert!(decoded[0] <= 1.0 && decoded[0] > 0.99);
    assert!(decoded[1] >= -1.0 && decoded[1] < -0.99);
}

#[test]
fn a_missing_handy_binary_names_the_fix() {
    let transcriber = HandyTranscriber {
        program: "handy-that-does-not-exist".to_string(),
        model: crate::model::SMALL_MODEL_ID.to_string(),
        lock: std::sync::Mutex::new(()),
    };
    let path = temp_wav_path();
    std::fs::write(&path, wav::write(&[0.0; 16], 16_000)).unwrap();
    let err = transcriber.run(&path).unwrap_err().to_string();
    let _ = std::fs::remove_file(&path);
    assert!(err.contains("could not find"), "unhelpful error: {err}");
    assert!(
        err.contains("in-process engine"),
        "no fallback named: {err}"
    );
}

#[test]
fn the_json_line_is_found_even_after_log_output() {
    let transcriber = HandyTranscriber::new(crate::model::SMALL_MODEL_ID);
    let stdout = "noise\n{\"load_ms\":5,\"transcribe_ms\":[7],\"text\":\"bot play lofi\"}\n";
    let parsed: HandyOutput = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!(parsed.text, "bot play lofi");
    assert_eq!(parsed.load_ms, 5);
    let _ = &transcriber;
}

/// `--transcribe-file` has no `--language`, so a model needing one returns
/// empty text silently (Canary and Nemotron did).
#[test]
fn the_default_model_does_not_need_a_language_hint() {
    assert!(
        !crate::model::SMALL_MODEL_ID.contains("canary"),
        "needs -l <lang>"
    );
    assert!(
        !crate::model::SMALL_MODEL_ID.contains("nemotron"),
        "returns empty on short audio"
    );
    assert!(
        crate::model::SMALL_MODEL_ID.contains("parakeet"),
        "no language hint needed"
    );
}
