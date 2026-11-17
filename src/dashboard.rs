//! The now-playing dashboard posted to the text channel.
//!
//! An embed plus buttons. Discord echoes a button's `custom_id` back on every
//! press, which is how the press is matched to [`action_from_id`].

use crate::queue::{Origin, Queued, Shift, Waiting};
use serenity::all::ReactionType;
use serenity::builder::{
    CreateActionRow, CreateButton, CreateEmbed, CreateEmbedAuthor, CreateMessage, CreateSelectMenu,
    CreateSelectMenuKind, CreateSelectMenuOption, EditMessage,
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

/// Says a remembered song was the wrong one: forgets the phrase, skips the
/// song and asks "Which one?" again. Only shown on a song the memory chose.
pub const ACTION_WRONG: &str = "wrong";

/// What the dashboard says under a song the song memory chose, so nobody
/// wonders why the bot did not ask.
pub const REMEMBERED_NOTE: &str = "🧠 remembered from last time";

/// Shows the next page of a `/play` search.
pub const ACTION_NEXT: &str = "next";

/// Throws away a `/play` search.
pub const ACTION_NOTHING: &str = "nothing";

/// The queue view's song picker: a select menu rather than a button per song,
/// because Discord fits five rows of five buttons in a message and the queue
/// holds twenty songs plus the controls.
const PICK: &str = "pick";

/// Marks a custom id as one of the queue view's edit buttons, which carries
/// the id of the song it acts on.
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
    /// The word in the custom id.
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

/// Builds the custom id for an edit button acting on the waiting song `id`.
///
/// The song is named by its queue id rather than its position, so a press
/// still acts on the song that was picked after the queue has moved on.
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

/// How many segments the progress bar is drawn with, the knob included.
const BAR_WIDTH: usize = 12;

/// "▬▬▬🔘▬▬▬▬ 1:12 / 3:26": how far into the song it is.
///
/// The position is clamped to the song, because songbird's clock can run a
/// fraction past a length YouTube rounded down.
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
///
/// `position` is how far into the song playback is, in seconds. With a known
/// length it is drawn as a progress bar in place of the bare length; without
/// either, the dashboard says only what it knows.
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

/// The dashboard showing what is waiting, with the controls to change it and
/// the way back to now-playing.
///
/// One line per song, numbered, under a summary of how many and how long: the
/// queue holds up to twenty songs, and a block per song made it taller than the
/// screen.
///
/// A song is changed by picking it from the menu and then pressing one of the
/// buttons under it, which act on `selected`. The buttons stay greyed out until
/// a song is picked, and a move that would go nowhere is greyed out too.
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
        // With nothing picked the id is left off: a greyed-out button still
        // needs an id, and one that names no song can never act.
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
/// A short message that something worked, such as an album being queued.
///
/// `markdown` is shown as written, so a name in it has to go through
/// [`markdown_escape`] first.
pub fn info(markdown: &str) -> View {
    View {
        embed: CreateEmbed::new()
            .description(trim(markdown, MAX_DESCRIPTION))
            .color(PLAYING),
        buttons: Vec::new(),
    }
}

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
    /// "Album", "EP" or "Single" for a whole release, which plays every
    /// track; `None` for a song.
    pub release: Option<&'static str>,
    /// The release's year, when known.
    pub year: Option<u16>,
}

impl ChoiceOption<'_> {
    /// The line shown under the title: what kind of release it is, if it is
    /// one, who uploaded it, and how long it runs.
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

/// Escapes the Discord markdown that would otherwise be read out of a title.
///
/// `requested` is speech-to-text output echoed back, so it can contain anything
/// the person said, and unescaped `*` or `_` would reformat the message.
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
