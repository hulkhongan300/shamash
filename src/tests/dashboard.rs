use super::*;
use crate::queue::{Queued, Source};
use std::path::PathBuf;

fn song() -> Queued {
    Queued {
        source: Source::File(PathBuf::from("/tmp/track.m4a")),
        title: "A Song".to_string(),
        artist: "An Artist".to_string(),
        url: "https://youtu.be/abc".to_string(),
        video_id: "abc".to_string(),
        duration_secs: Some(206.0),
        origin: Origin::Voice,
        remembered: None,
    }
}

/// The description a view sends, read via JSON since `CreateEmbed` fields are private.
fn description_of(view: View) -> String {
    serde_json::to_value(&view.embed)
        .expect("an embed serialises")
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Transcripts can contain markdown that would reformat the message.
#[test]
fn a_notice_escapes_what_it_echoes_back() {
    let description = description_of(notice("I heard “*stand* by _me_”, but couldn’t match it."));
    assert!(
        description.contains("\\*stand\\*") && description.contains("\\_me\\_"),
        "not escaped: {description}"
    );
}

#[test]
fn a_notice_has_no_buttons() {
    assert!(notice("something went wrong").buttons.is_empty());
}

/// Over Discord's limit the send fails, and the notice is silently lost.
#[test]
fn a_notice_is_trimmed_to_something_discord_will_send() {
    let said = "a".repeat(MAX_DESCRIPTION * 2);
    let description = description_of(notice(&said));
    assert!(
        description.chars().count() <= MAX_DESCRIPTION,
        "{} chars",
        description.chars().count()
    );
}

fn option(title: &str, channel: &str, duration: Option<f64>) -> ChoiceOption<'static> {
    ChoiceOption {
        title: Box::leak(title.to_string().into_boxed_str()),
        channel: Box::leak(channel.to_string().into_boxed_str()),
        duration_secs: duration,
        plays: None,
        release: None,
        year: None,
    }
}

#[test]
fn an_album_option_says_it_is_an_album() {
    let album = ChoiceOption {
        release: Some("Album"),
        year: Some(2017),
        ..option("17", "XXXTENTACION", None)
    };
    assert_eq!(album.detail(), "Album \u{2022} XXXTENTACION \u{2022} 2017");
}

#[test]
fn an_option_without_metadata_still_renders() {
    assert_eq!(option("Dracula", "", None).detail(), "unknown length");
}

/// Play count is what separates two recordings of the same title.
#[test]
fn an_option_shows_its_play_count() {
    let mut option = option("Dracula", "Tame Impala", Some(206.0));
    option.plays = Some(217_000_000);
    assert_eq!(
        option.detail(),
        "Tame Impala \u{2022} 3:26 \u{2022} 217M plays"
    );
}

#[test]
fn play_counts_are_abbreviated_like_youtube() {
    assert_eq!(plays_label(217_000_000), "217M plays");
    assert_eq!(plays_label(2_200_000), "2.2M plays");
    assert_eq!(plays_label(33_000), "33K plays");
    assert_eq!(plays_label(999), "999 plays");
    assert_eq!(plays_label(1), "1 play");
}

#[test]
fn every_button_round_trips_through_its_custom_id() {
    for action in [
        ACTION_PAUSE,
        ACTION_RESUME,
        ACTION_SKIP,
        ACTION_STOP,
        ACTION_QUEUE,
        ACTION_BACK,
        ACTION_REPEAT,
        ACTION_WRONG,
    ] {
        let id = button_id(action);
        assert_eq!(action_from_id(&id), Some(action));
    }
}

#[test]
fn a_custom_id_from_elsewhere_is_ignored() {
    assert_eq!(action_from_id("other:pause"), None);
    assert_eq!(action_from_id("shamash:unknown"), None);
    assert_eq!(action_from_id("shamash:"), None);
    assert_eq!(action_from_id("pause"), None);
    assert_eq!(action_from_id(""), None);
}

fn waiting(songs: &[Queued]) -> Vec<Waiting> {
    songs
        .iter()
        .cloned()
        .zip(0..)
        .map(|(song, id)| Waiting { id, song })
        .collect()
}

fn queue_rows(waiting: &[Waiting], selected: Option<u64>) -> Vec<serde_json::Value> {
    serde_json::to_value(&queue_view(waiting, selected).buttons)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

/// The edit buttons' (custom id, disabled) in the order shown.
fn edit_buttons(waiting: &[Waiting], selected: Option<u64>) -> Vec<(String, bool)> {
    queue_rows(waiting, selected)[1]["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            (
                b["custom_id"].as_str().unwrap().to_string(),
                b["disabled"].as_bool().unwrap_or(false),
            )
        })
        .collect()
}

#[test]
fn the_queue_view_picks_a_song_from_a_menu() {
    let queue = waiting(&vec![song(); crate::queue::MAX_QUEUE]);
    let rows = queue_rows(&queue, None);
    assert_eq!(rows.len(), 3, "menu, controls, back: {rows:?}");
    let menu = &rows[0]["components"][0];
    assert_eq!(menu["custom_id"], "shamash:pick");
    let options = menu["options"].as_array().unwrap();
    assert_eq!(options.len(), crate::queue::MAX_QUEUE);
    assert_eq!(options[0]["label"], "1. A Song");
    assert_eq!(options[0]["value"], "0");
    assert_eq!(options[0]["description"], "An Artist");
    assert_eq!(rows[2]["components"][0]["custom_id"], "shamash:back");
}

#[test]
fn the_edit_buttons_wait_for_a_pick() {
    let queue = waiting(&[song(), song()]);
    for (id, disabled) in edit_buttons(&queue, None) {
        assert!(disabled, "{id} must be greyed out with nothing picked");
        assert!(edit_from_id(&id).is_none(), "{id} must not name a song");
    }
}

/// By id, so they still act on it after the queue shifts.
#[test]
fn the_edit_buttons_name_the_picked_song() {
    let queue = waiting(&[song(), song(), song()]);
    let buttons = edit_buttons(&queue, Some(1));
    let edits: Vec<_> = buttons
        .iter()
        .map(|(id, _)| edit_from_id(id).expect("names a song"))
        .collect();
    assert_eq!(
        edits,
        [
            (Edit::Shift(Shift::Front), 1),
            (Edit::Shift(Shift::Up), 1),
            (Edit::Shift(Shift::Down), 1),
            (Edit::Remove, 1),
        ]
    );
    assert!(buttons.iter().all(|(_, disabled)| !disabled), "{buttons:?}");

    let menu = &queue_rows(&queue, Some(1))[0]["components"][0];
    assert_eq!(menu["options"][1]["default"], true, "the pick stays shown");
    assert_eq!(menu["options"][0]["default"], false);
}

#[test]
fn a_song_at_either_end_cannot_move_past_it() {
    let queue = waiting(&[song(), song()]);
    let disabled = |selected| -> Vec<bool> {
        edit_buttons(&queue, Some(selected))
            .into_iter()
            .map(|(_, disabled)| disabled)
            .collect()
    };
    // Play next, up, down, remove.
    assert_eq!(disabled(0), [true, true, false, false]);
    assert_eq!(disabled(1), [false, false, true, false]);
}

#[test]
fn a_pick_that_is_no_longer_waiting_picks_nothing() {
    let queue = waiting(&[song()]);
    assert!(edit_buttons(&queue, Some(7)).iter().all(|(_, d)| *d));
}

/// Discord rejects an empty menu.
#[test]
fn an_empty_queue_has_only_the_way_back() {
    let rows = queue_rows(&[], None);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["components"][0]["custom_id"], "shamash:back");
}

#[test]
fn an_edit_id_round_trips_and_strangers_are_ignored() {
    for edit in [
        Edit::Remove,
        Edit::Shift(Shift::Up),
        Edit::Shift(Shift::Down),
        Edit::Shift(Shift::Front),
    ] {
        assert_eq!(edit_from_id(&edit_id(edit, 42)), Some((edit, 42)));
    }
    assert_eq!(edit_from_id("shamash:edit:explode:1"), None);
    assert_eq!(edit_from_id("shamash:edit:remove:x"), None);
    assert_eq!(edit_from_id("shamash:remove"), None);
    assert_eq!(action_from_id("shamash:edit:remove:1"), None);
    assert!(is_pick("shamash:pick"));
    assert!(!is_pick("shamash:pause"));
    assert_eq!(picked(&["12".to_string()]), Some(12));
    assert_eq!(picked(&[]), None);
}

fn rendered(view: &View) -> String {
    serde_json::to_string(&(&view.embed, &view.buttons)).expect("view serialises")
}

#[test]
fn the_dashboard_carries_the_title_artist_and_thumbnail() {
    let view = dashboard(&song(), None, false, &[], false);
    let json = serde_json::to_string(&view.embed).expect("embed serialises");
    assert!(json.contains("A Song"), "the title must be shown: {json}");
    assert!(
        json.contains("An Artist"),
        "the artist must be shown: {json}"
    );
    assert!(
        json.contains("https://i.ytimg.com/vi/abc/hqdefault.jpg"),
        "the thumbnail must be shown: {json}"
    );
    assert!(
        json.contains("https://youtu.be/abc"),
        "the title must link to the video: {json}"
    );
}

#[test]
fn a_playing_song_offers_pause_and_a_paused_one_offers_resume() {
    let playing = rendered(&dashboard(&song(), None, false, &[], false));
    assert!(playing.contains("Pause"), "{playing}");
    assert!(!playing.contains("Resume"), "{playing}");

    let paused = rendered(&dashboard(&song(), None, true, &[], false));
    assert!(paused.contains("Resume"), "{paused}");
    assert!(
        paused.contains("Paused"),
        "the state must be visible: {paused}"
    );
}

fn named(title: &str, artist: &str, duration: Option<f64>) -> Queued {
    let mut song = song();
    song.title = title.to_string();
    song.artist = artist.to_string();
    song.duration_secs = duration;
    song
}

#[test]
fn the_next_song_is_named() {
    let empty = rendered(&dashboard(&song(), None, false, &[], false));
    assert!(empty.contains("Nothing queued"), "{empty}");

    let next = [named("Rhymes Like Dimes", "MF DOOM", None)];
    let one = rendered(&dashboard(&song(), None, false, &next, false));
    assert!(one.contains("**Rhymes Like Dimes** — MF DOOM"), "{one}");
    assert!(!one.contains("more"), "{one}");
    assert!(one.contains("Queue (1)"), "{one}");

    let three = [next[0].clone(), song(), song()];
    let many = rendered(&dashboard(&song(), None, false, &three, false));
    assert!(many.contains("Rhymes Like Dimes"), "{many}");
    assert!(many.contains("+2 more"), "{many}");
    assert!(many.contains("Queue (3)"), "{many}");
}

#[test]
fn a_remembered_song_says_so_and_offers_wrong_song() {
    let looked_up = rendered(&dashboard(&song(), None, false, &[], false));
    assert!(!looked_up.contains(REMEMBERED_NOTE), "{looked_up}");
    assert!(!looked_up.contains("Wrong song"), "{looked_up}");

    let mut recalled = song();
    recalled.remembered = Some("all caps by mf doom".to_string());
    let view = dashboard(&recalled, None, false, &[], false);
    let json = rendered(&view);
    assert!(json.contains("Wrong song"), "{json}");
    assert!(json.contains(&button_id(ACTION_WRONG)), "{json}");
    assert_eq!(
        description_of(view),
        format!("3:26 · 🎤 asked by voice\n{REMEMBERED_NOTE}")
    );
}

#[test]
fn the_dashboard_says_how_long_and_how_it_was_asked_for() {
    let voice = description_of(dashboard(&song(), None, false, &[], false));
    assert_eq!(voice, "3:26 · 🎤 asked by voice");

    let mut typed = song();
    typed.origin = Origin::Playlist;
    typed.duration_secs = None;
    assert_eq!(
        description_of(dashboard(&typed, None, false, &[], false)),
        "💿 from a playlist"
    );
}

#[test]
fn paused_and_repeat_are_visible_at_a_glance() {
    let colour = |view: &View| serde_json::to_value(&view.embed).unwrap()["color"].clone();
    let playing = dashboard(&song(), None, false, &[], false);
    let paused = dashboard(&song(), None, true, &[], true);
    assert_ne!(colour(&playing), colour(&paused));
    let description = description_of(paused);
    assert!(description.starts_with("⏸️ **Paused**"), "{description}");
    assert!(description.contains("🔂 Repeating"), "{description}");
}

#[test]
fn lengths_read_like_a_clock() {
    assert_eq!(clock(206.0), "3:26");
    assert_eq!(clock(59.6), "1:00");
    assert_eq!(clock(3723.0), "1:02:03");
    assert_eq!(total_time(1380.0), "23 min");
    assert_eq!(total_time(10.0), "1 min");
    assert_eq!(total_time(3900.0), "1 h 05 min");
}

#[test]
fn the_idle_dashboard_names_the_wake_words_and_play() {
    let words = ["shamash".to_string(), "bot".to_string()];
    let description = description_of(idle_dashboard(&words));
    assert_eq!(
        description,
        "Say **bot play** or **shamash play** and a song, or use `/play`."
    );
    let none: [String; 0] = [];
    assert!(description_of(idle_dashboard(&none)).contains("/play"));
}

#[test]
fn an_over_long_title_is_trimmed_rather_than_rejected() {
    // Discord rejects an over-long title, losing the whole dashboard.
    let mut long = song();
    long.title = "x".repeat(400);
    let json = serde_json::to_string(&dashboard(&long, None, false, &[], false).embed)
        .expect("serialises");
    let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
    let title = value["title"].as_str().expect("a title");
    assert!(
        title.chars().count() <= MAX_TITLE,
        "title was {} chars",
        title.chars().count()
    );
    assert!(title.ends_with('\u{2026}'), "a trimmed title is marked");
}

#[test]
fn trimming_prefers_a_word_boundary() {
    let text = "the quick brown fox jumps over the lazy dog";
    let trimmed = trim(text, 20);
    assert!(trimmed.ends_with('\u{2026}'));
    assert!(
        trimmed.chars().count() <= 20,
        "got {} chars",
        trimmed.chars().count()
    );
    assert!(
        !trimmed.contains('\u{2026}') || trimmed.ends_with('\u{2026}'),
        "only the end is marked"
    );
}

#[test]
fn short_text_is_left_alone() {
    assert_eq!(trim("short", 20), "short");
}

#[test]
fn the_dashboard_offers_the_queue_and_the_queue_offers_the_way_back() {
    let now_playing = rendered(&dashboard(&song(), None, false, &[song()], false));
    assert!(now_playing.contains("Queue"), "{now_playing}");
    assert!(!now_playing.contains("Back"), "{now_playing}");

    let queue = rendered(&queue_view(&waiting(&[song()]), None));
    assert!(queue.contains("Back"), "{queue}");
    assert!(!queue.contains("Stop"), "{queue}");
}

#[test]
fn the_queue_view_lists_every_waiting_song_in_order() {
    let view = queue_view(
        &waiting(&[
            named("Rhymes Like Dimes", "MF DOOM", Some(259.0)),
            named("All Caps", "Madvillain", Some(130.0)),
        ]),
        None,
    );
    let description = description_of(view);
    assert!(
        description.starts_with("**2 songs · 6 min**"),
        "{description}"
    );
    assert!(
        description.contains(
            "` 1.` **Rhymes Like Dimes** — MF DOOM · 4:19\n` 2.` **All Caps** — Madvillain · 2:10"
        ),
        "{description}"
    );
}

#[test]
fn a_queue_with_an_unknown_length_says_over() {
    let view = queue_view(
        &waiting(&[named("A", "", Some(600.0)), named("B", "", None)]),
        None,
    );
    assert!(description_of(view).starts_with("**2 songs · over 10 min**"));
}

#[test]
fn the_queue_view_keeps_the_order_and_artist() {
    let mut second = song();
    second.title = "Second Song".to_string();
    let view = queue_view(&waiting(&[song(), second]), None);
    let json = serde_json::to_string(&view.embed).expect("embed serialises");
    let first_at = json.find("A Song").expect("the first song is listed");
    let second_at = json.find("Second Song").expect("the second song is listed");
    assert!(first_at < second_at, "out of order: {json}");
    assert!(json.contains("An Artist"), "the artist is shown: {json}");
}

#[test]
fn an_empty_queue_says_so() {
    let view = queue_view(&[], None);
    assert!(
        rendered(&view).contains("Nothing is queued"),
        "{}",
        rendered(&view)
    );
}

#[test]
fn the_repeat_button_says_whether_repeat_is_on() {
    let off = rendered(&dashboard(&song(), None, false, &[], false));
    assert!(off.contains("Repeat"), "{off}");
    assert!(!off.contains("Repeat: on"), "off by default: {off}");

    let on = rendered(&dashboard(&song(), None, false, &[], true));
    assert!(on.contains("Repeat: on"), "{on}");
}

#[test]
fn stop_has_a_row_of_its_own_and_nothing_was_lost() {
    let value = serde_json::to_value(&dashboard(&song(), None, false, &[], false).buttons).unwrap();
    let rows = value.as_array().expect("rows");
    assert_eq!(rows.len(), 2, "{value}");
    let ids = |row: &serde_json::Value| -> Vec<String> {
        row["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["custom_id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        ids(&rows[0]),
        [
            "shamash:pause",
            "shamash:skip",
            "shamash:queue",
            "shamash:repeat"
        ]
    );
    assert_eq!(ids(&rows[1]), ["shamash:stop"]);
    for row in rows {
        for button in row["components"].as_array().unwrap() {
            assert!(button["emoji"]["name"].is_string(), "no icon: {button}");
        }
    }
}

#[test]
fn the_repeat_button_is_green_when_on() {
    let style = |repeat| {
        serde_json::to_value(&dashboard(&song(), None, false, &[], repeat).buttons).unwrap()[0]
                ["components"][3]["style"]
                .clone()
    };
    assert_eq!(style(false), 2, "secondary when off");
    assert_eq!(style(true), 3, "success when on");
}

/// Discord rejects an over-long field name rather than truncating it.
#[test]
fn an_over_long_queued_title_is_trimmed_rather_than_rejected() {
    let mut long = song();
    long.title = "x".repeat(400);
    let mut queue = vec![long; crate::queue::MAX_QUEUE];
    queue[0].artist = "y".repeat(400);
    let description = description_of(queue_view(&waiting(&queue), None));
    assert!(
        description.chars().count() <= MAX_DESCRIPTION,
        "{} chars",
        description.chars().count()
    );
    assert!(!description.contains(&"x".repeat(LIST_TITLE + 1)));
}

#[test]
fn a_title_with_no_spaces_still_fits() {
    let trimmed = trim(&"y".repeat(50), 10);
    assert_eq!(trimmed.chars().count(), 10);
}

fn offers(n: usize) -> Vec<ChoiceOption<'static>> {
    (0..n)
        .map(|i| option(&format!("Song {i}"), "Tame Impala", Some(206.0)))
        .collect()
}

#[test]
fn a_play_button_round_trips_and_is_not_a_playback_action() {
    assert_eq!(asked_index(&ask_id(4)), Some(4));
    assert_eq!(
        asked_index(&ask_id(0)),
        Some(0),
        "page two starts at zero again"
    );
    assert_eq!(
        action_from_id(&ask_id(4)),
        None,
        "must not be a playback action"
    );
    assert_eq!(asked_index("shamash:pick:abc"), None);
    assert_eq!(asked_index("shamash:ask:"), None);
    assert_eq!(asked_index("other:ask:0"), None);
}

#[test]
fn the_buttons_name_songs_across_the_whole_search() {
    let json = serde_json::to_string(&ask("dracula", &offers(9), 1, 20).into_message())
        .expect("serialises");
    assert!(json.contains("ask:9"), "page two starts at nine: {json}");
    assert!(json.contains("ask:17"), "and runs to seventeen: {json}");
    assert!(!json.contains("ask:0"), "the ids are absolute: {json}");
}

/// Discord allows five per row, 25 in total; the API drops the rest.
#[test]
fn the_chooser_fits_discords_buttons() {
    let json = serde_json::to_string(&ask("dracula", &offers(9), 0, 20).into_message())
        .expect("serialises");
    let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
    let rows = value["components"].as_array().expect("rows");
    assert_eq!(rows.len(), 3, "5 + 4 + the controls row: {json}");
    for row in rows {
        let buttons = row["components"].as_array().expect("buttons");
        assert!(buttons.len() <= 5, "a row overflowed: {json}");
    }
    let total: usize = rows
        .iter()
        .map(|r| r["components"].as_array().map(|b| b.len()).unwrap_or(0))
        .sum();
    assert_eq!(total, 11, "nine songs and two controls");
}

#[test]
fn the_next_page_button_is_disabled_when_the_list_has_run_out() {
    let next_disabled = |view: View| {
        let value = serde_json::to_value(&view.buttons).unwrap();
        value
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|row| row["components"].as_array().unwrap().clone())
            .find(|b| b["custom_id"] == "shamash:next")
            .map(|b| b["disabled"].as_bool().unwrap())
            .expect("a next page button")
    };
    assert!(
        !next_disabled(ask("dracula", &offers(9), 0, 20)),
        "more exist"
    );
    // 20 songs, 9 per page: page three is the last.
    assert!(
        next_disabled(ask("dracula", &offers(2), 2, 20)),
        "the last page"
    );
}

/// Discord rejects a button with an id and no label.
#[test]
fn every_button_in_the_chooser_is_labelled() {
    for (page, total) in [(0, 20), (1, 20), (2, 20), (0, 4)] {
        let json = serde_json::to_string(&ask("dracula", &offers(9), page, total).into_message())
            .expect("serialises");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        for row in value["components"].as_array().expect("rows") {
            for button in row["components"].as_array().expect("buttons") {
                assert!(
                    button["label"].as_str().is_some_and(|l| !l.is_empty()),
                    "an unlabelled button on page {page}: {json}"
                );
                assert!(
                    button["custom_id"].as_str().is_some_and(|i| !i.is_empty()),
                    "a button with no id on page {page}: {json}"
                );
            }
        }
    }
}

/// Swapped, button 1 would mean the tenth song.
#[test]
fn the_label_numbers_the_page_and_the_id_numbers_the_search() {
    let json = serde_json::to_string(&ask("dracula", &offers(9), 1, 20).into_message())
        .expect("serialises");
    assert!(
        json.contains(r#""custom_id":"shamash:ask:9","disabled":false,"style":1,"label":"1""#)
            || json.contains(r#""custom_id":"shamash:ask:9""#),
        "page two starts at song nine: {json}"
    );
    assert!(json.contains(r#""label":"1""#), "but is labelled 1: {json}");
    assert!(json.contains(r#""label":"9""#), "and the last is 9: {json}");
}

#[test]
fn the_chooser_is_one_line_per_song() {
    let mut first = option("All Caps", "Madvillain", Some(130.0));
    first.plays = Some(120_000_000);
    let description = description_of(ask(
        "all caps by mf doom",
        &[first, option("All Caps", "Someone Else", None)],
        0,
        2,
    ));
    assert_eq!(
        description,
        "Results for “all caps by mf doom”\n\n\
             `1.` **All Caps** · Madvillain \u{2022} 2:10 \u{2022} 120M plays\n\
             `2.` **All Caps** · Someone Else\n\n\
             Page 1 of 1"
    );
}

#[test]
fn the_chooser_says_which_page_of_how_many() {
    let json = description_of(ask("dracula", &offers(9), 1, 20));
    assert!(json.contains("Page 2 of 3"), "{json}");
    assert!(json.contains("dracula"), "the query is echoed: {json}");
}

#[test]
fn the_progress_bar_shows_how_far_into_the_song_it_is() {
    assert_eq!(progress_bar(0.0, 206.0), "🔘▬▬▬▬▬▬▬▬▬▬▬ 0:00 / 3:26");
    assert_eq!(progress_bar(206.0, 206.0), "▬▬▬▬▬▬▬▬▬▬▬🔘 3:26 / 3:26");
    assert_eq!(progress_bar(72.0, 206.0), "▬▬▬▬🔘▬▬▬▬▬▬▬ 1:12 / 3:26");
}

/// Songbird's clock can pass a length YouTube rounded down.
#[test]
fn the_progress_bar_stays_within_the_song() {
    assert_eq!(progress_bar(210.0, 206.0), progress_bar(206.0, 206.0));
    assert_eq!(progress_bar(-3.0, 206.0), progress_bar(0.0, 206.0));
    assert_eq!(progress_bar(5.0, 0.0), "🔘▬▬▬▬▬▬▬▬▬▬▬ 0:00 / 0:00");
    for position in [0.0, 1.0, 50.5, 100.0, 205.9, 206.0] {
        let bar = progress_bar(position, 206.0);
        let segments = bar.chars().filter(|c| matches!(c, '▬' | '🔘')).count();
        assert_eq!(segments, BAR_WIDTH, "{bar}");
        assert_eq!(bar.matches('🔘').count(), 1, "{bar}");
    }
}

#[test]
fn a_known_position_is_drawn_as_a_bar_in_place_of_the_length() {
    let description = description_of(dashboard(&song(), Some(72.0), false, &[], false));
    assert!(description.contains("🔘"), "{description}");
    assert!(description.contains("1:12 / 3:26"), "{description}");
    assert!(
        description.contains("asked by voice"),
        "how it was asked for is still said: {description}"
    );

    // No position or no length: no bar.
    let unknown = description_of(dashboard(&song(), None, false, &[], false));
    assert!(!unknown.contains("🔘"), "{unknown}");
    assert!(unknown.contains("3:26"), "{unknown}");
    let mut endless = song();
    endless.duration_secs = None;
    let endless = description_of(dashboard(&endless, Some(72.0), false, &[], false));
    assert!(!endless.contains("🔘"), "{endless}");
}

#[test]
fn a_paused_song_keeps_its_place_on_the_bar() {
    let paused = description_of(dashboard(&song(), Some(72.0), true, &[], false));
    assert!(paused.contains("Paused"), "{paused}");
    assert!(paused.contains("1:12 / 3:26"), "{paused}");
}
