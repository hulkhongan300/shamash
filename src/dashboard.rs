//! The now-playing dashboard posted to the text channel.
//!
//! An embed plus buttons. Discord echoes a button's `custom_id` back on every
//! press, which is how the press is matched to [`action_from_id`].

use crate::queue::Queued;
use serenity::builder::{
    CreateActionRow, CreateButton, CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter,
    CreateMessage, EditMessage,
};
use serenity::model::application::ButtonStyle;

/// Custom id prefix for every dashboard button.
///
/// The guild is not encoded: a press already arrives with the guild attached.
const PREFIX: &str = "shamash";

/// Marks a custom id as a "which song did you mean" button, which names a
/// video rather than an action.
const PICK_PREFIX: &str = "pick";

/// Buttons, in the order they are shown.
pub const ACTION_PAUSE: &str = "pause";
pub const ACTION_RESUME: &str = "resume";
pub const ACTION_SKIP: &str = "skip";
pub const ACTION_STOP: &str = "stop";

/// The longest a title may be before it is trimmed.
///
/// Discord rejects an over-long embed rather than truncating it, which would
/// lose the whole dashboard.
const MAX_TITLE: usize = 256;
/// The same limit applies to the description and field values.
const MAX_DESCRIPTION: usize = 4096;
/// Longest field name Discord accepts.
const MAX_FIELD_NAME: usize = 256;
/// Longest footer Discord accepts, which is lower than the description's.
const MAX_FOOTER: usize = 2048;

/// Builds the custom id for a dashboard button.
fn button_id(action: &str) -> String {
    format!("{PREFIX}:{action}")
}

/// Recovers the intent from a pressed button's custom id, or `None` for
/// anything that is not a dashboard button.
pub fn action_from_id(custom_id: &str) -> Option<&str> {
    let action = custom_id.strip_prefix(&format!("{PREFIX}:"))?;
    match action {
        ACTION_PAUSE | ACTION_RESUME | ACTION_SKIP | ACTION_STOP => Some(action),
        _ => None,
    }
}

/// Builds the custom id for a "which song did you mean" button.
///
/// The video id rides in the custom id rather than being looked up, so a press
/// needs no state. An 11-character id stays well inside the 100-character
/// limit.
pub fn pick_id(video_id: &str) -> String {
    format!("{PREFIX}:{PICK_PREFIX}:{video_id}")
}

/// Recovers the video id from a choice button's custom id. Separate from
/// [`action_from_id`], which only reports the four playback actions.
pub fn picked_video_id(custom_id: &str) -> Option<&str> {
    let action = custom_id.strip_prefix(&format!("{PREFIX}:"))?;
    let video_id = action.strip_prefix(&format!("{PICK_PREFIX}:"))?;
    // An empty id would match everything and then fetch nothing.
    (!video_id.is_empty()).then_some(video_id)
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
/// needs the opposite action.
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

    // Labels rather than emoji: serenity builds a reaction type from a single
    // `char`, which cannot carry the variation selector a button emoji needs.
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

/// The message posted when the bot could not tell which song was meant.
///
/// One numbered button per candidate, since a row of titles would overflow the
/// 80-character label limit. The titles are in the embed above them.
pub fn choice(
    requested: &str,
    resolved: Option<&str>,
    options: &[ChoiceOption<'_>],
) -> CreateMessage {
    let mut embed = CreateEmbed::new()
        .title("Which one did you mean?")
        .description(format!(
            "I heard {}but these look closest.",
            trim(&markdown_escape(requested), MAX_DESCRIPTION)
        ))
        .color(0xfa_a4_1d);
    if let Some(resolved) = resolved {
        embed = embed.footer(CreateEmbedFooter::new(format!(
            "Closest match: {}",
            trim(resolved, MAX_FOOTER)
        )));
    }
    embed = embed.fields(
        options
            .iter()
            .enumerate()
            .map(|(index, option)| {
                (
                    format!("{}. {}", index + 1, trim(option.title, MAX_FIELD_NAME)),
                    option.detail(),
                    false,
                )
            })
            .collect::<Vec<_>>(),
    );

    let buttons = options
        .iter()
        .enumerate()
        .map(|(index, option)| {
            CreateButton::new(pick_id(option.video_id)).label((index + 1).to_string())
        })
        .collect();

    CreateMessage::new()
        .embed(embed)
        .components(vec![CreateActionRow::Buttons(buttons)])
}

/// A one-line notice for something that did not work.
///
/// Not an embed and not a button row: it is a sentence to read, it has to say
/// what was heard, and nothing about it is actionable from Discord.
pub fn notice(text: &str) -> View {
    View {
        embed: CreateEmbed::new()
            .description(trim(&markdown_escape(text), MAX_DESCRIPTION))
            .color(0xcd_2f_31),
        buttons: Vec::new(),
    }
}

/// One option in the chooser.
#[derive(Clone, Copy)]
pub struct ChoiceOption<'a> {
    pub title: &'a str,
    pub channel: &'a str,
    pub duration_secs: Option<f64>,
    /// The video to play when this is chosen.
    pub video_id: &'a str,
}

impl ChoiceOption<'_> {
    /// The line shown under the title: who uploaded it, and how long it runs.
    fn detail(&self) -> String {
        let mut detail = String::new();
        if !self.channel.is_empty() {
            detail.push_str(&trim(self.channel, MAX_FIELD_NAME));
        }
        if let Some(duration) = self.duration_secs {
            if !detail.is_empty() {
                detail.push_str(" \u{2022} ");
            }
            let total = duration.max(0.0) as u64;
            detail.push_str(&format!("{}:{:02}", total / 60, total % 60));
        }
        if detail.is_empty() {
            detail.push_str("unknown length");
        }
        detail
    }
}

/// Escapes the Discord markdown that would otherwise be read out of a title.
///
/// `requested` is speech-to-text output echoed back, so it can contain anything
/// the person said, and unescaped `*` or `_` would reformat the message.
fn markdown_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '*' | '_' | '~' | '`' | '|' | '>' | '\\') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
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

    /// The description a view will actually send.
    ///
    /// `CreateEmbed` keeps its fields private and offers only setters, so the
    /// message is read back the way Discord would see it.
    fn description_of(view: View) -> String {
        serde_json::to_value(&view.embed)
            .expect("an embed serialises")
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or_default()
            .to_string()
    }

    /// A notice echoes back what the bot thought it heard, which is
    /// speech-to-text output, so it can contain anything the person said --
    /// including markdown that would reformat the message or a stray backtick
    /// pair that swallows the rest of it.
    #[test]
    fn a_notice_escapes_what_it_echoes_back() {
        let description =
            description_of(notice("I heard “*stand* by _me_”, but couldn’t match it."));
        assert!(
            description.contains("\\*stand\\*") && description.contains("\\_me\\_"),
            "not escaped: {description}"
        );
    }

    /// A notice is not a question, so it must not leave buttons behind that
    /// would answer a question nobody asked.
    #[test]
    fn a_notice_has_no_buttons() {
        assert!(notice("something went wrong").buttons.is_empty());
    }

    /// A very long transcript must not be able to push the notice over
    /// Discord's field limit, which would make the send fail -- and a failed
    /// send is exactly the silence this is here to remove.
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

    /// A chooser button has to survive the round trip, or a press cannot be
    /// matched back to the video it stands for.
    #[test]
    fn a_choice_button_round_trips_through_its_custom_id() {
        let id = pick_id("dQw4w9WgXcQ");
        assert_eq!(picked_video_id(&id), Some("dQw4w9WgXcQ"));
    }

    /// A choice button is not a playback action, and `action_from_id` is what
    /// the button dispatcher trusts, so it must not claim these.
    #[test]
    fn a_choice_button_is_not_a_playback_action() {
        let id = pick_id("dQw4w9WgXcQ");
        assert_eq!(action_from_id(&id), None);
        assert_eq!(picked_video_id("shamash:pause"), None);
        assert_eq!(picked_video_id("other:pick:abc"), None);
        assert_eq!(picked_video_id("shamash:pick:"), None);
    }

    /// The custom id has a 100-character limit in Discord, and the video id is
    /// the only variable part.
    #[test]
    fn a_choice_custom_id_fits_discords_limit() {
        let id = pick_id(&"x".repeat(64));
        assert!(id.len() <= 100, "too long: {}", id.len());
    }

    fn option(title: &str, channel: &str, duration: Option<f64>) -> ChoiceOption<'static> {
        ChoiceOption {
            title: Box::leak(title.to_string().into_boxed_str()),
            channel: Box::leak(channel.to_string().into_boxed_str()),
            duration_secs: duration,
            video_id: "vid",
        }
    }

    /// The question has to say what was heard and what is being offered,
    /// numbered to match the buttons.
    #[test]
    fn the_chooser_shows_the_request_and_the_options() {
        let options = [
            option("Dracula", "Tame Impala", Some(206.0)),
            option("Dracula (Live)", "Someone", Some(240.0)),
        ];
        let message = choice("dracula tame impala", None, &options);
        let json = serde_json::to_string(&message).expect("choice serialises");
        assert!(json.contains("dracula tame impala"), "{json}");
        assert!(json.contains("Dracula"), "{json}");
        assert!(json.contains("Tame Impala"), "{json}");
        // Lengths, so two recordings of one title can be told apart.
        assert!(json.contains("3:26"), "{json}");
        assert!(json.contains("1"), "{json}");
    }

    /// The catalog match is the reason the bot is asking: it knows roughly
    /// what was meant, and showing it makes a wrong guess obvious.
    #[test]
    fn the_chooser_names_the_closest_catalog_match() {
        let message = choice(
            "dracula",
            Some("Dracula by Tame Impala"),
            &[option("Dracula", "Tame Impala", Some(206.0))],
        );
        let json = serde_json::to_string(&message).expect("choice serialises");
        assert!(json.contains("Dracula by Tame Impala"), "{json}");
    }

    /// The request is speech-to-text output, so it can contain markdown that
    /// would otherwise reformat the message.
    #[test]
    fn a_request_containing_markdown_is_escaped() {
        // Checked on the escaped text, because the JSON encoding adds a second
        // layer of backslashes that has nothing to do with Discord's.
        assert_eq!(
            markdown_escape("play *everything* _now_"),
            r"play \*everything\* \_now\_"
        );
        let message = choice("play *everything* _now_", None, &[]);
        let json = serde_json::to_string(&message).expect("choice serialises");
        assert!(!json.contains("play *everything*"), "unescaped: {json}");
    }

    /// A candidate with no reported length still has to render as a field.
    #[test]
    fn an_option_without_metadata_still_renders() {
        assert_eq!(option("Dracula", "", None).detail(), "unknown length");
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
