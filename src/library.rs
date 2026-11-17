//! What the bot has learned, on disk, so a repeated phrase skips the network.
//!
//! - **Aliases:** [`phrase_key`] → song, written only when a listener picks
//!   from the chooser (a tolerated guess is not a request).
//! - **History:** every song a guild played, matched via [`crate::matcher`].
//!
//! Text only: no audio, no user ids. Writes are atomic and debounced.

use crate::matcher;
use crate::resolve::{CatalogTrack, Hit};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How many aliases are kept.
///
/// ponytail: ~a few months of daily use; only bounds file growth.
pub const MAX_ALIASES: usize = 2_000;

/// How many history rows are kept, across every guild.
pub const MAX_HISTORY: usize = 5_000;

/// Seconds a recalled song must play before a skip stops counting against it.
pub const KEPT_SECS: u64 = 10;

/// Quick skips before the alias is dropped; one is often just a change of mind.
pub const QUICK_SKIPS_BEFORE_DROP: u32 = 2;

/// How long writes are coalesced for.
const SAVE_DEBOUNCE: Duration = Duration::from_secs(5);

/// A song, as stored; no provider fields, so the file isn't tied to one provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub artist: String,
    pub title: String,
    pub duration_secs: Option<f64>,
    /// The YouTube video, so a repeat skips the search. Optional for older files.
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

    /// As a catalog hit, so the same matcher judges it.
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
    /// [`phrase_key`] of the transcript; re-keyed on compare, so old files still match.
    pub said: String,
    pub track: Track,
    /// Unix seconds, for oldest-first eviction.
    pub last_used: u64,
    /// Whether a chooser press created this.
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
    save_at: Option<Instant>,
}

/// The bot's memory. Cloning shares it, like the queue.
#[derive(Debug, Clone, Default)]
pub struct Library(Arc<Mutex<State>>);

impl Library {
    /// Reads the library at `path`, or starts empty (also when corrupt: it's only a cache).
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

    /// The song this phrase has meant before, via [`find_alias`].
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

    /// Songs this guild has played, as hits for [`crate::matcher`].
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
                // Never downgrade a chooser press.
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

    /// Records a quick skip; returns true when the alias was dropped for it.
    pub fn quick_skip(&self, said: &str, track: &Track) -> bool {
        let said = phrase_key(said);
        let mut state = self.lock();
        let Some(at) = find_alias(&state.file.aliases, &said) else {
            return false;
        };
        let alias = &mut state.file.aliases[at];
        if &alias.track != track {
            // A different song this time, so the skip isn't about this alias.
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

    /// Forgets an alias outright (the Wrong song button), returning its song.
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

    /// How many aliases are remembered.
    pub fn len(&self) -> usize {
        self.lock().file.aliases.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn schedule(&self, state: &mut State) {
        state.save_at = Some(Instant::now() + SAVE_DEBOUNCE);
    }

    /// Writes if the debounce has passed; called from playback, not a timer.
    pub fn maybe_save(&self) {
        let mut state = self.lock();
        if state.save_at.is_some_and(|at| Instant::now() < at) {
            return;
        }
        write(&mut state);
    }

    /// Writes now, ignoring the debounce.
    pub fn save(&self) {
        let mut state = self.lock();
        write(&mut state);
    }
}

/// [`matcher::sim`] floor: "mf dume" (0.92) passes, "Lover" vs "Love Story" (0.82) fails.
pub const CLOSE_PHRASE: f32 = 0.88;

/// How far the best phrase must lead the best one meaning a different song.
pub const CLOSE_MARGIN: f32 = matcher::MARGIN;

/// Shorter phrases match only exactly: "hey" and "hay" are nothing alike.
const CLOSE_MIN_LETTERS: usize = 8;

/// [`matcher::normalize`] with single letters glued, so "m f doom" == "mf doom".
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

/// The numbers in a phrase; "Part 1" and "Part 2" are different songs.
fn numbers(key: &str) -> Vec<&str> {
    key.split_whitespace()
        .filter(|word| word.chars().all(|c| c.is_ascii_digit()))
        .collect()
}

/// Which alias `said` means: exact phrase, else an unambiguous title, else a
/// close phrase with the same numbers.
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

/// Writes atomically (temp file + rename) so a crash never leaves half a file.
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

#[cfg(test)]
#[path = "tests/library.rs"]
mod tests;
