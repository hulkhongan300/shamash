//! The now-playing dashboard posted to the text channel.
//!
//! An embed with the song title, its thumbnail, and a row of buttons that
//! pause, skip and stop playback. Discord requires a unique `custom_id` per
//! button and echoes it back on every press, which is how [`action_from_id`]
//! recovers the intent.

use crate::queue::Queued;
use serenity::builder::{
    CreateActionRow, CreateButton, CreateEmbed, CreateEmbedAuthor, CreateMessage, EditMessage,
};
use serenity::model::application::ButtonStyle;

/// Custom id prefix for every dashboard button.
///
/// The guild is not encoded, because a press arrives with the guild already
/// attached and the dashboard is only ever shown for the guild it is in.
const PREFIX: &str = "shamash";

/// Buttons, in the order they are shown.
pub const ACTION_PAUSE: &str = "pause";
pub const ACTION_RESUME: &str = "resume";
pub const ACTION_SKIP: &str = "skip";
pub const ACTION_STOP: &str = "stop";

/// The longest a title may be before it is trimmed.
///
/// Discord truncates an embed title at 256 characters anyway, and an over-long
/// one is rejected rather than trimmed, which would lose the whole dashboard.
const MAX_TITLE: usize = 256;
/// The same limit applies to the description and field values.
const MAX_DESCRIPTION: usize = 4096;
/// Longest field name Discord accepts.
const MAX_FIELD_NAME: usize = 256;

/// Builds the custom id for a dashboard button.
fn button_id(action: &str) -> String {
    format!("{PREFIX}:{action}")
}

/// Recovers the intent from a pressed button's custom id.
///
/// Returns `None` for anything that is not a dashboard button, so an
/// interaction from some other part of the bot is ignored rather than
/// misread.
pub fn action_from_id(custom_id: &str) -> Option<&str> {
    let action = custom_id.strip_prefix(&format!("{PREFIX}:"))?;
    match action {
        ACTION_PAUSE | ACTION_RESUME | ACTION_SKIP | ACTION_STOP => Some(action),
        _ => None,
    }
}

/// Shortens `text` to at most `max` characters on a word boundary.
fn trim(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    match cut.rfind(' ') {
        Some(space) => format!("{}\u{2026}", cut[..space].trim_end()),
        None => format!("{cut}\u{2026}"),
    }
}

/// The dashboard view: the embed and the buttons, built apart from the
/// message so the same parts can be used to post it and to edit it.
#[derive(Clone)]
pub struct View {
    embed: CreateEmbed,
    buttons: Vec<CreateActionRow>,
}

impl View {
    /// The message that draws this dashboard for the first time.
    pub fn into_message(self) -> CreateMessage {
        CreateMessage::new()
            .embed(self.embed)
            .components(self.buttons)
    }

    /// The edit that redraws an existing dashboard message.
    pub fn into_edit(self) -> EditMessage {
        EditMessage::new()
            .embed(self.embed)
            .components(self.buttons)
    }
}

/// Builds the dashboard for the song playing now.
///
/// `paused` picks between a pause and a resume button, because a paused song
/// and a playing one need opposite actions.
pub fn dashboard(song: &Queued, paused: bool, pending: usize) -> View {
    let mut embed = CreateEmbed::new()
        .title(trim(&song.title, MAX_TITLE))
        .url(&song.url)
        .thumbnail(song.thumbnail())
        .color(0x1d_b9_54);

    if !song.artist.is_empty() {
        embed = embed.author(CreateEmbedAuthor::new(trim(&song.artist, MAX_FIELD_NAME)));
    }
    embed = embed.field(
        trim("Up next", MAX_FIELD_NAME),
        if pending == 0 {
            "nothing queued".to_string()
        } else {
            format!("{pending} song{}", if pending == 1 { "" } else { "s" })
        },
        false,
    );
    if paused {
        embed = embed.description(trim("Paused", MAX_DESCRIPTION));
    }

    // Labels rather than emoji: serenity only builds a reaction type from a
    // single `char`, which cannot carry the variation selector a button emoji
    // needs, and a word reads as clearly as a glyph in a row of three.
    let (toggle_label, toggle_action) = if paused {
        ("Resume", ACTION_RESUME)
    } else {
        ("Pause", ACTION_PAUSE)
    };

    View {
        embed,
        buttons: vec![CreateActionRow::Buttons(vec![
            CreateButton::new(button_id(toggle_action)).label(toggle_label),
            CreateButton::new(button_id(ACTION_SKIP)).label("Skip"),
            CreateButton::new(button_id(ACTION_STOP))
                .label("Stop")
                .style(ButtonStyle::Danger),
        ])],
    }
}

/// The dashboard shown when there is nothing playing.
pub fn idle_dashboard() -> View {
    View {
        embed: CreateEmbed::new()
            .title("Nothing playing")
            .description("Say the wake word and a song to start the queue.")
            .color(0x2b_2d_31),
        buttons: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::Queued;
    use std::path::PathBuf;

    fn song() -> Queued {
        Queued {
            path: PathBuf::from("/tmp/track.m4a"),
            title: "A Song".to_string(),
            artist: "An Artist".to_string(),
            url: "https://youtu.be/abc".to_string(),
            video_id: "abc".to_string(),
        }
    }

    #[test]
    fn every_button_round_trips_through_its_custom_id() {
        for action in [ACTION_PAUSE, ACTION_RESUME, ACTION_SKIP, ACTION_STOP] {
            let id = button_id(action);
            assert_eq!(action_from_id(&id), Some(action));
        }
    }

    #[test]
    fn a_custom_id_from_elsewhere_is_ignored() {
        // Another feature's button must not be read as a playback action.
        assert_eq!(action_from_id("other:pause"), None);
        assert_eq!(action_from_id("shamash:unknown"), None);
        assert_eq!(action_from_id("shamash:"), None);
        assert_eq!(action_from_id("pause"), None);
        assert_eq!(action_from_id(""), None);
    }

    /// The view as Discord would receive it, for asserting on its contents.
    fn rendered(view: &View) -> String {
        serde_json::to_string(&(&view.embed, &view.buttons)).expect("view serialises")
    }

    #[test]
    fn the_dashboard_carries_the_title_artist_and_thumbnail() {
        let view = dashboard(&song(), false, 0);
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
        let playing = rendered(&dashboard(&song(), false, 0));
        assert!(playing.contains("Pause"), "{playing}");
        assert!(!playing.contains("Resume"), "{playing}");

        let paused = rendered(&dashboard(&song(), true, 0));
        assert!(paused.contains("Resume"), "{paused}");
        assert!(
            paused.contains("Paused"),
            "the state must be visible: {paused}"
        );
    }

    #[test]
    fn the_queue_length_is_summarised() {
        let empty = rendered(&dashboard(&song(), false, 0));
        assert!(empty.contains("nothing queued"), "{empty}");

        let one = rendered(&dashboard(&song(), false, 1));
        assert!(one.contains("1 song"), "{one}");

        let many = rendered(&dashboard(&song(), false, 3));
        assert!(many.contains("3 songs"), "{many}");
    }

    #[test]
    fn an_over_long_title_is_trimmed_rather_than_rejected() {
        // Discord rejects an embed whose title is over the limit, which would
        // lose the whole dashboard.
        let mut long = song();
        long.title = "x".repeat(400);
        let json = serde_json::to_string(&dashboard(&long, false, 0).embed).expect("serialises");
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
    fn a_title_with_no_spaces_still_fits() {
        let trimmed = trim(&"y".repeat(50), 10);
        assert_eq!(trimmed.chars().count(), 10);
    }
}
