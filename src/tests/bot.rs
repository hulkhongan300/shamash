use super::*;

#[test]
fn a_chooser_press_is_refused_only_for_a_reason_that_fits() {
    // The owner of the list, or anyone in the channel for a spoken one.
    assert_eq!(chooser_refusal(true, false, true), None);
    assert_eq!(chooser_refusal(true, true, true), None);
    // Somebody else's `/play` is theirs; a spoken one is the channel's.
    assert_eq!(chooser_refusal(true, false, false), Some(NOT_YOURS));
    assert_eq!(chooser_refusal(true, true, false), Some(NOT_IN_VOICE));
    // A list the bot no longer holds says so, rather than blaming the
    // presser for not being in the voice channel.
    assert_eq!(chooser_refusal(false, false, false), Some(NOT_ON_OFFER));
    assert_eq!(chooser_refusal(false, true, true), Some(NOT_ON_OFFER));
}

#[test]
fn a_failed_pick_is_told_in_a_private_follow_up() {
    // Issue #42: the press is acknowledged before the song is fetched, and a
    // second response to it is rejected by Discord, so a failure was never
    // shown. It is a follow-up, private to the presser, carrying the reason.
    let sent = serde_json::to_value(private_follow_up(
        "I couldn't play that song: could not download that song",
    ))
    .unwrap();
    assert_eq!(
        sent["content"],
        "I couldn't play that song: could not download that song"
    );
    let ephemeral = serenity::model::channel::MessageFlags::EPHEMERAL.bits();
    assert_eq!(sent["flags"].as_u64().unwrap() & ephemeral, ephemeral);
}

#[test]
fn autocomplete_name_includes_artist_title_and_duration() {
    let track = resolve::CatalogTrack {
        artist: "Queen".to_string(),
        title: "Bohemian Rhapsody".to_string(),
        duration_secs: Some(355.0),
        popularity: None,
        video_id: Some("fJ9rUzIMcZQ".to_string()),
    };
    assert_eq!(
        format_autocomplete_name(&track),
        "Queen — Bohemian Rhapsody (5:55)"
    );
}

#[test]
fn autocomplete_name_clamps_to_100_chars() {
    let track = resolve::CatalogTrack {
        artist: "A".repeat(60),
        title: "B".repeat(60),
        duration_secs: Some(120.0),
        popularity: None,
        video_id: Some("123".to_string()),
    };
    let formatted = format_autocomplete_name(&track);
    assert!(formatted.chars().count() <= 100);
    assert!(formatted.ends_with("..."));
}
