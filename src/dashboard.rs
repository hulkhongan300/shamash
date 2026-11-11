//! The now-playing dashboard posted to the text channel.
//!
//! An embed plus buttons. Discord echoes a button's `custom_id` back on every
//! press, which is how the press is matched to [`action_from_id`].

use crate::queue::{Origin, Queued};
use serenity::all::ReactionType;
use serenity::builder::{
    CreateActionRow, CreateButton, CreateEmbed, CreateEmbedAuthor, CreateMessage, EditMessage,
};
use serenity::model::application::ButtonStyle;

/// Custom id prefix for every dashboard button.
///
/// The guild is not encoded: a press already arrives with the guild attached.
const PREFIX: &str = "shamash";

/// Buttons, in the order they are shown.
pub const ACTION_PAUSE: &str = "pause";
pub const ACTION_RESUME: &str = "resume";
pub const ACTION_SKIP: &str = "skip";
pub const ACTION_STOP: &str = "stop";

/// Shows what is waiting in the queue, in place of the now-playing view.
pub const ACTION_QUEUE: &str = "queue";

/// Goes back to the now-playing view from the queue.
///
/// The same button cannot do both, because the dashboard is one message that is
/// edited in place and the bot does not track which way round it was left.
pub const ACTION_BACK: &str = "back";

/// Turns repeat-one on or off.
pub const ACTION_REPEAT: &str = "repeat";

/// Shows the next page of a `/play` search.
pub const ACTION_NEXT: &str = "next";

/// Throws away a `/play` search.
pub const ACTION_NOTHING: &str = "nothing";

/// Marks a custom id as a `/play` song button, which names a position in the
/// guild's outstanding search rather than an action.
const ASK_PREFIX: &str = "ask";

/// How many songs one page of a `/play` search offers.
///
/// Discord takes five buttons to a row and nine is two rows, which leaves room
/// for the next-page and dismiss buttons on a row of their own.
pub const ASK_PAGE: usize = 9;

/// The longest a title may be before it is trimmed.
///
/// Discord rejects an over-long embed rather than truncating it, which would
/// lose the whole dashboard.
const MAX_TITLE: usize = 256;
/// The same limit applies to the description and field values.
const MAX_DESCRIPTION: usize = 4096;
/// Longest field name Discord accepts.
const MAX_FIELD_NAME: usize = 256;
/// The same limit applies to a field's value.
const MAX_FIELD_VALUE: usize = 1024;
/// Builds the custom id for a dashboard button.
fn button_id(action: &str) -> String {
    format!("{PREFIX}:{action}")
}

/// Recovers the intent from a pressed button's custom id, or `None` for
/// anything that is not a dashboard button.
pub fn action_from_id(custom_id: &str) -> Option<&str> {
    let action = custom_id.strip_prefix(&format!("{PREFIX}:"))?;
    match action {
        ACTION_PAUSE | ACTION_RESUME | ACTION_SKIP | ACTION_STOP | ACTION_QUEUE | ACTION_BACK
        | ACTION_REPEAT | ACTION_NEXT | ACTION_NOTHING => Some(action),
        _ => None,
    }
}

/// Builds the custom id for a `/play` song button.
pub fn ask_id(index: usize) -> String {
    format!("{PREFIX}:{ASK_PREFIX}:{index}")
}

/// Recovers the index from a `/play` song button's custom id.
pub fn asked_index(custom_id: &str) -> Option<usize> {
    let action = custom_id.strip_prefix(&format!("{PREFIX}:{ASK_PREFIX}:"))?;
    action.parse().ok()
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

/// The colour of a dashboard that is playing.
const PLAYING: u32 = 0x1d_b9_54;
/// The colour of a paused dashboard, so the state shows at a glance.
const PAUSED: u32 = 0x80_84_8e;
/// The colour of the idle dashboard.
const IDLE: u32 = 0x2b_2d_31;

/// How much of a title a list line shows. A list is for finding a song, and a
/// whole title can be read on the song itself.
const LIST_TITLE: usize = 80;
/// How much of an artist a list line shows.
const LIST_ARTIST: usize = 40;

/// A dashboard button with an icon.
///
/// The emoji goes in as a string, which carries the variation selector Discord
/// needs to draw it as an emoji rather than as a plain character.
fn button(action: &str, label: &str, emoji: &str, style: ButtonStyle) -> CreateButton {
    CreateButton::new(button_id(action))
        .label(label)
        .emoji(ReactionType::Unicode(emoji.to_string()))
        .style(style)
}

/// "3:26", or "1:02:03" past an hour.
fn clock(secs: f64) -> String {
    let total = secs.max(0.0).round() as u64;
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// "23 min", or "1 h 05 min" past an hour.
fn total_time(secs: f64) -> String {
    let minutes = (secs.max(0.0) / 60.0).round() as u64;
    if minutes >= 60 {
        format!("{} h {:02} min", minutes / 60, minutes % 60)
    } else {
        format!("{} min", minutes.max(1))
    }
}

/// How a song was asked for, as the dashboard says it.
fn origin_label(origin: Origin) -> &'static str {
    match origin {
        Origin::Voice => "🎤 asked by voice",
        Origin::Slash => "⌨️ from /play",
        Origin::Playlist => "💿 from a playlist",
    }
}

/// "**Title** — Artist", escaped and shortened for a list.
fn song_line(title: &str, artist: &str) -> String {
    let title = markdown_escape(&trim(title, LIST_TITLE));
    if artist.is_empty() {
        format!("**{title}**")
    } else {
        format!(
            "**{title}** — {}",
            markdown_escape(&trim(artist, LIST_ARTIST))
        )
    }
}

/// Builds the dashboard for the song playing now.
///
/// `paused` picks between a pause and a resume button, because a paused song
/// needs the opposite action, and greys the dashboard so the state shows at a
/// glance. `repeat` is stated on the button and in the embed, because a toggle
/// whose state is only visible by its effect is one nobody trusts. `up_next` is
/// the queue behind this song, of which the first is named.
pub fn dashboard(song: &Queued, paused: bool, up_next: &[Queued], repeat: bool) -> View {
    let mut embed = CreateEmbed::new()
        .title(trim(&song.title, MAX_TITLE))
        .url(&song.url)
        .thumbnail(song.thumbnail())
        .color(if paused { PAUSED } else { PLAYING });

    if !song.artist.is_empty() {
        embed = embed.author(CreateEmbedAuthor::new(trim(&song.artist, MAX_FIELD_NAME)));
    }

    let mut lines = Vec::new();
    if paused {
        lines.push("⏸️ **Paused**".to_string());
    }
    let about: Vec<String> = song
        .duration_secs
        .map(clock)
        .into_iter()
        .chain([origin_label(song.origin).to_string()])
        .collect();
    lines.push(about.join(" · "));
    if repeat {
        lines.push("🔂 Repeating this song".to_string());
    }
    embed = embed.description(trim(&lines.join("\n"), MAX_DESCRIPTION));

    let next = match up_next {
        [] => "Nothing queued".to_string(),
        [next] => song_line(&next.title, &next.artist),
        [next, rest @ ..] => format!(
            "{}\n+{} more",
            song_line(&next.title, &next.artist),
            rest.len()
        ),
    };
    embed = embed.field("Up next", trim(&next, MAX_FIELD_VALUE), false);

    let toggle = if paused {
        button(ACTION_RESUME, "Resume", "▶️", ButtonStyle::Primary)
    } else {
        button(ACTION_PAUSE, "Pause", "⏸️", ButtonStyle::Primary)
    };
    let queue_label = if up_next.is_empty() {
        "Queue".to_string()
    } else {
        format!("Queue ({})", up_next.len())
    };

    View {
        embed,
        buttons: vec![
            CreateActionRow::Buttons(vec![
                toggle,
                button(ACTION_SKIP, "Skip", "⏭️", ButtonStyle::Secondary),
                button(ACTION_QUEUE, &queue_label, "📜", ButtonStyle::Secondary),
                // Green when on: the colour of a switch that is set.
                button(
                    ACTION_REPEAT,
                    if repeat { "Repeat: on" } else { "Repeat" },
                    "🔂",
                    if repeat {
                        ButtonStyle::Success
                    } else {
                        ButtonStyle::Secondary
                    },
                ),
            ]),
            // A row of its own, so it is not pressed by a thumb aiming at Repeat.
            CreateActionRow::Buttons(vec![button(ACTION_STOP, "Stop", "⏹️", ButtonStyle::Danger)]),
        ],
    }
}

/// The dashboard showing what is waiting, with the way back to now-playing.
///
/// One line per song, numbered, under a summary of how many and how long: the
/// queue holds up to twenty songs, and a block per song made it taller than the
/// screen.
pub fn queue_view(pending: &[Queued]) -> View {
    let mut embed = CreateEmbed::new().title("📜 Up next").color(PLAYING);

    if pending.is_empty() {
        embed = embed.description("Nothing is queued.");
    } else {
        let known: f64 = pending.iter().filter_map(|song| song.duration_secs).sum();
        let all_known = pending.iter().all(|song| song.duration_secs.is_some());
        let count = format!(
            "{} song{}",
            pending.len(),
            if pending.len() == 1 { "" } else { "s" }
        );
        let summary = match (known > 0.0, all_known) {
            (false, _) => count,
            (true, true) => format!("{count} · {}", total_time(known)),
            (true, false) => format!("{count} · over {}", total_time(known)),
        };
        let mut lines = vec![format!("**{summary}**"), String::new()];
        lines.extend(pending.iter().enumerate().map(|(index, song)| {
            let mut line = format!(
                "`{:>2}.` {}",
                index + 1,
                song_line(&song.title, &song.artist)
            );
            if let Some(duration) = song.duration_secs {
                line.push_str(&format!(" · {}", clock(duration)));
            }
            line
        }));
        embed = embed.description(trim(&lines.join("\n"), MAX_DESCRIPTION));
    }

    View {
        embed,
        buttons: vec![CreateActionRow::Buttons(vec![button(
            ACTION_BACK,
            "Back",
            "◀️",
            ButtonStyle::Secondary,
        )])],
    }
}

/// "**bot**", "**bot** or **shamash**", "**a**, **b** or **c**".
fn either(words: &[String]) -> String {
    let words: Vec<String> = words.iter().map(|w| format!("**{w}**")).collect();
    match words.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

/// The dashboard shown when there is nothing playing, saying both ways to
/// start a song.
pub fn idle_dashboard<'a>(wake_words: impl IntoIterator<Item = &'a String>) -> View {
    let mut words: Vec<String> = wake_words.into_iter().cloned().collect();
    words.sort();
    let description = if words.is_empty() {
        "Use `/play` to start the queue.".to_string()
    } else {
        format!("Say {} and a song, or use `/play`.", either(&words))
    };
    View {
        embed: CreateEmbed::new()
            .title("Nothing playing")
            .description(trim(&description, MAX_DESCRIPTION))
            .color(IDLE),
        buttons: Vec::new(),
    }
}

/// The custom id for a dashboard action button.
pub fn action_id(action: &str) -> String {
    format!("{PREFIX}:{action}")
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

/// The chooser a `/play` search posts.
///
/// A catalog search rather than a video search: the buttons name songs, and the
/// upload for whichever one is picked is found afterwards. That is the order
/// that makes a second page worth having -- the catalog has far more songs
/// under a query than there are uploads worth offering.
///
/// `page` is zero-based, `total` is how many songs the search found in all, and
/// `options` is this page's window onto them. The buttons carry positions within
/// the whole search, not within the page, so a press still names the same song
/// however many pages have gone by.
pub fn ask(requested: &str, options: &[ChoiceOption<'_>], page: usize, total: usize) -> View {
    let has_next = (page + 1) * ASK_PAGE < total;
    let total_pages = total.div_ceil(ASK_PAGE).max(1);
    // One line per song: nine blocks of two lines each was taller than the
    // screen, and the numbers are what the buttons below say.
    let mut lines = vec![
        format!(
            "Results for “{}”",
            trim(&markdown_escape(requested), LIST_TITLE * 2)
        ),
        String::new(),
    ];
    lines.extend(options.iter().enumerate().map(|(index, option)| {
        format!(
            "`{}.` **{}** · {}",
            index + 1,
            markdown_escape(&trim(option.title, LIST_TITLE)),
            markdown_escape(&option.detail())
        )
    }));
    lines.push(String::new());
    lines.push(format!("Page {} of {total_pages}", page + 1));
    let embed = CreateEmbed::new()
        .title("Which one?")
        .description(trim(&lines.join("\n"), MAX_DESCRIPTION))
        .color(PLAYING);

    // Five to a row, because that is Discord's limit for one action row. The id
    // carries the position in the whole search, not in this page, so a press
    // still names the same song however many pages have gone by.
    let first = page * ASK_PAGE;
    let numbered: Vec<CreateButton> = options
        .iter()
        .enumerate()
        // The label numbers the page, to match the fields in the embed above;
        // the id numbers the whole search, so a press still names the same song
        // after the page moves. Both are needed: Discord rejects a button that
        // has an id and no label.
        .map(|(offset, _)| {
            CreateButton::new(ask_id(first + offset)).label((offset + 1).to_string())
        })
        .collect();
    let mut rows: Vec<CreateActionRow> = numbered
        .chunks(5)
        .map(|chunk| CreateActionRow::Buttons(chunk.to_vec()))
        .collect();

    // Disabled rather than absent when the list has run out: a button that
    // vanishes leaves the message looking broken, and a press on it would fail.
    rows.push(CreateActionRow::Buttons(vec![
        button(ACTION_NEXT, "Next page", "➡️", ButtonStyle::Secondary).disabled(!has_next),
        button(
            ACTION_NOTHING,
            "None of these",
            "✖️",
            ButtonStyle::Secondary,
        ),
    ]));

    View {
        embed,
        buttons: rows,
    }
}

/// One option in the chooser.
#[derive(Clone, Copy)]
pub struct ChoiceOption<'a> {
    pub title: &'a str,
    /// Who recorded or uploaded it, shown as the second line.
    pub channel: &'a str,
    pub duration_secs: Option<f64>,
    /// YouTube Music plays, or the upload's view count, when known.
    pub plays: Option<u64>,
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
        if let Some(plays) = self.plays {
            if !detail.is_empty() {
                detail.push_str(" \u{2022} ");
            }
            detail.push_str(&plays_label(plays));
        }
        if detail.is_empty() {
            detail.push_str("unknown length");
        }
        detail
    }
}

/// "217M plays" from a raw count, matching how YouTube writes it.
fn plays_label(plays: u64) -> String {
    if plays == 1 {
        return "1 play".to_string();
    }
    let (value, suffix) = if plays >= 1_000_000_000 {
        (plays as f64 / 1_000_000_000.0, "B")
    } else if plays >= 1_000_000 {
        (plays as f64 / 1_000_000.0, "M")
    } else if plays >= 1_000 {
        (plays as f64 / 1_000.0, "K")
    } else {
        return format!("{plays} plays");
    };
    // One decimal below ten ("2.2M"), none above ("217M").
    let number = if value < 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.0}")
    };
    format!("{}{suffix} plays", number.trim_end_matches(".0"))
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

    fn option(title: &str, channel: &str, duration: Option<f64>) -> ChoiceOption<'static> {
        ChoiceOption {
            title: Box::leak(title.to_string().into_boxed_str()),
            channel: Box::leak(channel.to_string().into_boxed_str()),
            duration_secs: duration,
            plays: None,
        }
    }

    /// A candidate with no reported length still has to render as a field.
    #[test]
    fn an_option_without_metadata_still_renders() {
        assert_eq!(option("Dracula", "", None).detail(), "unknown length");
    }

    /// The chooser answers "which one" with the one fact that separates two
    /// recordings of the same title: how often each has been played.
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
        ] {
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
        let view = dashboard(&song(), false, &[], false);
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
        let playing = rendered(&dashboard(&song(), false, &[], false));
        assert!(playing.contains("Pause"), "{playing}");
        assert!(!playing.contains("Resume"), "{playing}");

        let paused = rendered(&dashboard(&song(), true, &[], false));
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

    /// The next song is named rather than counted, and the rest are summed up.
    #[test]
    fn the_next_song_is_named() {
        let empty = rendered(&dashboard(&song(), false, &[], false));
        assert!(empty.contains("Nothing queued"), "{empty}");

        let next = [named("Rhymes Like Dimes", "MF DOOM", None)];
        let one = rendered(&dashboard(&song(), false, &next, false));
        assert!(one.contains("**Rhymes Like Dimes** — MF DOOM"), "{one}");
        assert!(!one.contains("more"), "{one}");
        assert!(one.contains("Queue (1)"), "{one}");

        let three = [next[0].clone(), song(), song()];
        let many = rendered(&dashboard(&song(), false, &three, false));
        assert!(many.contains("Rhymes Like Dimes"), "{many}");
        assert!(many.contains("+2 more"), "{many}");
        assert!(many.contains("Queue (3)"), "{many}");
    }

    /// The length and how the song was asked for are shown under the title.
    #[test]
    fn the_dashboard_says_how_long_and_how_it_was_asked_for() {
        let voice = description_of(dashboard(&song(), false, &[], false));
        assert_eq!(voice, "3:26 · 🎤 asked by voice");

        let mut typed = song();
        typed.origin = Origin::Playlist;
        typed.duration_secs = None;
        assert_eq!(
            description_of(dashboard(&typed, false, &[], false)),
            "💿 from a playlist"
        );
    }

    /// Paused greys the dashboard and says so; repeat says so too.
    #[test]
    fn paused_and_repeat_are_visible_at_a_glance() {
        let colour = |view: &View| serde_json::to_value(&view.embed).unwrap()["color"].clone();
        let playing = dashboard(&song(), false, &[], false);
        let paused = dashboard(&song(), true, &[], true);
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

    /// The idle dashboard names both ways in: the wake words and `/play`.
    #[test]
    fn the_idle_dashboard_names_the_wake_words_and_play() {
        let words = ["shamash".to_string(), "bot".to_string()];
        let description = description_of(idle_dashboard(&words));
        assert_eq!(
            description,
            "Say **bot** or **shamash** and a song, or use `/play`."
        );
        let none: [String; 0] = [];
        assert!(description_of(idle_dashboard(&none)).contains("/play"));
    }

    #[test]
    fn an_over_long_title_is_trimmed_rather_than_rejected() {
        // Discord rejects an embed whose title is over the limit, which would
        // lose the whole dashboard.
        let mut long = song();
        long.title = "x".repeat(400);
        let json =
            serde_json::to_string(&dashboard(&long, false, &[], false).embed).expect("serialises");
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

    /// The now-playing view has to offer the way into the queue, or the button
    /// is unreachable.
    #[test]
    fn the_dashboard_offers_the_queue_and_the_queue_offers_the_way_back() {
        let now_playing = rendered(&dashboard(&song(), false, &[song()], false));
        assert!(now_playing.contains("Queue"), "{now_playing}");
        assert!(!now_playing.contains("Back"), "{now_playing}");

        let queue = rendered(&queue_view(&[song()]));
        assert!(queue.contains("Back"), "{queue}");
        // Back is the only way off, so a queue view without it is a dead end.
        assert!(!queue.contains("Stop"), "{queue}");
    }

    /// The queue is the reason the view exists, so the songs in it have to be
    /// named, soonest first.
    #[test]
    fn the_queue_view_lists_every_waiting_song_in_order() {
        let view = queue_view(&[
            named("Rhymes Like Dimes", "MF DOOM", Some(259.0)),
            named("All Caps", "Madvillain", Some(130.0)),
        ]);
        let description = description_of(view);
        assert!(
            description.starts_with("**2 songs · 6 min**"),
            "{description}"
        );
        assert!(
            description.contains("` 1.` **Rhymes Like Dimes** — MF DOOM · 4:19\n` 2.` **All Caps** — Madvillain · 2:10"),
            "{description}"
        );
    }

    /// A song of unknown length makes the total a lower bound.
    #[test]
    fn a_queue_with_an_unknown_length_says_over() {
        let view = queue_view(&[named("A", "", Some(600.0)), named("B", "", None)]);
        assert!(description_of(view).starts_with("**2 songs · over 10 min**"));
    }

    #[test]
    fn the_queue_view_keeps_the_order_and_artist() {
        let mut second = song();
        second.title = "Second Song".to_string();
        let view = queue_view(&[song(), second]);
        let json = serde_json::to_string(&view.embed).expect("embed serialises");
        let first_at = json.find("A Song").expect("the first song is listed");
        let second_at = json.find("Second Song").expect("the second song is listed");
        assert!(first_at < second_at, "out of order: {json}");
        assert!(json.contains("An Artist"), "the artist is shown: {json}");
    }

    /// An empty queue still has to render, and must say so rather than showing
    /// an embed with nothing in it.
    #[test]
    fn an_empty_queue_says_so() {
        let view = queue_view(&[]);
        assert!(
            rendered(&view).contains("Nothing is queued"),
            "{}",
            rendered(&view)
        );
    }

    /// A toggle whose state is only visible in its effect is one nobody trusts,
    /// so the button has to say which way it is set.
    #[test]
    fn the_repeat_button_says_whether_repeat_is_on() {
        let off = rendered(&dashboard(&song(), false, &[], false));
        assert!(off.contains("Repeat"), "{off}");
        assert!(!off.contains("Repeat: on"), "off by default: {off}");

        let on = rendered(&dashboard(&song(), false, &[], true));
        assert!(on.contains("Repeat: on"), "{on}");
    }

    /// Every control is still there, with Stop on a row of its own so it is
    /// not pressed by accident, and every button has an icon.
    #[test]
    fn stop_has_a_row_of_its_own_and_nothing_was_lost() {
        let value = serde_json::to_value(&dashboard(&song(), false, &[], false).buttons).unwrap();
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

    /// Repeat is grey when off and green when on, the colours of a switch.
    #[test]
    fn the_repeat_button_is_green_when_on() {
        let style = |repeat| {
            serde_json::to_value(&dashboard(&song(), false, &[], repeat).buttons).unwrap()[0]
                ["components"][3]["style"]
                .clone()
        };
        assert_eq!(style(false), 2, "secondary when off");
        assert_eq!(style(true), 3, "success when on");
    }

    /// A queued title can be arbitrarily long, and Discord rejects an embed
    /// whose field name is over the limit rather than truncating it.
    #[test]
    fn an_over_long_queued_title_is_trimmed_rather_than_rejected() {
        let mut long = song();
        long.title = "x".repeat(400);
        let mut queue = vec![long; crate::queue::MAX_QUEUE];
        queue[0].artist = "y".repeat(400);
        let description = description_of(queue_view(&queue));
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

    /// A `/play` button has to survive the round trip, and must not be
    /// readable as a playback action.
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

    /// The position in the button id is the position in the whole search, not
    /// in the page, so a press still names the same song after a redraw.
    #[test]
    fn the_buttons_name_songs_across_the_whole_search() {
        let json = serde_json::to_string(&ask("dracula", &offers(9), 1, 20).into_message())
            .expect("serialises");
        assert!(json.contains("ask:9"), "page two starts at nine: {json}");
        assert!(json.contains("ask:17"), "and runs to seventeen: {json}");
        assert!(!json.contains("ask:0"), "the ids are absolute: {json}");
    }

    /// Nine songs, so two rows of five and one of four, plus a row for the page
    /// controls. Discord takes five to a row and twenty-five in total, and
    /// anything over is dropped by the API.
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

    /// The last page says so on the button rather than leaving it to fail: a
    /// button that vanishes makes the message look broken.
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
        // 20 songs, 9 to a page: page three is the last, and holds two.
        assert!(
            next_disabled(ask("dracula", &offers(2), 2, 20)),
            "the last page"
        );
    }

    /// Discord rejects a button that carries an id and no label, so a chooser
    /// of unlabelled buttons does not post at all. Every button must have both.
    #[test]
    fn every_button_in_the_chooser_is_labelled() {
        for (page, total) in [(0, 20), (1, 20), (2, 20), (0, 4)] {
            let json =
                serde_json::to_string(&ask("dracula", &offers(9), page, total).into_message())
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

    /// The label numbers the page, because that is what the embed's fields say;
    /// the id numbers the whole search. Getting those the wrong way round would
    /// make button 1 mean the tenth song.
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

    /// One line per song, numbered like the buttons, with what was asked for
    /// echoed above them.
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

    /// The page count has to be the whole list's, not this page's.
    #[test]
    fn the_chooser_says_which_page_of_how_many() {
        let json = description_of(ask("dracula", &offers(9), 1, 20));
        assert!(json.contains("Page 2 of 3"), "{json}");
        assert!(json.contains("dracula"), "the query is echoed: {json}");
    }
}
