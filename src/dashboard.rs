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

/// The button on a chooser that means "none of these".
///
/// A question with no way to say no is a question that has to be answered. The
/// listener who asked for something the bot did not offer gets a way to say so
/// rather than leaving a question on offer forever.
pub const ACTION_NONE: &str = "none";

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
        ACTION_PAUSE | ACTION_RESUME | ACTION_SKIP | ACTION_STOP | ACTION_NONE | ACTION_QUEUE
        | ACTION_BACK | ACTION_REPEAT | ACTION_NEXT | ACTION_NOTHING => Some(action),
        _ => None,
    }
}

/// Builds the custom id for a "which one did you mean" button.
///
/// The key rides in the custom id rather than being looked up, so a press needs
/// no state. It is opaque: a video's id, or the index of a song in the guild's
/// current question. Either way what it means is decided by the choice store and
/// by nothing that arrived from the client.
pub fn pick_id(key: &str) -> String {
    format!("{PREFIX}:{PICK_PREFIX}:{key}")
}

/// Recovers the key from a choice button's custom id. Separate from
/// [`action_from_id`], which only reports the playback actions.
pub fn picked_key(custom_id: &str) -> Option<&str> {
    let action = custom_id.strip_prefix(&format!("{PREFIX}:"))?;
    let key = action.strip_prefix(&format!("{PICK_PREFIX}:"))?;
    // An empty key would match nothing and then do nothing.
    (!key.is_empty()).then_some(key)
}

/// Builds the custom id for a `/play` song button.
pub fn ask_id(index: usize) -> String {
    format!("{PREFIX}:{ASK_PREFIX}:{index}")
}

/// Recovers the index from a `/play` song button's custom id.
///
/// Separate from [`picked_key`] because a `/play` chooser is not the voice
/// chooser: the two have different owners and different stores, and letting
/// one id reach the other would let a press act on the wrong list.
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

/// Builds the dashboard for the song playing now.
///
/// `paused` picks between a pause and a resume button, because a paused song
/// needs the opposite action. `repeat` is stated on the button, because a
/// toggle whose state is only visible by its effect is one nobody trusts.
pub fn dashboard(song: &Queued, paused: bool, pending: usize, repeat: bool) -> View {
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
            CreateButton::new(button_id(ACTION_QUEUE)).label("Queue"),
            CreateButton::new(button_id(ACTION_REPEAT))
                .label(if repeat { "Repeat: on" } else { "Repeat" })
                .style(if repeat {
                    ButtonStyle::Secondary
                } else {
                    ButtonStyle::Primary
                }),
            CreateButton::new(button_id(ACTION_STOP))
                .label("Stop")
                .style(ButtonStyle::Danger),
        ])],
    }
}

/// The dashboard showing what is waiting, with the way back to now-playing.
///
/// One field per song rather than one line in the description, so a long queue
/// stays readable and Discord's own limit does the truncating for us.
pub fn queue_view(pending: &[Queued]) -> View {
    let mut embed = CreateEmbed::new().title("Up next").color(0x1d_b9_54);

    if pending.is_empty() {
        embed = embed.description("Nothing is queued.");
    } else {
        embed = embed.fields(
            pending
                .iter()
                .enumerate()
                .map(|(index, song)| {
                    let name = trim(&format!("{}. {}", index + 1, song.title), MAX_FIELD_NAME);
                    let artist = trim(&song.artist, MAX_FIELD_VALUE);
                    (
                        name,
                        if artist.is_empty() {
                            "\u{2014}".to_string()
                        } else {
                            artist
                        },
                        false,
                    )
                })
                .collect::<Vec<_>>(),
        );
    }

    View {
        embed,
        buttons: vec![CreateActionRow::Buttons(vec![
            CreateButton::new(button_id(ACTION_BACK)).label("Back"),
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

    let mut buttons: Vec<CreateButton> = options
        .iter()
        .enumerate()
        .map(|(index, option)| {
            CreateButton::new(pick_id(option.key)).label((index + 1).to_string())
        })
        .collect();
    buttons.push(
        CreateButton::new(action_id(ACTION_NONE))
            .label("None of these")
            .style(ButtonStyle::Secondary),
    );

    CreateMessage::new()
        .embed(embed)
        .components(vec![CreateActionRow::Buttons(buttons)])
}

/// The custom id for a dashboard action button.
pub fn action_id(action: &str) -> String {
    format!("{PREFIX}:{action}")
}

/// The edit that removes the buttons from a question nobody answered.
///
/// Used after the chooser expires: the question stays readable, but a press on
/// a button from ten minutes ago must not start a download.
pub fn expired(question: &str) -> EditMessage {
    EditMessage::new()
        .embed(
            CreateEmbed::new()
                .title("Which one did you mean?")
                .description(trim(question, MAX_DESCRIPTION))
                .footer(CreateEmbedFooter::new("This question timed out."))
                .color(0x60_6a_76),
        )
        .components(Vec::new())
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
    let embed = CreateEmbed::new()
        .title("Which one?")
        .description(format!(
            "Searching the catalog for {}.\nPage {} of {}.",
            trim(&markdown_escape(requested), MAX_DESCRIPTION),
            page + 1,
            total_pages
        ))
        .color(0x1d_b9_54)
        .fields(
            options
                .iter()
                .enumerate()
                .map(|(index, option)| {
                    (
                        format!("{}. {}", index + 1, trim(option.title, MAX_FIELD_NAME)),
                        trim(&option.detail(), MAX_FIELD_VALUE),
                        false,
                    )
                })
                .collect::<Vec<_>>(),
        );

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
        CreateButton::new(action_id(ACTION_NEXT))
            .label("Next page")
            .style(ButtonStyle::Secondary)
            .disabled(!has_next),
        CreateButton::new(action_id(ACTION_NOTHING))
            .label("None of these")
            .style(ButtonStyle::Secondary),
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
    /// The opaque key this button stands for, in the guild's choice store.
    ///
    /// Opaque on purpose: a press carries a key, and the store decides what it
    /// means. Nothing a client sends is treated as a video id to play.
    pub key: &'a str,
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
        assert_eq!(picked_key(&id), Some("dQw4w9WgXcQ"));
    }

    /// A choice button is not a playback action, and `action_from_id` is what
    /// the button dispatcher trusts, so it must not claim these.
    #[test]
    fn a_choice_button_is_not_a_playback_action() {
        let id = pick_id("dQw4w9WgXcQ");
        assert_eq!(action_from_id(&id), None);
        assert_eq!(picked_key("shamash:pause"), None);
        assert_eq!(picked_key("other:pick:abc"), None);
        assert_eq!(picked_key("shamash:pick:"), None);
        // A catalog question's key is an index, and the store is what says what
        // index 0 means.
        assert_eq!(picked_key(&pick_id("song:0")), Some("song:0"));
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
            key: "vid",
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
        for action in [
            ACTION_PAUSE,
            ACTION_RESUME,
            ACTION_SKIP,
            ACTION_STOP,
            ACTION_QUEUE,
            ACTION_BACK,
            ACTION_REPEAT,
            ACTION_NONE,
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
        let view = dashboard(&song(), false, 0, false);
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
        let playing = rendered(&dashboard(&song(), false, 0, false));
        assert!(playing.contains("Pause"), "{playing}");
        assert!(!playing.contains("Resume"), "{playing}");

        let paused = rendered(&dashboard(&song(), true, 0, false));
        assert!(paused.contains("Resume"), "{paused}");
        assert!(
            paused.contains("Paused"),
            "the state must be visible: {paused}"
        );
    }

    #[test]
    fn the_queue_length_is_summarised() {
        let empty = rendered(&dashboard(&song(), false, 0, false));
        assert!(empty.contains("nothing queued"), "{empty}");

        let one = rendered(&dashboard(&song(), false, 1, false));
        assert!(one.contains("1 song"), "{one}");

        let many = rendered(&dashboard(&song(), false, 3, false));
        assert!(many.contains("3 songs"), "{many}");
    }

    #[test]
    fn an_over_long_title_is_trimmed_rather_than_rejected() {
        // Discord rejects an embed whose title is over the limit, which would
        // lose the whole dashboard.
        let mut long = song();
        long.title = "x".repeat(400);
        let json =
            serde_json::to_string(&dashboard(&long, false, 0, false).embed).expect("serialises");
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
        let now_playing = rendered(&dashboard(&song(), false, 1, false));
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
        let off = rendered(&dashboard(&song(), false, 0, false));
        assert!(off.contains("Repeat"), "{off}");
        assert!(!off.contains("Repeat: on"), "off by default: {off}");

        let on = rendered(&dashboard(&song(), false, 0, true));
        assert!(on.contains("Repeat: on"), "{on}");
    }

    /// Discord takes at most five buttons in a row, and this row now has five.
    /// A sixth would be dropped by the API, taking Stop with it.
    #[test]
    fn the_button_row_stays_within_discords_limit() {
        let json = serde_json::to_string(&dashboard(&song(), false, 0, false).buttons)
            .expect("serialises");
        let buttons = json.matches("\"type\":2").count();
        assert!(buttons <= 5, "{buttons} buttons in one row: {json}");
        assert_eq!(buttons, 5, "the row lost a button: {json}");
    }

    /// A queued title can be arbitrarily long, and Discord rejects an embed
    /// whose field name is over the limit rather than truncating it.
    #[test]
    fn an_over_long_queued_title_is_trimmed_rather_than_rejected() {
        let mut long = song();
        long.title = "x".repeat(400);
        let json = serde_json::to_string(&queue_view(&[long]).embed).expect("serialises");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        let name = value["fields"][0]["name"].as_str().expect("a field name");
        assert!(
            name.chars().count() <= MAX_FIELD_NAME,
            "field name was {} chars",
            name.chars().count()
        );
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
    /// readable as a voice chooser key: the two reach different stores.
    #[test]
    fn a_play_button_round_trips_and_is_not_a_voice_choice() {
        assert_eq!(asked_index(&ask_id(4)), Some(4));
        assert_eq!(
            asked_index(&ask_id(0)),
            Some(0),
            "page two starts at zero again"
        );
        assert_eq!(
            picked_key(&ask_id(4)),
            None,
            "must not reach the voice store"
        );
        assert_eq!(
            action_from_id(&ask_id(4)),
            None,
            "must not be a playback action"
        );
        assert_eq!(asked_index(&pick_id("abc")), None);
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
        let more = serde_json::to_string(&ask("dracula", &offers(9), 0, 20).into_message())
            .expect("serialises");
        assert!(
            more.contains(
                "\"custom_id\":\"shamash:next\",\"label\":\"Next page\",\"disabled\":false"
            ),
            "more results exist: {more}"
        );

        // 20 songs, 9 to a page: page three is the last, and holds two.
        let last = serde_json::to_string(&ask("dracula", &offers(2), 2, 20).into_message())
            .expect("serialises");
        assert!(
            last.contains(
                "\"custom_id\":\"shamash:next\",\"label\":\"Next page\",\"disabled\":true"
            ),
            "the last page disables it: {last}"
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

    /// The page count has to be the whole list's, not this page's.
    #[test]
    fn the_chooser_says_which_page_of_how_many() {
        let json = description_of(ask("dracula", &offers(9), 1, 20));
        assert!(json.contains("Page 2 of 3"), "{json}");
        assert!(json.contains("dracula"), "the query is echoed: {json}");
    }
}
