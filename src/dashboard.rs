//! The now-playing dashboard: an embed plus buttons matched by `custom_id`.

use crate::queue::{Origin, Queued, Shift, Waiting};
use serenity::all::ReactionType;
use serenity::builder::{
    CreateActionRow, CreateButton, CreateEmbed, CreateEmbedAuthor, CreateMessage, CreateSelectMenu,
    CreateSelectMenuKind, CreateSelectMenuOption, EditMessage,
};
use serenity::model::application::ButtonStyle;

/// Custom id prefix for every dashboard button (presses already carry the guild).
const PREFIX: &str = "shamash";

pub const ACTION_PAUSE: &str = "pause";
pub const ACTION_RESUME: &str = "resume";
pub const ACTION_SKIP: &str = "skip";
pub const ACTION_STOP: &str = "stop";

pub const ACTION_QUEUE: &str = "queue";

/// Separate from Queue: the bot doesn't track which view the message shows.
pub const ACTION_BACK: &str = "back";

pub const ACTION_REPEAT: &str = "repeat";

/// Forgets a remembered phrase, skips, and asks again. Only on remembered songs.
pub const ACTION_WRONG: &str = "wrong";

/// Shown under a remembered song, so nobody wonders why the bot didn't ask.
pub const REMEMBERED_NOTE: &str = "🧠 remembered from last time";

pub const ACTION_NEXT: &str = "next";

pub const ACTION_NOTHING: &str = "nothing";

/// A select menu: 20 songs plus controls won't fit Discord's 5x5 buttons.
const PICK: &str = "pick";

/// Custom id marker for queue edit buttons.
const EDIT_PREFIX: &str = "edit";

/// A change to the queue, made from the queue view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// Take the song out of the queue.
    Remove,
    /// Move the song to another place in the queue.
    Shift(Shift),
}

impl Edit {
    fn word(self) -> &'static str {
        match self {
            Edit::Remove => "remove",
            Edit::Shift(Shift::Up) => "up",
            Edit::Shift(Shift::Down) => "down",
            Edit::Shift(Shift::Front) => "front",
        }
    }
}

/// Discord's limit on a select option's label and description.
const MAX_OPTION: usize = 100;

/// Custom id marker for `/play` song buttons.
const ASK_PREFIX: &str = "ask";

/// Songs per `/play` page: two rows of buttons, leaving a row for next/dismiss.
pub const ASK_PAGE: usize = 9;

/// Discord embed limits; it rejects an over-long embed outright.
const MAX_TITLE: usize = 256;
const MAX_DESCRIPTION: usize = 4096;
const MAX_FIELD_NAME: usize = 256;
const MAX_FIELD_VALUE: usize = 1024;
/// Builds the custom id for a dashboard button.
fn button_id(action: &str) -> String {
    format!("{PREFIX}:{action}")
}

/// The action a dashboard button's custom id names, if any.
pub fn action_from_id(custom_id: &str) -> Option<&str> {
    let action = custom_id.strip_prefix(&format!("{PREFIX}:"))?;
    match action {
        ACTION_PAUSE | ACTION_RESUME | ACTION_SKIP | ACTION_STOP | ACTION_QUEUE | ACTION_BACK
        | ACTION_REPEAT | ACTION_WRONG | ACTION_NEXT | ACTION_NOTHING => Some(action),
        _ => None,
    }
}

/// Whether a custom id is the queue view's song picker.
pub fn is_pick(custom_id: &str) -> bool {
    custom_id == button_id(PICK)
}

/// The waiting song the picker chose, from the values Discord sent back.
pub fn picked(values: &[String]) -> Option<u64> {
    values.first()?.parse().ok()
}

/// Names the song by queue id, not position, so it survives the queue moving.
fn edit_id(edit: Edit, id: u64) -> String {
    format!("{PREFIX}:{EDIT_PREFIX}:{}:{id}", edit.word())
}

/// Recovers the change and the song from an edit button's custom id.
pub fn edit_from_id(custom_id: &str) -> Option<(Edit, u64)> {
    let rest = custom_id.strip_prefix(&format!("{PREFIX}:{EDIT_PREFIX}:"))?;
    let (word, id) = rest.split_once(':')?;
    let edit = [
        Edit::Remove,
        Edit::Shift(Shift::Up),
        Edit::Shift(Shift::Down),
        Edit::Shift(Shift::Front),
    ]
    .into_iter()
    .find(|edit| edit.word() == word)?;
    Some((edit, id.parse().ok()?))
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

/// The embed and buttons, usable for both posting and editing.
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

const PLAYING: u32 = 0x1d_b9_54;
const PAUSED: u32 = 0x80_84_8e;
const IDLE: u32 = 0x2b_2d_31;

/// How much of a title/artist a list line shows.
const LIST_TITLE: usize = 80;
const LIST_ARTIST: usize = 40;

/// A dashboard button; the emoji string keeps the variation selector Discord needs.
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

const BAR_WIDTH: usize = 12;

/// "▬▬▬🔘▬▬▬▬ 1:12 / 3:26". Clamped: songbird can overrun YouTube's rounded length.
fn progress_bar(position: f64, duration: f64) -> String {
    let duration = duration.max(0.0);
    let position = position.clamp(0.0, duration);
    let fraction = if duration > 0.0 {
        position / duration
    } else {
        0.0
    };
    let knob = ((fraction * (BAR_WIDTH - 1) as f64).round() as usize).min(BAR_WIDTH - 1);
    format!(
        "{}🔘{} {} / {}",
        "▬".repeat(knob),
        "▬".repeat(BAR_WIDTH - 1 - knob),
        clock(position),
        clock(duration)
    )
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

/// Builds the dashboard for the song playing now; `position` is in seconds.
pub fn dashboard(
    song: &Queued,
    position: Option<f64>,
    paused: bool,
    up_next: &[Queued],
    repeat: bool,
) -> View {
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
    match (position, song.duration_secs) {
        (Some(position), Some(duration)) => {
            lines.push(progress_bar(position, duration));
            lines.push(origin_label(song.origin).to_string());
        }
        _ => {
            let about: Vec<String> = song
                .duration_secs
                .map(clock)
                .into_iter()
                .chain([origin_label(song.origin).to_string()])
                .collect();
            lines.push(about.join(" · "));
        }
    }
    if song.remembered.is_some() {
        lines.push(REMEMBERED_NOTE.to_string());
    }
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
            // Own row, away from a thumb aiming at Repeat.
            CreateActionRow::Buttons(
                song.remembered
                    .is_some()
                    .then(|| button(ACTION_WRONG, "Wrong song", "🙅", ButtonStyle::Secondary))
                    .into_iter()
                    .chain([button(ACTION_STOP, "Stop", "⏹️", ButtonStyle::Danger)])
                    .collect(),
            ),
        ],
    }
}

/// The queue view: one line per song (blocks overflowed the screen), edits act on `selected`.
pub fn queue_view(waiting: &[Waiting], selected: Option<u64>) -> View {
    // A pick whose song has since started or been removed picks nothing.
    let selected = selected.and_then(|id| {
        waiting
            .iter()
            .position(|w| w.id == id)
            .map(|index| (index, id))
    });
    let pending: Vec<&Queued> = waiting.iter().map(|w| &w.song).collect();

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
            if selected.is_some_and(|(at, _)| at == index) {
                line.push_str(" ◀️");
            }
            line
        }));
        embed = embed.description(trim(&lines.join("\n"), MAX_DESCRIPTION));
    }

    let back = CreateActionRow::Buttons(vec![button(
        ACTION_BACK,
        "Back",
        "◀️",
        ButtonStyle::Secondary,
    )]);
    if waiting.is_empty() {
        // Discord rejects a menu with no options, and there is nothing to edit.
        return View {
            embed,
            buttons: vec![back],
        };
    }

    let options = waiting
        .iter()
        .enumerate()
        .map(|(index, w)| {
            let label = trim(&format!("{}. {}", index + 1, w.song.title), MAX_OPTION);
            let mut option = CreateSelectMenuOption::new(label, w.id.to_string())
                .default_selection(selected.is_some_and(|(_, id)| id == w.id));
            if !w.song.artist.is_empty() {
                option = option.description(trim(&w.song.artist, MAX_OPTION));
            }
            option
        })
        .collect();
    let picker = CreateSelectMenu::new(button_id(PICK), CreateSelectMenuKind::String { options })
        .placeholder("Pick a song to move or remove");

    let last = waiting.len() - 1;
    let edit = |edit: Edit, label: &str, emoji: &str, style: ButtonStyle, useful: bool| {
        // A disabled button still needs an id; one with no song can never act.
        let id = match selected {
            Some((_, id)) => edit_id(edit, id),
            None => format!("{PREFIX}:{EDIT_PREFIX}:{}", edit.word()),
        };
        CreateButton::new(id)
            .label(label)
            .emoji(ReactionType::Unicode(emoji.to_string()))
            .style(style)
            .disabled(selected.is_none() || !useful)
    };
    let at = selected.map(|(index, _)| index);
    let controls = CreateActionRow::Buttons(vec![
        edit(
            Edit::Shift(Shift::Front),
            "Play next",
            "⏫",
            ButtonStyle::Primary,
            at != Some(0),
        ),
        edit(
            Edit::Shift(Shift::Up),
            "Up",
            "🔼",
            ButtonStyle::Secondary,
            at != Some(0),
        ),
        edit(
            Edit::Shift(Shift::Down),
            "Down",
            "🔽",
            ButtonStyle::Secondary,
            at != Some(last),
        ),
        edit(Edit::Remove, "Remove", "🗑️", ButtonStyle::Danger, true),
    ]);

    View {
        embed,
        buttons: vec![CreateActionRow::SelectMenu(picker), controls, back],
    }
}

/// "**bot play**", "**bot play** or **shamash play**".
fn either(words: &[String]) -> String {
    let words: Vec<String> = words.iter().map(|w| format!("**{w} play**")).collect();
    match words.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

/// The dashboard shown when nothing is playing.
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

/// A short success message; escape names in `markdown` with [`markdown_escape`].
pub fn info(markdown: &str) -> View {
    View {
        embed: CreateEmbed::new()
            .description(trim(markdown, MAX_DESCRIPTION))
            .color(PLAYING),
        buttons: Vec::new(),
    }
}

/// A one-line notice for something that did not work.
pub fn notice(text: &str) -> View {
    View {
        embed: CreateEmbed::new()
            .description(trim(&markdown_escape(text), MAX_DESCRIPTION))
            .color(0xcd_2f_31),
        buttons: Vec::new(),
    }
}

/// The `/play` chooser; `page` is zero-based, `options` is this page of `total`.
pub fn ask(requested: &str, options: &[ChoiceOption<'_>], page: usize, total: usize) -> View {
    let has_next = (page + 1) * ASK_PAGE < total;
    let total_pages = total.div_ceil(ASK_PAGE).max(1);
    // One line per song: two-line blocks overflowed the screen.
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

    let first = page * ASK_PAGE;
    let numbered: Vec<CreateButton> = options
        .iter()
        .enumerate()
        // Label numbers the page; id numbers the whole search, so it survives paging.
        .map(|(offset, _)| {
            CreateButton::new(ask_id(first + offset)).label((offset + 1).to_string())
        })
        .collect();
    let mut rows: Vec<CreateActionRow> = numbered
        .chunks(5)
        .map(|chunk| CreateActionRow::Buttons(chunk.to_vec()))
        .collect();

    // Disabled, not removed: a vanishing button looks broken.
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
    /// Who recorded or uploaded it.
    pub channel: &'a str,
    pub duration_secs: Option<f64>,
    /// YouTube Music plays, or the upload's view count, when known.
    pub plays: Option<u64>,
    /// "Album", "EP" or "Single"; `None` for a song.
    pub release: Option<&'static str>,
    pub year: Option<u16>,
}

impl ChoiceOption<'_> {
    fn detail(&self) -> String {
        let mut detail = String::new();
        if let Some(release) = self.release {
            detail.push_str(release);
        }
        if !self.channel.is_empty() {
            if !detail.is_empty() {
                detail.push_str(" \u{2022} ");
            }
            detail.push_str(&trim(self.channel, MAX_FIELD_NAME));
        }
        if let Some(year) = self.year {
            if !detail.is_empty() {
                detail.push_str(" \u{2022} ");
            }
            detail.push_str(&year.to_string());
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

/// Escapes Discord markdown in untrusted text such as transcripts.
pub(crate) fn markdown_escape(text: &str) -> String {
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
#[path = "tests/dashboard.rs"]
mod tests;
