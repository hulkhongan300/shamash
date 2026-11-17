use super::*;

#[test]
fn the_song_memory_path_defaults_and_can_be_switched_off() {
    assert_eq!(library_path(None), Some(DEFAULT_LIBRARY_PATH.into()));
    assert_eq!(
        library_path(Some("  ".into())),
        Some(DEFAULT_LIBRARY_PATH.into())
    );
    assert_eq!(
        library_path(Some("/data/library.json".into())),
        Some("/data/library.json".into())
    );
    for off in ["off", "OFF", "none", "0", "false"] {
        assert_eq!(library_path(Some(off.into())), None, "{off}");
    }
}

#[test]
fn parses_csv_wake_words() {
    let words = split_csv("Shamash, BOT,  ,tveir");
    assert_eq!(words, vec!["shamash", "bot", "tveir"]);
}

#[test]
fn empty_csv_yields_no_wake_words() {
    assert!(split_csv(" , , ").is_empty());
}

#[test]
fn a_blank_optional_channel_id_means_unset() {
    assert_eq!(parse_optional_id(Some(String::new())).unwrap(), None);
    assert_eq!(parse_optional_id(Some("   ".to_string())).unwrap(), None);
    assert_eq!(parse_optional_id(None).unwrap(), None);
}

#[test]
fn an_optional_channel_id_is_read_when_present() {
    assert_eq!(
        parse_optional_id(Some(" 12345 ".to_string())).unwrap(),
        Some(12345)
    );
}

#[test]
fn a_non_numeric_optional_channel_id_is_rejected() {
    assert!(parse_optional_id(Some("not-a-channel".to_string())).is_err());
}

/// A typo must not become a silent `false` here.
#[test]
fn a_malformed_boolean_is_an_error_rather_than_a_default() {
    for (raw, expected) in [
        ("1", true),
        ("true", true),
        (" YES ", true),
        ("on", true),
        ("0", false),
        ("false", false),
        ("No", false),
        ("off", false),
    ] {
        assert_eq!(parse_bool("UNVERIFIED_CHOOSER", raw).unwrap(), expected);
    }
    let err = parse_bool("UNVERIFIED_CHOOSER", "ture")
        .unwrap_err()
        .to_string();
    assert!(err.contains("UNVERIFIED_CHOOSER"), "unhelpful: {err}");
    assert!(err.contains("true or false"), "unhelpful: {err}");
}

#[test]
fn asr_engine_names_are_case_insensitive() {
    assert_eq!(AsrEngine::parse("Handy").unwrap(), AsrEngine::Handy);
    assert_eq!(AsrEngine::parse(" parakeet ").unwrap(), AsrEngine::Handy);
    assert_eq!(AsrEngine::parse("DIRECT").unwrap(), AsrEngine::Direct);
    assert_eq!(
        AsrEngine::parse(" transcribe-cpp ").unwrap(),
        AsrEngine::Direct
    );
}

#[test]
fn the_removed_whisper_engine_says_where_to_go() {
    let err = AsrEngine::parse("whisper").unwrap_err().to_string();
    assert!(err.contains("ASR_ENGINE=direct"), "unhelpful error: {err}");
}

#[test]
fn the_ambiguity_policy_is_case_insensitive_and_asks_on_a_typo() {
    assert_eq!(AmbiguousPolicy::parse(Some("auto")), AmbiguousPolicy::Auto);
    assert_eq!(
        AmbiguousPolicy::parse(Some(" POPULAR ")),
        AmbiguousPolicy::Popular
    );
    assert_eq!(AmbiguousPolicy::parse(Some("ask")), AmbiguousPolicy::Ask);
    assert_eq!(AmbiguousPolicy::parse(Some("atou")), AmbiguousPolicy::Ask);
    assert_eq!(AmbiguousPolicy::parse(None), AmbiguousPolicy::Ask);
}

#[test]
fn an_unknown_asr_engine_is_rejected_by_name() {
    let err = AsrEngine::parse("deepspeech").unwrap_err().to_string();
    assert!(err.contains("deepspeech"), "unhelpful error: {err}");
}
