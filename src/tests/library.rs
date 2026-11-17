use super::*;

fn track(artist: &str, title: &str) -> Track {
    Track {
        artist: artist.to_string(),
        title: title.to_string(),
        duration_secs: Some(205.0),
        video_id: None,
    }
}

/// A library in a directory of its own, removed when the test ends.
fn scratch(name: &str) -> (PathBuf, Library) {
    let dir = std::env::temp_dir().join(format!("shamash-library-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("library.json");
    (dir.clone(), Library::load(Some(&path)))
}

fn path_of(dir: &Path) -> PathBuf {
    dir.join("library.json")
}

/// The case from the request: "All Caps by MF DOOM" is credited to
/// Madvillain, the listener picks it from the list, and the same words play
/// it next time, after a restart too. The artist that was said never has to
/// match the one that is stored.
#[test]
fn a_pick_is_recalled_even_when_the_artist_said_is_another() {
    let (dir, library) = scratch("pick-other-artist");
    let mut picked = track("Madvillain", "All Caps");
    picked.video_id = Some("playedvideo".to_string());
    library.remember(1, "All Caps by MF DOOM", &picked, true);
    library.save();

    let reopened = Library::load(Some(&path_of(&dir)));
    assert_eq!(reopened.recall("all caps by mf doom!"), Some(picked));
    assert_eq!(reopened.recall("all caps by madvillain"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The table from issue #33: the same request, transcribed a little
/// differently each time, finds the song that was picked.
#[test]
fn a_pick_is_found_when_said_a_little_differently() {
    let (_dir, library) = scratch("said-differently");
    let picked = track("Madvillain", "All Caps");
    library.remember(1, "All Caps by MF DOOM", &picked, true);
    for said in [
        "All Caps by MF DOOM",
        "All Caps by M.F. DOOM",
        "all caps by m f doom",
        "All Caps by MF Dume",
        "All Caps",
    ] {
        assert_eq!(library.recall(said), Some(picked.clone()), "for {said:?}");
    }
}

/// Spelled-out initials are glued back into a word; digits are not.
#[test]
fn single_letters_are_glued_back_together() {
    assert_eq!(phrase_key("All Caps by M.F. DOOM"), "all caps by mf doom");
    assert_eq!(phrase_key("a b c by the jackson 5"), "abc by the jackson 5");
    assert_eq!(phrase_key("i want it that way"), "i want it that way");
    assert_eq!(phrase_key("2 4 6 8"), "2 4 6 8");
    assert_eq!(phrase_key(""), "");
}

/// The phrase is stored glued, so the two spellings are one entry.
#[test]
fn initials_spelled_out_are_the_same_entry() {
    let (_dir, library) = scratch("glued-entry");
    library.remember(
        1,
        "all caps by mf doom",
        &track("Madvillain", "All Caps"),
        true,
    );
    library.remember(
        1,
        "all caps by m f doom",
        &track("Madvillain", "All Caps"),
        true,
    );
    assert_eq!(library.len(), 1);
}

/// Close is not enough when a different song is about as close, or when
/// the difference is a word that changes the song.
#[test]
fn a_different_song_is_not_replaced_by_a_remembered_one() {
    let (_dir, library) = scratch("strict");
    library.remember(
        1,
        "lover by taylor swift",
        &track("Taylor Swift", "Lover"),
        true,
    );
    library.remember(
        1,
        "part 1 by some band",
        &track("Some Band", "Part 1"),
        true,
    );
    // One word apart, and a different song.
    assert!(library.recall("love story by taylor swift").is_none());
    // A different number is a different song, however close the rest.
    assert!(library.recall("part 2 by some band").is_none());
    // A short phrase is only ever found exactly.
    library.remember(1, "hey", &track("Pixies", "Hey"), true);
    assert!(library.recall("hay").is_none());
}

/// Two remembered songs both near what was said: neither is taken.
#[test]
fn a_close_phrase_needs_to_be_clearly_the_closest() {
    let (_dir, library) = scratch("ambiguous");
    library.remember(
        1,
        "dancing queen abba",
        &track("ABBA", "Dancing Queen"),
        true,
    );
    library.remember(
        1,
        "dancing green abba",
        &track("Someone", "Dancing Green"),
        true,
    );
    assert!(library.recall("dancing qween abba").is_none());
}

/// A title alone is used only when exactly one remembered song has it.
#[test]
fn a_title_alone_needs_exactly_one_song_with_it() {
    let (_dir, library) = scratch("title-alone");
    library.remember(1, "hello by adele", &track("Adele", "Hello"), true);
    // Two phrases for the same song are still one song.
    library.remember(1, "adele hello", &track("Adele", "Hello"), true);
    assert_eq!(library.recall("hello"), Some(track("Adele", "Hello")));

    library.remember(
        1,
        "hello lionel richie",
        &track("Lionel Richie", "Hello"),
        true,
    );
    assert!(library.recall("hello").is_none(), "which Hello?");
}

/// A song recalled by a close phrase still counts its quick skips against
/// the phrase that recalled it.
#[test]
fn quick_skips_reach_a_phrase_recalled_by_a_close_one() {
    let (_dir, library) = scratch("close-skips");
    let song = track("Madvillain", "All Caps");
    library.remember(1, "all caps by mf doom", &song, true);
    assert!(!library.quick_skip("all caps by m f doom", &song));
    assert!(library.quick_skip("all caps by mf dume", &song));
    assert!(library.is_empty());
}

/// The whole point: the same misheard phrase costs one lookup the first
/// time and none at all the second.
#[test]
fn a_learned_phrase_is_recalled_exactly() {
    let (_dir, library) = scratch("recall");
    assert!(
        library.is_empty(),
        "nothing is known before anything is learned"
    );
    assert!(library.recall("billy eyelash bad guy").is_none());

    library.remember(
        1,
        "dracula by tame impala",
        &track("Tame Impala", "Dracula"),
        false,
    );
    assert_eq!(
        library.recall("dracula by tame impala"),
        Some(track("Tame Impala", "Dracula"))
    );
    // Case, punctuation and decoration are not part of a phrase, so the
    // same words reach the same entry however they were transcribed.
    for said in [
        "Dracula, by Tame Impala!",
        "dracula  BY  tame   impala",
        "dracula by tame impala (remastered)",
    ] {
        assert_eq!(
            library.recall(said),
            Some(track("Tame Impala", "Dracula")),
            "for {said:?}"
        );
    }
    // A genuinely different mishearing is a different phrase and is not
    // recalled, which is why the catalog and the rewrite still run for it.
    assert!(library.recall("drakoola tame implala").is_none());
}

/// An unrelated phrase must not hit, however similar it looks.
#[test]
fn an_unrelated_phrase_is_not_recalled() {
    let (_dir, library) = scratch("unrelated");
    library.remember(
        1,
        "dracula tame impala",
        &track("Tame Impala", "Dracula"),
        false,
    );
    assert!(library.recall("hooligan bts").is_none());
    assert!(library.recall("").is_none());
}

/// A song learned twice is one entry, not two.
#[test]
fn learning_the_same_phrase_twice_keeps_one_entry() {
    let (_dir, library) = scratch("twice");
    library.remember(
        1,
        "dracula tame impala",
        &track("Tame Impala", "Dracula"),
        false,
    );
    library.remember(
        1,
        "dracula tame impala",
        &track("Tame Impala", "Dracula"),
        false,
    );
    assert_eq!(library.len(), 1);
}

/// A press of a chooser button is believed immediately and never
/// downgraded by a later, weaker piece of evidence.
#[test]
fn a_pressed_choice_is_remembered_and_stays_pressed() {
    let (_dir, library) = scratch("pressed");
    library.remember(1, "stand by me", &track("Oasis", "Stand By Me"), true);
    library.remember(1, "stand by me", &track("Oasis", "Stand By Me"), false);
    assert_eq!(
        library.recall("stand by me"),
        Some(track("Oasis", "Stand By Me"))
    );
}

/// Two quick skips and the bot forgets it was told. This is the only way an
/// alias is removed, and it is the correction for a wrong lesson.
#[test]
fn two_quick_skips_forget_the_phrase() {
    let (_dir, library) = scratch("skips");
    let song = track("Oasis", "Stand By Me");
    library.remember(1, "stand by me", &song, false);

    assert!(
        !library.quick_skip("stand by me", &song),
        "one skip is the listener changing their mind"
    );
    assert_eq!(library.len(), 1, "still believed after one skip");
    assert!(
        library.quick_skip("stand by me", &song),
        "twice is the bot having been taught wrong"
    );
    assert!(library.is_empty());
    assert!(library.recall("stand by me").is_none());
}

/// A quick skip is only feedback about the song it was asked for. Skipping
/// some *other* song must not count against an alias.
#[test]
fn a_quick_skip_of_a_different_song_is_not_feedback() {
    let (_dir, library) = scratch("other-skip");
    library.remember(1, "stand by me", &track("Oasis", "Stand By Me"), false);
    assert!(!library.quick_skip("stand by me", &track("Oasis", "Wonderwall")));
    assert_eq!(library.len(), 1);
}

/// History is fuzzy: a misheard request reaches a song this guild has played
/// before, judged by the same matcher that judges catalog hits.
#[test]
fn history_is_matched_fuzzily_by_the_shared_matcher() {
    let (_dir, library) = scratch("history");
    library.remember(
        1,
        "dracula tame impala",
        &track("Tame Impala", "Dracula"),
        false,
    );

    let hits = library.history_hits(1);
    assert!(!hits.is_empty());
    let verdict = matcher::decide("drakula tame impala", &hits, |hit: &Hit| {
        (hit.artist.as_str(), hit.title.as_str())
    });
    assert!(
        matches!(verdict, matcher::Verdict::Confident(_)),
        "{verdict:?}"
    );
}

/// History is per guild: another server's listener has heard nothing here.
#[test]
fn history_is_per_guild() {
    let (_dir, library) = scratch("guilds");
    library.remember(
        1,
        "dracula tame impala",
        &track("Tame Impala", "Dracula"),
        false,
    );
    // `remember` files history under whichever guild last played; the row
    // exists, and it belongs to exactly one.
    let hits = library.history_hits(999);
    assert!(hits.is_empty(), "another guild has heard nothing here");
}

/// A file that is not there is the first run, not an error.
#[test]
fn a_missing_file_is_not_an_error() {
    let (dir, _) = scratch("missing");
    let library = Library::load(Some(&path_of(&dir)));
    assert!(library.is_empty());
}

/// A corrupt file must not stop the bot. A library is an optimisation.
#[test]
fn a_corrupt_file_starts_empty_rather_than_crashing() {
    let (dir, _) = scratch("corrupt");
    std::fs::create_dir_all(&dir).expect("create dir");
    std::fs::write(path_of(&dir), "{ this is not json").expect("write");

    let library = Library::load(Some(&path_of(&dir)));
    assert!(library.is_empty());
    // And it is still usable afterwards.
    library.remember(1, "dracula", &track("Tame Impala", "Dracula"), false);
    library.save();
    assert_eq!(
        Library::load(Some(&path_of(&dir))).recall("dracula"),
        Some(track("Tame Impala", "Dracula"))
    );
}

/// The whole point of the atomic write: what is on disk is always a
/// complete document, never half of one.
#[test]
fn the_file_is_written_atomically() {
    let (dir, library) = scratch("atomic");
    library.remember(
        1,
        "dracula tame impala",
        &track("Tame Impala", "Dracula"),
        false,
    );
    library.save();

    let text = std::fs::read_to_string(path_of(&dir)).expect("the file is there");
    let file: File = serde_json::from_str(&text).expect("and it is whole");
    assert_eq!(file.aliases.len(), 1);
    assert!(
        !path_of(&dir).with_extension("json.tmp").exists(),
        "the temp file is renamed, not left behind"
    );
}

/// Everything survives a restart, which is the only thing a file is for.
#[test]
fn the_library_survives_a_restart() {
    let (dir, library) = scratch("restart");
    library.remember(1, "hoolig by bts", &track("BTS", "Hooligan"), true);
    library.save();

    let reopened = Library::load(Some(&path_of(&dir)));
    assert_eq!(
        reopened.recall("hoolig by bts"),
        Some(track("BTS", "Hooligan"))
    );
    assert_eq!(reopened.len(), 1);
}

/// The caps exist so a long-running bot cannot grow the file without bound.
#[test]
fn the_file_is_capped_and_drops_the_least_recently_used() {
    let (dir, library) = scratch("capped");
    for i in 0..(MAX_ALIASES + 50) {
        library.remember(
            1,
            &format!("phrase {i}"),
            &track("Someone", &format!("Song {i}")),
            false,
        );
    }
    library.save();

    assert_eq!(library.len(), MAX_ALIASES);
    let reopened = Library::load(Some(&path_of(&dir)));
    assert_eq!(reopened.len(), MAX_ALIASES);
    // The newest survived; the oldest did not.
    assert!(
        reopened
            .recall(&format!("phrase {}", MAX_ALIASES + 49))
            .is_some()
    );
    assert!(reopened.recall("phrase 0").is_none());
}

/// A burst of learns is one write, not one per learn.
#[test]
fn writes_are_debounced() {
    let (dir, library) = scratch("debounced");
    library.remember(1, "dracula", &track("Tame Impala", "Dracula"), false);
    library.maybe_save();
    assert!(!path_of(&dir).exists(), "the write is still pending");

    library.save();
    assert!(path_of(&dir).exists());
    library.maybe_save();
    // Nothing pending, so this is a no-op rather than an error.
    assert!(Library::load(Some(&path_of(&dir))).len() == 1);
}

/// No library configured is a working bot with no memory, not an error.
#[test]
fn no_path_means_no_memory_and_no_panics() {
    let library = Library::load(None);
    library.remember(1, "dracula", &track("Tame Impala", "Dracula"), false);
    library.save();
    library.maybe_save();
    assert!(library.recall("dracula").is_some());
}

/// Nothing is learned from a request that named no song: an unknown has no
/// track to remember.
#[test]
fn nothing_with_no_title_is_learned() {
    let (_dir, library) = scratch("no-title");
    library.remember(
        1,
        "something by adele",
        &Track {
            artist: "Adele".into(),
            title: "  ".into(),
            duration_secs: None,
            video_id: None,
        },
        false,
    );
    assert!(library.is_empty());
    library.remember(1, "", &track("Tame Impala", "Dracula"), false);
    assert!(library.is_empty(), "an empty phrase is not a phrase");
}

/// `forget` is the manual correction.
#[test]
fn a_phrase_can_be_forgotten_by_hand() {
    let (_dir, library) = scratch("forget");
    library.remember(1, "dracula", &track("Tame Impala", "Dracula"), false);
    assert_eq!(
        library.forget("DRACULA!"),
        Some(track("Tame Impala", "Dracula")),
        "it says which song it forgot"
    );
    assert!(
        library.is_empty(),
        "forgotten through the same normalisation"
    );
    assert_eq!(library.recall("dracula"), None);
    assert_eq!(library.forget("dracula"), None, "nothing left to forget");
}
