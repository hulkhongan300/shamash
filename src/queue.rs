//! The per-guild play queue.
//!
//! Songbird plays one track at a time and does not queue for us, so the order
//! songs play in is kept here. A song reaching its end is reported as a track
//! event, which the player turns into a call to [`advance`].
//!
//! [`advance`]: Queue::advance

use reqwest::header::HeaderMap;
use serenity::model::id::GuildId;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// How many songs one guild may have waiting.
///
/// Unbounded, a long queue of downloads would fill the disk.
pub const MAX_QUEUE: usize = 20;

/// Where a queued song's audio comes from.
///
/// Two, because waiting for a whole download before the first sample is the
/// larger half of the wait: a stream starts playing while the rest of it is
/// still arriving, and the download carries on in the background into the cache
/// so the next time this song is asked for it plays instantly. When the stream
/// cannot be used -- YouTube refusing a URL that has expired, or a connection
/// that never delivers -- the song plays from the file instead.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// Audio already on disk.
    File(PathBuf),
    /// Audio at a URL, played as it arrives.
    Stream {
        /// The signed format URL. It expires, so it is a source that can fail
        /// rather than one that always works.
        url: String,
        /// The headers the URL must be requested with. YouTube signs the URL
        /// against the client that asked for it and answers 403 without them.
        headers: HeaderMap,
        /// How long the audio is, in bytes.
        ///
        /// Carried because the request cannot be made without it: YouTube serves
        /// this URL only for a closed `range`, `bytes=0-<length-1>`, and
        /// refuses a plain GET with 403.
        length: u64,
        /// The file the same audio is written to while it streams, and the one
        /// a fallback plays if the stream turns out to be unusable.
        cache: PathBuf,
    },
}

impl Source {
    /// The cache file this song's audio belongs in.
    ///
    /// For a stream that is where the background write is going, and for a
    /// download that is where the audio already is. The cache must never prune
    /// either.
    pub fn cache(&self) -> &Path {
        match self {
            Source::File(path) | Source::Stream { cache: path, .. } => path,
        }
    }
}

/// One song waiting to play, or playing now.
#[derive(Debug, Clone, PartialEq)]
pub struct Queued {
    /// The audio to play, and where it comes from.
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
    /// The words the song memory recalled this for, when it played because it
    /// was remembered rather than looked up. The dashboard says so and offers
    /// a Wrong song button, which needs these words to forget them.
    pub remembered: Option<String>,
}

/// How a song came to be queued.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Origin {
    /// Said to the bot in the voice channel, or picked from the chooser a
    /// spoken request raised.
    #[default]
    Voice,
    /// Typed with `/play`.
    Slash,
    /// One track of a playlist or album queued with `/play`.
    Playlist,
}

impl Queued {
    /// The thumbnail URL for this video.
    ///
    /// `hqdefault` is always present, where the higher quality ones are not.
    pub fn thumbnail(&self) -> String {
        format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", self.video_id)
    }
}

/// A song waiting to play, with the number that names it while it waits.
///
/// Positions shift whenever a song finishes or is added, so a change picked
/// from the queue view names the song by this number instead: a remove or a
/// move then acts on the song that was picked, never on whatever has since
/// slid into its place.
#[derive(Debug, Clone, PartialEq)]
pub struct Waiting {
    /// Unique within one guild's queue, and never reused.
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
    /// Not yet played, oldest first.
    pending: VecDeque<Waiting>,
    /// The id the next song to join `pending` gets.
    next_id: u64,
    /// The song currently playing, if any.
    current: Option<Queued>,
    /// Whether the current song is paused rather than playing.
    paused: bool,
    /// Counts how many times the current song has been started.
    ///
    /// Songbird reports a track ending both when it finishes and when it is
    /// stopped, so the callback for a song that ends on its own has to tell
    /// itself apart from one already replaced by a skip.
    generation: u64,
    /// Whether a song that ends with nothing queued behind it goes back on.
    repeat: bool,
}

/// Puts a finished song back on the queue when repeat is on and nothing else is
/// waiting.
///
/// Repeat means repeat *one*: a song that ends with the queue empty goes back on
/// rather than the queue restarting from the top, because there is no top to
/// restart from once the list has run down. Called before the next song is
/// taken, so the requeued song is what plays next.
fn requeue_if_repeating(state: &mut State, stopped: &Option<Queued>) {
    if state.repeat
        && state.pending.is_empty()
        && let Some(song) = stopped
    {
        state.enqueue(song.clone());
    }
}

impl State {
    /// Adds a song to the end of the waiting list under a fresh id.
    fn enqueue(&mut self, song: Queued) {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.pending.push_back(Waiting { id, song });
    }

    /// Makes the next waiting song the current one.
    fn take_next(&mut self) {
        self.current = self.pending.pop_front().map(|waiting| waiting.song);
    }

    /// Where the waiting song with this id is now, if it is still waiting.
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
    /// The number identifying this advance, to pass to the next callback.
    pub generation: u64,
}

/// The songs queued for one guild.
///
/// Cloning shares the same queue, so the voice pipeline and the interaction
/// handler both see one list.
#[derive(Debug, Clone, Default)]
pub struct Queue(Arc<Mutex<State>>);

impl Queue {
    /// Locks the queue, recovering from a panic elsewhere in the process.
    ///
    /// The queue is plain data with no invariant worth preserving across a
    /// panic, so recovering beats refusing every later command.
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds a song to the end of the queue.
    ///
    /// Returns false when the queue is full, so the caller can say so rather
    /// than silently dropping the request.
    pub fn push(&self, song: Queued) -> bool {
        let mut state = self.lock();
        if state.pending.len() >= MAX_QUEUE {
            return false;
        }
        state.enqueue(song);
        true
    }

    /// Takes the next song, making it the current one, and bumps the generation so
    /// a callback left over from the previous song knows it is superseded.
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

    /// Takes the next song only if `generation` is still the current one, or
    /// `None` when the song has already been replaced.
    ///
    /// This is what a song calling in its own ending uses: skipping advances
    /// the queue already and songbird reports the skipped song as ended too, so
    /// without the check one skip would drop two songs.
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

    /// Turns replaying the current song on or off, and reports the new setting
    /// so the caller can say which way it went.
    pub fn toggle_repeat(&self) -> bool {
        let mut state = self.lock();
        state.repeat = !state.repeat;
        state.repeat
    }

    /// Drops the current song without starting another.
    pub fn clear(&self) -> Option<Queued> {
        let mut state = self.lock();
        state.paused = false;
        // A song cleared this way must not come back when it is reported as
        // ended.
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

    /// The songs waiting behind the current one, soonest first, each with the
    /// id that [`remove`] and [`shift`] take.
    ///
    /// [`remove`]: Queue::remove
    /// [`shift`]: Queue::shift
    pub fn waiting(&self) -> Vec<Waiting> {
        self.lock().pending.iter().cloned().collect()
    }

    /// Takes a waiting song out of the queue.
    ///
    /// `None` when no song with that id is waiting any more -- it started
    /// playing, or was removed already -- in which case nothing changes. The
    /// song playing now is never in the waiting list, so it cannot be removed
    /// this way; that is what skip is for.
    pub fn remove(&self, id: u64) -> Option<Queued> {
        let mut state = self.lock();
        let index = state.position(id)?;
        state.pending.remove(index).map(|waiting| waiting.song)
    }

    /// Moves a waiting song, and returns its new position, counting from 0 for
    /// the song that plays next.
    ///
    /// `None` when no song with that id is waiting any more, in which case
    /// nothing changes. A song already first moved up, or already last moved
    /// down, stays where it is.
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

    /// Every song whose audio must be kept: the one playing, and every one
    /// waiting behind it.
    ///
    /// Used by the cache to know which downloads it is not allowed to prune.
    /// A queued song's audio is read when it starts, so deleting it because too
    /// many newer files arrived plays silence instead of the song. A stream's
    /// cache file counts: it is the audio a later request and a repeat will
    /// read without waiting for YouTube again.
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

/// The queues of every guild the bot is in, keyed by guild.
///
/// Cloning shares the same map, so the pipeline and the interaction handler
/// agree on what is playing.
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
