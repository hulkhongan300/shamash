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
}

impl Queued {
    /// The thumbnail URL for this video.
    ///
    /// `hqdefault` is always present, where the higher quality ones are not.
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
        state.pending.push_back(song.clone());
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
        state.pending.push_back(song);
        true
    }

    /// Takes the next song, making it the current one, and bumps the generation so
    /// a callback left over from the previous song knows it is superseded.
    pub fn advance(&self) -> Advanced {
        let mut state = self.lock();
        let stopped = state.current.take();
        requeue_if_repeating(&mut state, &stopped);
        state.current = state.pending.pop_front();
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
        state.current = state.pending.pop_front();
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
        self.lock().pending.iter().cloned().collect()
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
            .chain(state.pending.iter())
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
mod tests {
    use super::*;

    fn song(title: &str) -> Queued {
        Queued {
            source: Source::File(PathBuf::from(format!("/tmp/{title}.m4a"))),
            title: title.to_string(),
            artist: "someone".to_string(),
            url: format!("https://youtu.be/{title}"),
            video_id: title.to_string(),
        }
    }

    /// A song whose audio is still arriving, which is what most queued songs
    /// are: nothing is on disk yet, but the file the stream will be written to
    /// has to be treated the same as one that is.
    fn streaming(title: &str) -> Queued {
        Queued {
            source: Source::Stream {
                url: format!("https://video.example/{title}"),
                headers: HeaderMap::new(),
                length: 1024,
                cache: PathBuf::from(format!("/tmp/{title}.m4a")),
            },
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

    /// The cache must know which audio is still wanted: the song playing and
    /// every one queued behind it. `prune` never deletes these.
    #[test]
    fn in_use_covers_the_playing_song_and_the_whole_queue() {
        let queue = Queue::default();
        assert!(
            queue.in_use().is_empty(),
            "an empty queue needs nothing kept"
        );

        assert!(queue.push(song("first")));
        assert!(queue.push(song("second")));
        assert_eq!(
            queue.in_use(),
            vec![
                PathBuf::from("/tmp/first.m4a"),
                PathBuf::from("/tmp/second.m4a")
            ]
        );

        queue.advance();
        // The playing song is still wanted, and the queue behind it too.
        assert_eq!(queue.in_use().len(), 2);

        queue.clear();
        // With nothing playing, only the waiting song is still wanted.
        assert_eq!(queue.in_use(), vec![PathBuf::from("/tmp/second.m4a")]);

        queue.advance();
        assert_eq!(
            queue.in_use(),
            vec![PathBuf::from("/tmp/second.m4a")],
            "the song that moved to playing is still wanted"
        );
        queue.clear();
        assert!(queue.in_use().is_empty());
    }

    /// A stream's audio is not on disk when it is queued, but the file it is
    /// being written to is exactly what a later request will read, so the cache
    /// must keep it. Pruning it would make the repeat download the song again.
    #[test]
    fn in_use_keeps_the_cache_file_a_stream_is_being_written_to() {
        let queue = Queue::default();
        assert!(queue.push(streaming("streaming")));
        assert_eq!(queue.in_use(), vec![PathBuf::from("/tmp/streaming.m4a")]);
        assert_eq!(streaming("x").source.cache(), Path::new("/tmp/x.m4a"));
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
    /// The point of repeat: a song that ends with nothing behind it plays
    /// again, rather than the queue running dry.
    #[test]
    fn repeat_plays_a_song_again_when_nothing_is_queued_behind_it() {
        let queue = Queue::default();
        assert!(queue.push(song("only")));
        assert!(queue.advance().current.is_some(), "the song is playing");
        assert!(!queue.is_repeat(), "repeat is off to begin with");

        assert!(queue.toggle_repeat(), "toggle turns it on");
        let now = queue.advance();
        assert_eq!(
            now.stopped.expect("stopped song").title,
            "only",
            "the song that finished is the one reported as stopped"
        );
        assert_eq!(
            now.current.expect("played again").title,
            "only",
            "and it is what plays next"
        );
    }

    /// Repeat one is not repeat-all: a song that ends with others waiting plays
    /// the next one, and only comes back when the list has run down.
    #[test]
    fn repeat_does_not_interrupt_the_queue_behind_the_song() {
        let queue = Queue::default();
        assert!(queue.toggle_repeat());
        assert!(queue.push(song("first")));
        assert!(queue.push(song("second")));
        queue.advance();

        let now = queue.advance();
        assert_eq!(now.current.expect("the next song").title, "second");
        assert_eq!(
            queue.pending(),
            0,
            "the finished song did not jump the queue"
        );

        // The list has run down now, so the last song comes round again.
        let now = queue.advance();
        assert_eq!(now.current.expect("repeats last").title, "second");
    }

    /// Turning it off must actually stop it, or the button is a one-way door.
    #[test]
    fn turning_repeat_off_stops_the_song_coming_back() {
        let queue = Queue::default();
        assert!(queue.push(song("only")));
        queue.advance();
        assert!(queue.toggle_repeat());
        assert!(!queue.toggle_repeat(), "the second press turns it off");

        let now = queue.advance();
        assert!(now.current.is_none(), "nothing left to play: {now:?}");
        assert_eq!(queue.pending(), 0);
    }

    /// Stopping clears the queue. A repeat flag left on would otherwise put a
    /// stopped song straight back.
    #[test]
    fn a_cleared_queue_does_not_resurrect_the_song_under_repeat() {
        let queue = Queue::default();
        assert!(queue.push(song("only")));
        queue.advance();
        assert!(queue.toggle_repeat());

        queue.clear();
        let now = queue.advance();
        assert!(now.current.is_none(), "stop must beat repeat: {now:?}");
        assert_eq!(queue.pending(), 0);
    }
}
