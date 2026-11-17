//! What the bot has learned, on disk.
//!
//! Every request costs a catalog lookup and a YouTube search, and the same
//! misheard phrase costs the same ones every time: "billy eyelash bad guy" is
//! heard again next week and has to be mended again. This is a small local file
//! that remembers, in text, what was asked for and what it turned out to mean.
//!
//! Two things are stored:
//!
//! - **Aliases.** [`phrase_key`]`(transcript)` → the song it meant. An alias
//!   hit skips the catalog entirely. The same words hit, and so do the same
//!   words said a little differently (a close mishearing, initials spelled
//!   out) or the title alone of a song only one alias means; see
//!   [`find_alias`] for how strict that is. It is written only when a listener picks
//!   a song from the chooser for a spoken request: a song the bot chose by
//!   itself is never remembered, however long it plays, because putting up with
//!   a guess is not the same as asking for it.
//! - **History.** Every catalog song this guild has played, with a count. A
//!   transcript that fuzzily matches one of them is resolved without a network
//!   call too, using [`crate::matcher`] -- the same matcher that decides between
//!   catalog hits, so a remembered song and a catalog song are judged alike.
//!
//! Deliberately text only: no audio, no user ids, nothing about who spoke. A
//! file of song titles cannot identify anybody.
//!
//! Writes are atomic (a temp file, then a rename) and debounced, because a
//! single request can finish several of them and a half-written JSON file that
//! is read back at the next startup would throw away everything.

use crate::matcher;
use crate::resolve::{CatalogTrack, Hit};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How many aliases are kept.
///
/// ponytail: 2000 is about a few months of a daily listener. The cap exists so
/// a long-running bot cannot grow the file without bound, not because there is a
/// known need for more.
pub const MAX_ALIASES: usize = 2_000;

/// How many history rows are kept, across every guild.
pub const MAX_HISTORY: usize = 5_000;

/// How long a remembered song has to play before a skip no longer counts
/// against the phrase that recalled it.
///
/// Ten seconds is past the point where anyone skips by accident and well before
/// a song anyone wanted has finished.
pub const KEPT_SECS: u64 = 10;

/// How many times a song has to be skipped that fast before the alias is
/// dropped.
///
/// Two, not one: the first skip is often the listener changing their mind, and a
/// single press is weak evidence against a pattern that has been right many
/// times.
pub const QUICK_SKIPS_BEFORE_DROP: u32 = 2;

/// How long writes are coalesced for.
///
/// Every request would otherwise rewrite the whole file, and the file is small
/// enough that a burst of them is mostly wasted IO.
const SAVE_DEBOUNCE: Duration = Duration::from_secs(5);

/// A song, as stored.
///
/// A smaller record than [`CatalogTrack`]: the provider that found it and the
/// popularity it reported are facts about that lookup, not about the song, and
/// storing them would tie the file to one provider's vocabulary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub artist: String,
    pub title: String,
    pub duration_secs: Option<f64>,
    /// The YouTube video this song is on, when one is known.
    ///
    /// Stored so a phrase heard again goes straight to the download instead of
    /// through the catalog and a YouTube search a second time. Optional and
    /// omitted when empty, so a library written before this existed still reads
    /// and is only worth one lookup less per alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_id: Option<String>,
}

impl Track {
    pub fn label(&self) -> String {
        format!("{} — {}", self.artist, self.title)
    }

    /// The catalog track this memory stands for.
    pub fn to_catalog_track(&self) -> CatalogTrack {
        CatalogTrack {
            artist: self.artist.clone(),
            title: self.title.clone(),
            duration_secs: self.duration_secs,
            popularity: None,
            video_id: self.video_id.clone(),
        }
    }

    /// As a catalog hit, so remembered songs are judged by the same matcher as
    /// catalog ones.
    pub fn as_hit(&self) -> Hit {
        Hit {
            artist: self.artist.clone(),
            title: self.title.clone(),
            durations: self.duration_secs.into_iter().collect(),
            duration_uncertain: false,
            popularity: None,
            video_id: self.video_id.clone(),
        }
    }
}

/// A phrase, and what it turned out to mean.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alias {
    /// [`phrase_key`] of the transcript. The raw text is not kept: the whole
    /// point is that "M.F. DOOM" and "mf doom" reach the same entry. A file
    /// written before the key glued letters still reads, because stored phrases
    /// are keyed again when compared.
    pub said: String,
    pub track: Track,
    /// Unix seconds. Read once, so entries can be dropped oldest-first.
    pub last_used: u64,
    /// Whether a press of a chooser button created this, rather than a song that
    /// was played and not skipped.
    pub from_chooser: bool,
    /// Times this was skipped faster than [`KEPT_SECS`].
    pub quick_skips: u32,
}

/// One guild's memory of one song.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryRow {
    pub guild: u64,
    pub track: Track,
    pub plays: u32,
    pub last_played: u64,
}

/// The file on disk.
#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    aliases: Vec<Alias>,
    #[serde(default)]
    history: Vec<HistoryRow>,
}

#[derive(Debug, Default)]
struct State {
    file: File,
    path: Option<PathBuf>,
    /// When a write was last asked for, so a burst is coalesced.
    save_at: Option<Instant>,
}

/// The bot's memory. Cloning shares it, like the queue.
#[derive(Debug, Clone, Default)]
pub struct Library(Arc<Mutex<State>>);

impl Library {
    /// Reads the library at `path`, or starts empty.
    ///
    /// A missing file is not an error -- it is the first run. A corrupt one is
    /// also not an error: it is logged and the bot starts empty, because a
    /// library is an optimisation and being unable to play because a JSON file
    /// was truncated by a power cut would be absurd.
    pub fn load(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            return Self::default();
        };
        let file = match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => File::default(),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %format!("{e:#}"),
                    "could not read the song library; starting empty"
                );
                File::default()
            }
            Ok(text) => match serde_json::from_str::<File>(&text) {
                Ok(file) => file,
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %format!("{e:#}"),
                        "the song library is unreadable; starting empty"
                    );
                    File::default()
                }
            },
        };
        tracing::info!(
            path = %path.display(),
            aliases = file.aliases.len(),
            history = file.history.len(),
            "song library loaded"
        );
        Self(Arc::new(Mutex::new(State {
            file,
            path: Some(path.to_path_buf()),
            save_at: None,
        })))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The song this phrase has meant before.
    ///
    /// Found by [`find_alias`]: the same words, the same words said a little
    /// differently, or the title of exactly one remembered song.
    pub fn recall(&self, transcript: &str) -> Option<Track> {
        let said = phrase_key(transcript);
        if said.is_empty() {
            return None;
        }
        let mut state = self.lock();
        let at = find_alias(&state.file.aliases, &said)?;
        let alias = &mut state.file.aliases[at];
        alias.last_used = now_seconds();
        let track = alias.track.clone();
        self.schedule(&mut state);
        Some(track)
    }

    /// Songs this guild has played that the request plausibly names.
    ///
    /// Scored by [`crate::matcher`], the same way catalog hits are, so a
    /// remembered song is never treated as more certain than a catalog one just
    /// because it is nearby.
    pub fn history_hits(&self, guild: u64) -> Vec<Hit> {
        let state = self.lock();
        state
            .file
            .history
            .iter()
            .filter(|row| row.guild == guild)
            .map(|row| row.track.as_hit())
            .collect()
    }

    /// Records that a confidently-chosen song was queued for `said`.
    ///
    /// `from_chooser` marks a press of a chooser button, which is believed
    /// immediately: the listener chose it, so there is nothing to wait for.
    /// Everything else waits for [`KEPT_SECS`] -- see [`Self::finished`].
    pub fn remember(&self, guild: u64, said: &str, track: &Track, from_chooser: bool) {
        let said = phrase_key(said);
        if said.is_empty() || track.title.trim().is_empty() {
            return;
        }
        let mut state = self.lock();
        let now = now_seconds();
        match state
            .file
            .aliases
            .iter_mut()
            .find(|alias| phrase_key(&alias.said) == said)
        {
            Some(alias) => {
                alias.track = track.clone();
                alias.last_used = now;
                alias.quick_skips = 0;
                // A press is stronger evidence than a kept song, so it does not
                // downgrade to the weaker kind.
                alias.from_chooser |= from_chooser;
            }
            None => {
                state.file.aliases.push(Alias {
                    said,
                    track: track.clone(),
                    last_used: now,
                    from_chooser,
                    quick_skips: 0,
                });
            }
        }
        match state
            .file
            .history
            .iter_mut()
            .find(|row| row.guild == guild && row.track == *track)
        {
            Some(row) => {
                row.plays = row.plays.saturating_add(1);
                row.last_played = now;
            }
            None => state.file.history.push(HistoryRow {
                guild,
                track: track.clone(),
                plays: 1,
                last_played: now,
            }),
        }
        trim(&mut state.file);
        self.schedule(&mut state);
    }

    /// Records that a queued song was skipped before it had been played.
    ///
    /// Returns whether this was the second such skip, in which case the alias is
    /// dropped: the bot has been taught the wrong thing twice.
    ///
    /// The alias is found the same way [`Self::recall`] found it, so a song
    /// recalled by words said a little differently still counts against the
    /// phrase that recalled it.
    pub fn quick_skip(&self, said: &str, track: &Track) -> bool {
        let said = phrase_key(said);
        let mut state = self.lock();
        let Some(at) = find_alias(&state.file.aliases, &said) else {
            return false;
        };
        let alias = &mut state.file.aliases[at];
        if &alias.track != track {
            // The request resolved to a different song this time, so a quick
            // skip is about that song, not about this alias.
            return false;
        }
        alias.quick_skips = alias.quick_skips.saturating_add(1);
        let dropped = alias.quick_skips >= QUICK_SKIPS_BEFORE_DROP;
        if dropped {
            tracing::info!(
                said = %alias.said,
                track = %alias.track.label(),
                "skipped twice as fast as any song lasts; forgetting that phrase"
            );
            state.file.aliases.remove(at);
        }
        self.schedule(&mut state);
        dropped
    }

    /// Forgets an alias outright, and returns the song it meant.
    ///
    /// The Wrong song button's job: the listener has said, in one press, what
    /// two quick skips would otherwise have to show.
    pub fn forget(&self, said: &str) -> Option<Track> {
        let said = phrase_key(said);
        let mut state = self.lock();
        let index = state
            .file
            .aliases
            .iter()
            .position(|alias| phrase_key(&alias.said) == said)?;
        let forgotten = state.file.aliases.remove(index);
        tracing::info!(
            said = %forgotten.said,
            track = %forgotten.track.label(),
            "told it was the wrong song; forgetting that phrase"
        );
        self.schedule(&mut state);
        Some(forgotten.track)
    }

    /// How many aliases are remembered, for the startup log and the tests.
    pub fn len(&self) -> usize {
        self.lock().file.aliases.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Queues a write, coalescing a burst into one.
    fn schedule(&self, state: &mut State) {
        state.save_at = Some(Instant::now() + SAVE_DEBOUNCE);
    }

    /// Writes anything whose debounce has passed. Called from the playback path,
    /// so the file is flushed on the way past rather than on a timer task.
    pub fn maybe_save(&self) {
        let mut state = self.lock();
        if state.save_at.is_some_and(|at| Instant::now() < at) {
            return;
        }
        write(&mut state);
    }

    /// Writes now, whatever the debounce says. For shutdown and for tests.
    pub fn save(&self) {
        let mut state = self.lock();
        write(&mut state);
    }
}

/// How close a phrase has to be to a remembered one to be taken for it.
///
/// Measured by [`matcher::sim`], so a mishearing that sounds the same ("mf
/// dume" for "mf doom") reaches its phonetic floor of 0.92, and a one-letter
/// slip in a phrase of any length clears it. "Lover" for "Love Story" scores
/// about 0.82 and does not: a different song is worse than asking again.
pub const CLOSE_PHRASE: f32 = 0.88;

/// How far the closest remembered phrase has to be ahead of the closest one
/// that means a different song. Two remembered songs both near what was said
/// is exactly the case where guessing goes wrong.
pub const CLOSE_MARGIN: f32 = matcher::MARGIN;

/// The shortest phrase, in letters, that is matched by closeness at all.
///
/// One edit is a large part of a short phrase: "hey" and "hay" are a letter
/// apart and nothing alike. Short phrases are only ever found exactly.
const CLOSE_MIN_LETTERS: usize = 8;

/// A phrase as it is compared: [`matcher::normalize`], with runs of single
/// letters glued back into a word.
///
/// Transcription spells initials out: "M.F. DOOM" comes back as "m f doom",
/// "A$AP" as "a ap". Gluing makes "m f doom" and "mf doom" one phrase. Digits
/// are left alone, since "2 4 6" is not a word.
pub fn phrase_key(text: &str) -> String {
    let normalized = matcher::normalize(text);
    let mut words: Vec<String> = Vec::new();
    let mut glued = false;
    for word in normalized.split_whitespace() {
        let letter = word.chars().count() == 1 && word.chars().all(char::is_alphabetic);
        match words.last_mut() {
            Some(last) if letter && glued => last.push_str(word),
            _ => words.push(word.to_string()),
        }
        glued = letter;
    }
    words.join(" ")
}

/// The numbers in a phrase. Two phrases that differ in a number name different
/// songs ("Part 1", "Part 2"), however close the rest is.
fn numbers(key: &str) -> Vec<&str> {
    key.split_whitespace()
        .filter(|word| word.chars().all(|c| c.is_ascii_digit()))
        .collect()
}

/// Which remembered phrase `said` (a [`phrase_key`]) stands for, if any.
///
/// In order:
///
/// 1. The same phrase.
/// 2. The title alone, when exactly one remembered song has that title. With
///    two ("Hello" by Adele and by Lionel Richie) the title says nothing about
///    which, so neither is taken.
/// 3. A close phrase: at least [`CLOSE_PHRASE`] alike, with the same numbers,
///    and [`CLOSE_MARGIN`] ahead of any phrase that means a different song.
fn find_alias(aliases: &[Alias], said: &str) -> Option<usize> {
    if said.is_empty() {
        return None;
    }
    let keys: Vec<String> = aliases
        .iter()
        .map(|alias| phrase_key(&alias.said))
        .collect();
    if let Some(at) = keys.iter().position(|key| key == said) {
        return Some(at);
    }

    let titled: Vec<usize> = (0..aliases.len())
        .filter(|&at| phrase_key(&aliases[at].track.title) == said)
        .collect();
    if let Some(&first) = titled.first()
        && titled
            .iter()
            .all(|&at| aliases[at].track == aliases[first].track)
    {
        return Some(first);
    }

    if said.chars().filter(|c| c.is_alphabetic()).count() < CLOSE_MIN_LETTERS {
        return None;
    }
    let said_numbers = numbers(said);
    let scored: Vec<(usize, f32)> = keys
        .iter()
        .enumerate()
        .filter(|(_, key)| numbers(key) == said_numbers)
        .map(|(at, key)| (at, matcher::sim(said, key)))
        .collect();
    let &(best, score) = scored.iter().max_by(|a, b| a.1.total_cmp(&b.1))?;
    if score < CLOSE_PHRASE {
        return None;
    }
    let rival = scored
        .iter()
        .filter(|(at, _)| aliases[*at].track != aliases[best].track)
        .map(|&(_, score)| score)
        .fold(0.0_f32, f32::max);
    (score - rival >= CLOSE_MARGIN).then_some(best)
}

/// Keeps the files bounded, dropping the least recently used first.
fn trim(file: &mut File) {
    if file.aliases.len() > MAX_ALIASES {
        file.aliases.sort_by_key(|alias| alias.last_used);
        file.aliases.drain(..file.aliases.len() - MAX_ALIASES);
    }
    if file.history.len() > MAX_HISTORY {
        file.history.sort_by_key(|row| row.last_played);
        file.history.drain(..file.history.len() - MAX_HISTORY);
    }
}

/// Writes the file atomically: a temp file in the same directory, then a rename.
///
/// A rename within one directory is atomic on every filesystem this will run
/// on, so a crash mid-write leaves the previous file intact rather than half a
/// JSON document. Writing in place would leave exactly that.
fn write(state: &mut State) {
    state.save_at = None;
    let Some(path) = state.path.clone() else {
        return;
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(path = %parent.display(), error = %format!("{e:#}"), "could not create the library directory");
        return;
    }
    let temp = path.with_extension("json.tmp");
    let text = match serde_json::to_string(&state.file) {
        Ok(text) => text,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "could not serialise the song library");
            return;
        }
    };
    if let Err(e) = std::fs::write(&temp, text) {
        tracing::warn!(path = %temp.display(), error = %format!("{e:#}"), "could not write the song library");
        return;
    }
    if let Err(e) = std::fs::rename(&temp, &path) {
        tracing::warn!(path = %path.display(), error = %format!("{e:#}"), "could not save the song library");
        let _ = std::fs::remove_file(&temp);
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Keeps the file small and the useful entries, by dropping the oldest.
#[cfg(test)]
#[path = "tests/library.rs"]
mod tests;
