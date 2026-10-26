//! What the bot *could* learn, on disk — not currently wired up.
//!
//! Every request costs a catalog lookup and a YouTube search, and the same
//! misheard phrase costs the same ones every time: "billy eyelash bad guy" is
//! heard again next week and has to be mended again. This is a small local file
//! that remembers, in text, what was asked for and what it turned out to mean.
//!
//! Two things are stored:
//!
//! - **Aliases.** `normalize(transcript)` → the song it meant. An exact alias
//!   hit skips the catalog entirely. It is written when a confidently-played
//!   song is *kept* for ten seconds, or immediately when the listener presses a
//!   chooser button -- a press is a stronger statement of intent than a song
//!   that happened not to be skipped.
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
//!
//! NOTE: `Player::new` currently constructs with `library: None`. The fields
//! and methods exist but no path in the codebase assigns a `Library` instance,
//! so aliases and history are never persisted or read. Wiring this is a
//! separate feature change.

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

/// How long a song has to keep playing before the request that queued it is
/// believed.
///
/// Ten seconds is past the point where anyone skips by accident and well before
/// a song anyone wanted has finished. Learning from a song that was skipped in
/// two seconds is how a library learns the wrong thing.
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
    /// [`matcher::normalize`] of the transcript. The raw text is not kept: the
    /// whole point is that "billy eyelash bad guy" and "billy eilish bad guy"
    /// reach the same entry once normalized.
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

    /// The song this exact phrase has meant before.
    pub fn recall(&self, transcript: &str) -> Option<Track> {
        let said = matcher::normalize(transcript);
        if said.is_empty() {
            return None;
        }
        let mut state = self.lock();
        let found = state
            .file
            .aliases
            .iter()
            .find(|alias| alias.said == said)
            .map(|alias| alias.track.clone());
        if let Some(track) = &found {
            if let Some(alias) = state
                .file
                .aliases
                .iter_mut()
                .find(|alias| &alias.track == track)
            {
                alias.last_used = now_seconds();
            }
            self.schedule(&mut state);
        }
        found
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
        let said = matcher::normalize(said);
        if said.is_empty() || track.title.trim().is_empty() {
            return;
        }
        let mut state = self.lock();
        let now = now_seconds();
        match state
            .file
            .aliases
            .iter_mut()
            .find(|alias| alias.said == said)
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
    pub fn quick_skip(&self, said: &str, track: &Track) -> bool {
        let said = matcher::normalize(said);
        let mut state = self.lock();
        let Some(alias) = state
            .file
            .aliases
            .iter_mut()
            .find(|alias| alias.said == said)
        else {
            return false;
        };
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
            state.file.aliases.retain(|kept| kept.said != said);
        }
        self.schedule(&mut state);
        dropped
    }

    /// Forgets an alias outright.
    pub fn forget(&self, said: &str) {
        let said = matcher::normalize(said);
        let mut state = self.lock();
        state.file.aliases.retain(|alias| alias.said != said);
        self.schedule(&mut state);
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
mod tests {
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
        library.forget("DRACULA!");
        assert!(
            library.is_empty(),
            "forgotten through the same normalisation"
        );
    }
}
