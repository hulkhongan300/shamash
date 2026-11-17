//! The per-guild play queue; songbird plays one track at a time and does not queue.

use reqwest::header::HeaderMap;
use serenity::model::id::GuildId;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// How many songs one guild may have waiting; unbounded, downloads would fill the disk.
pub const MAX_QUEUE: usize = 20;

/// Where a queued song's audio comes from: a stream starts sooner, the file is the fallback.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// Audio already on disk.
    File(PathBuf),
    /// Audio at a URL, played as it arrives.
    Stream {
        /// The signed format URL; it expires, so it can fail.
        url: String,
        /// Headers the URL was signed for; YouTube answers 403 without them.
        headers: HeaderMap,
        /// Audio length in bytes; YouTube only serves a closed `range` and 403s a plain GET.
        length: u64,
        /// File the stream is written to, and the fallback plays.
        cache: PathBuf,
    },
}

impl Source {
    /// The cache file this song's audio belongs in.
    pub fn cache(&self) -> &Path {
        match self {
            Source::File(path) | Source::Stream { cache: path, .. } => path,
        }
    }
}

/// One song waiting to play, or playing now.
#[derive(Debug, Clone, PartialEq)]
pub struct Queued {
    pub source: Source,
    /// Title as shown on the dashboard.
    pub title: String,
    /// Uploading channel, shown as the artist line.
    pub artist: String,
    /// Watch URL, linked from the dashboard title.
    pub url: String,
    /// Video id, used to build the thumbnail URL.
    pub video_id: String,
    /// The song's length, when YouTube reported it.
    pub duration_secs: Option<f64>,
    /// How it was asked for, shown on the dashboard.
    pub origin: Origin,
    /// Words song memory recalled this for; the Wrong song button needs them to forget.
    pub remembered: Option<String>,
}

/// How a song came to be queued.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Origin {
    /// Spoken, or picked from the chooser a spoken request raised.
    #[default]
    Voice,
    /// Typed with `/play`.
    Slash,
    /// One track of a playlist or album queued with `/play`.
    Playlist,
}

impl Queued {
    /// The thumbnail URL; `hqdefault` always exists, higher qualities may not.
    pub fn thumbnail(&self) -> String {
        format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", self.video_id)
    }
}

/// A waiting song with a stable id, so remove/move hit the picked song even after positions shift.
#[derive(Debug, Clone, PartialEq)]
pub struct Waiting {
    /// Unique per guild queue, never reused.
    pub id: u64,
    pub song: Queued,
}

/// Where a waiting song is moved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shift {
    /// One place sooner.
    Up,
    /// One place later.
    Down,
    /// To the front, so it plays next.
    Front,
}

/// One guild's queue, plus whether the current song is paused.
#[derive(Debug, Default)]
struct State {
    pending: VecDeque<Waiting>,
    next_id: u64,
    current: Option<Queued>,
    paused: bool,
    /// Bumped per start; songbird reports "ended" for skips too, so callbacks check this.
    generation: u64,
    repeat: bool,
}

/// Repeat-one: requeues the finished song only when nothing else is waiting.
fn requeue_if_repeating(state: &mut State, stopped: &Option<Queued>) {
    if state.repeat
        && state.pending.is_empty()
        && let Some(song) = stopped
    {
        state.enqueue(song.clone());
    }
}

impl State {
    fn enqueue(&mut self, song: Queued) {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.pending.push_back(Waiting { id, song });
    }

    fn take_next(&mut self) {
        self.current = self.pending.pop_front().map(|waiting| waiting.song);
    }

    fn position(&self, id: u64) -> Option<usize> {
        self.pending.iter().position(|waiting| waiting.id == id)
    }
}

/// The result of moving a queue on to the next song.
#[derive(Debug, Clone, PartialEq)]
pub struct Advanced {
    /// The song that was playing, now finished.
    pub stopped: Option<Queued>,
    /// The song to play now, if the queue had another.
    pub current: Option<Queued>,
    /// Pass to the next end-of-track callback.
    pub generation: u64,
}

/// The songs queued for one guild; clones share it.
#[derive(Debug, Clone, Default)]
pub struct Queue(Arc<Mutex<State>>);

impl Queue {
    /// Locks, ignoring poison: plain data with no invariant worth refusing commands over.
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds a song to the end of the queue; false when full.
    pub fn push(&self, song: Queued) -> bool {
        let mut state = self.lock();
        if state.pending.len() >= MAX_QUEUE {
            return false;
        }
        state.enqueue(song);
        true
    }

    /// Makes the next song current and bumps the generation.
    pub fn advance(&self) -> Advanced {
        let mut state = self.lock();
        let stopped = state.current.take();
        requeue_if_repeating(&mut state, &stopped);
        state.take_next();
        state.paused = false;
        state.generation = state.generation.wrapping_add(1);
        Advanced {
            stopped,
            current: state.current.clone(),
            generation: state.generation,
        }
    }

    /// Advances only if `generation` is current, so a skip's "ended" event doesn't drop two songs.
    pub fn advance_if_current(&self, generation: u64) -> Option<Advanced> {
        let mut state = self.lock();
        if state.generation != generation {
            return None;
        }
        let stopped = state.current.take();
        requeue_if_repeating(&mut state, &stopped);
        state.take_next();
        state.paused = false;
        state.generation = state.generation.wrapping_add(1);
        Some(Advanced {
            stopped,
            current: state.current.clone(),
            generation: state.generation,
        })
    }

    /// Whether a song that ends with nothing behind it plays again.
    pub fn is_repeat(&self) -> bool {
        self.lock().repeat
    }

    /// Toggles repeat and returns the new setting.
    pub fn toggle_repeat(&self) -> bool {
        let mut state = self.lock();
        state.repeat = !state.repeat;
        state.repeat
    }

    /// Drops the current song without starting another.
    pub fn clear(&self) -> Option<Queued> {
        let mut state = self.lock();
        state.paused = false;
        // So its "ended" event can't advance the queue.
        state.generation = state.generation.wrapping_add(1);
        state.current.take()
    }

    /// The song playing now, if any.
    pub fn current(&self) -> Option<Queued> {
        self.lock().current.clone()
    }

    /// How many songs are waiting behind the current one.
    pub fn pending(&self) -> usize {
        self.lock().pending.len()
    }

    /// The songs waiting behind the current one, soonest first.
    pub fn pending_songs(&self) -> Vec<Queued> {
        self.lock()
            .pending
            .iter()
            .map(|waiting| waiting.song.clone())
            .collect()
    }

    /// The waiting songs, soonest first, with the ids `remove` and `shift` take.
    pub fn waiting(&self) -> Vec<Waiting> {
        self.lock().pending.iter().cloned().collect()
    }

    /// Takes a waiting song out; `None` if it already started or was removed.
    pub fn remove(&self, id: u64) -> Option<Queued> {
        let mut state = self.lock();
        let index = state.position(id)?;
        state.pending.remove(index).map(|waiting| waiting.song)
    }

    /// Moves a waiting song and returns its new 0-based position; `None` if no longer waiting.
    pub fn shift(&self, id: u64, shift: Shift) -> Option<usize> {
        let mut state = self.lock();
        let from = state.position(id)?;
        let last = state.pending.len() - 1;
        let to = match shift {
            Shift::Up => from.saturating_sub(1),
            Shift::Down => (from + 1).min(last),
            Shift::Front => 0,
        };
        let waiting = state.pending.remove(from)?;
        state.pending.insert(to, waiting);
        Some(to)
    }

    /// Whether the current song is paused.
    pub fn is_paused(&self) -> bool {
        self.lock().paused
    }

    /// Records that the current song was paused or resumed.
    pub fn set_paused(&self, paused: bool) {
        self.lock().paused = paused;
    }

    /// Whether anything is playing or waiting.
    pub fn is_idle(&self) -> bool {
        let state = self.lock();
        state.current.is_none() && state.pending.is_empty()
    }

    /// Whether the queue is at its limit.
    pub fn is_full(&self) -> bool {
        self.lock().pending.len() >= MAX_QUEUE
    }

    /// Cache files the pruner must keep: playing and waiting songs, streams included.
    pub fn in_use(&self) -> Vec<PathBuf> {
        let state = self.lock();
        state
            .current
            .iter()
            .chain(state.pending.iter().map(|waiting| &waiting.song))
            .map(|song| song.source.cache().to_path_buf())
            .collect()
    }
}

/// Every guild's queue; clones share the map.
#[derive(Debug, Clone, Default)]
pub struct Queues(Arc<Mutex<std::collections::HashMap<GuildId, Queue>>>);

impl Queues {
    /// The queue for a guild, created on first use.
    pub fn get(&self, guild_id: GuildId) -> Queue {
        let mut queues = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        queues.entry(guild_id).or_default().clone()
    }
}

#[cfg(test)]
#[path = "tests/queue.rs"]
mod tests;
