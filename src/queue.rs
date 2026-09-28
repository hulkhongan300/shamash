//! The per-guild play queue and the dashboard shown for it.
//!
//! Songbird plays one track at a time and does not queue for us, so the order
//! songs play in is kept here. A song reaching its end is reported by songbird
//! as a track event, which the player turns into a call to [`advance`], so the
//! next entry starts without a voice command.
//!
//! [`advance`]: Queue::advance

use serenity::model::id::GuildId;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

/// How many songs one guild may have waiting.
///
/// Unbounded, a long queue of downloads would fill the disk; voice commands
/// arrive faster than songs end, so this is generous rather than tight.
pub const MAX_QUEUE: usize = 20;

/// One song waiting to play, or playing now.
#[derive(Debug, Clone, PartialEq)]
pub struct Queued {
    /// Downloaded audio on disk.
    pub path: PathBuf,
    /// Title as shown on the dashboard.
    pub title: String,
    /// Uploading channel, shown as the artist line.
    pub artist: String,
    /// Watch URL, linked from the dashboard title.
    pub url: String,
    /// Video id, used to build the thumbnail URL.
    pub video_id: String,
}

impl Queued {
    /// The thumbnail URL for this video.
    ///
    /// YouTube serves a predictable still per video id. `hqdefault` is always
    /// present, where the higher quality ones are not, so a missing thumbnail
    /// is not something to fall back from.
    pub fn thumbnail(&self) -> String {
        format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", self.video_id)
    }
}

/// One guild's queue, plus whether the current song is paused.
#[derive(Debug, Default)]
struct State {
    /// Not yet played, oldest first.
    pending: VecDeque<Queued>,
    /// The song currently playing, if any.
    current: Option<Queued>,
    /// Whether the current song is paused rather than playing.
    paused: bool,
    /// Counts how many times the current song has been started.
    ///
    /// Songbird reports a track ending both when it finishes and when it is
    /// stopped, so the callback for a song that ends on its own has to tell
    /// itself apart from one already replaced by a skip. Each start takes the
    /// next number, and a callback whose number has been superseded does
    /// nothing.
    generation: u64,
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
    /// A poisoned lock means some other thread panicked mid-update. The queue
    /// is plain data with no invariant worth preserving across a panic, so
    /// recovering beats refusing every later command.
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
        state.pending.push_back(song);
        true
    }

    /// Takes the next song, making it the current one.
    ///
    /// Bumps the generation, so a callback left over from the song that was
    /// playing is told it has been superseded.
    pub fn advance(&self) -> Advanced {
        let mut state = self.lock();
        let stopped = state.current.take();
        state.current = state.pending.pop_front();
        state.paused = false;
        state.generation = state.generation.wrapping_add(1);
        Advanced {
            stopped,
            current: state.current.clone(),
            generation: state.generation,
        }
    }

    /// Takes the next song only if `generation` is still the current one.
    ///
    /// This is what a song calling in its own ending uses. Skipping already
    /// advanced the queue, and songbird reports the skipped song as ended
    /// too, so without this check one skip would drop two songs.
    ///
    /// Returns `None` when the song has already been replaced.
    pub fn advance_if_current(&self, generation: u64) -> Option<Advanced> {
        let mut state = self.lock();
        if state.generation != generation {
            return None;
        }
        let stopped = state.current.take();
        state.current = state.pending.pop_front();
        state.paused = false;
        state.generation = state.generation.wrapping_add(1);
        Some(Advanced {
            stopped,
            current: state.current.clone(),
            generation: state.generation,
        })
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
mod tests {
    use super::*;

    fn song(title: &str) -> Queued {
        Queued {
            path: PathBuf::from(format!("/tmp/{title}.m4a")),
            title: title.to_string(),
            artist: "someone".to_string(),
            url: format!("https://youtu.be/{title}"),
            video_id: title.to_string(),
        }
    }

    #[test]
    fn songs_play_in_the_order_they_were_added() {
        let queue = Queue::default();
        assert!(queue.push(song("first")));
        assert!(queue.push(song("second")));

        let now = queue.advance();
        assert_eq!(now.current.expect("first song").title, "first");
        assert_eq!(queue.pending(), 1);

        let now = queue.advance();
        assert_eq!(now.stopped.expect("stopped song").title, "first");
        assert_eq!(now.current.expect("second song").title, "second");
        assert_eq!(queue.pending(), 0);

        let now = queue.advance();
        assert_eq!(now.stopped.expect("stopped song").title, "second");
        assert!(now.current.is_none(), "nothing is left to play");
    }

    #[test]
    fn skipping_drops_the_song_playing_and_starts_the_next() {
        let queue = Queue::default();
        queue.push(song("skipping me"));
        queue.push(song("playing instead"));

        let now = queue.advance();
        assert_eq!(now.current.expect("first").title, "skipping me");

        let skipped = queue.advance();
        assert_eq!(
            skipped.stopped.expect("the skipped song").title,
            "skipping me"
        );
        assert_eq!(
            skipped.current.expect("replacement").title,
            "playing instead"
        );
    }

    #[test]
    fn a_skip_supersedes_the_skipped_songs_own_ending() {
        // Songbird reports a skipped song as ended, so a skip must not also be
        // counted as the song finishing, which would drop two songs for one
        // button press.
        let queue = Queue::default();
        queue.push(song("playing"));
        queue.push(song("next"));
        queue.push(song("would be lost"));
        let first = queue.advance();

        let skipped = queue.advance();
        assert!(
            queue.advance_if_current(first.generation).is_none(),
            "the ending of a skipped song must be ignored"
        );
        assert_eq!(queue.current().expect("current").title, "next");
        assert_eq!(skipped.current.expect("current").title, "next");
    }

    #[test]
    fn a_song_ending_on_its_own_starts_the_next() {
        let queue = Queue::default();
        queue.push(song("playing"));
        queue.push(song("next"));
        let first = queue.advance();

        let ended = queue
            .advance_if_current(first.generation)
            .expect("the current song ending should advance");
        assert_eq!(ended.stopped.expect("the song that ended").title, "playing");
        assert_eq!(ended.current.expect("the next song").title, "next");
        assert!(
            ended.generation != first.generation,
            "each advance takes a new number"
        );
    }

    #[test]
    fn ending_the_last_song_leaves_nothing_playing() {
        let queue = Queue::default();
        queue.push(song("only"));
        let only = queue.advance();
        assert!(only.current.is_some());

        let ended = queue.advance_if_current(only.generation).expect("advance");
        assert!(ended.stopped.is_some());
        assert!(ended.current.is_none());
        assert!(queue.is_idle(), "the queue should be empty again");
    }

    #[test]
    fn a_stopped_song_does_not_come_back_when_it_reports_ending() {
        let queue = Queue::default();
        queue.push(song("playing"));
        queue.push(song("waiting"));
        let playing = queue.advance();

        assert!(queue.clear().is_some());
        assert!(
            queue.advance_if_current(playing.generation).is_none(),
            "a cleared song must not advance the queue"
        );
        assert_eq!(queue.pending(), 1, "the waiting song survives a stop");
    }

    #[test]
    fn stopping_keeps_the_waiting_songs() {
        // A stop button pauses the queue, it is not a way to throw away
        // everything the listener asked for.
        let queue = Queue::default();
        queue.push(song("playing"));
        queue.push(song("waiting"));
        queue.advance();

        assert!(queue.clear().is_some());
        assert!(queue.current().is_none());
        assert_eq!(queue.pending(), 1, "the waiting song survives a stop");
    }

    #[test]
    fn a_full_queue_refuses_more_songs() {
        let queue = Queue::default();
        for i in 0..MAX_QUEUE {
            assert!(queue.push(song(&format!("song{i}"))), "song {i} should fit");
        }
        assert!(!queue.push(song("one too many")), "the queue is full");
        assert_eq!(queue.pending(), MAX_QUEUE);
    }

    #[test]
    fn advancing_clears_the_paused_flag() {
        let queue = Queue::default();
        queue.push(song("first"));
        queue.push(song("second"));
        queue.advance();
        queue.set_paused(true);
        assert!(queue.is_paused());

        queue.advance();
        assert!(
            !queue.is_paused(),
            "the next song should start playing, not paused"
        );
    }

    #[test]
    fn a_guild_gets_its_own_queue() {
        let queues = Queues::default();
        let one = queues.get(GuildId::new(1));
        let two = queues.get(GuildId::new(2));

        one.push(song("only for one"));
        assert_eq!(one.pending(), 1);
        assert_eq!(two.pending(), 0, "queues must not be shared");
        // The same guild resolves to the same queue.
        assert_eq!(queues.get(GuildId::new(1)).pending(), 1);
    }

    #[test]
    fn a_clone_shares_one_queue() {
        let queue = Queue::default();
        let other = queue.clone();
        queue.push(song("shared"));
        assert_eq!(other.pending(), 1);
    }

    #[test]
    fn the_thumbnail_comes_from_the_video_id() {
        let song = song("dQw4w9WgXcQ");
        assert_eq!(
            song.thumbnail(),
            "https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg"
        );
    }

    #[test]
    fn a_queue_survives_a_poisoned_lock() {
        // A panic elsewhere must not make every later command fail.
        let queue = Queue::default();
        let clone = queue.clone();
        let _ = std::thread::spawn(move || {
            let _guard = clone.0.lock().unwrap();
            panic!("poison the lock");
        })
        .join();

        assert!(queue.push(song("after the panic")));
    }
}
