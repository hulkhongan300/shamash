use super::*;

fn song(title: &str) -> Queued {
    Queued {
        source: Source::File(PathBuf::from(format!("/tmp/{title}.m4a"))),
        title: title.to_string(),
        artist: "someone".to_string(),
        url: format!("https://youtu.be/{title}"),
        video_id: title.to_string(),
        duration_secs: None,
        origin: Origin::Voice,
        remembered: None,
    }
}

/// A song still streaming: its cache file must count as on disk.
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
        duration_secs: None,
        origin: Origin::Voice,
        remembered: None,
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
    // Songbird reports a skipped song as ended; that must not drop a second song.
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
    assert_eq!(queue.in_use().len(), 2);

    queue.clear();
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

/// Pruning a stream's cache file would make a repeat download it again.
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
    let queue = Queue::default();
    let clone = queue.clone();
    let _ = std::thread::spawn(move || {
        let _guard = clone.0.lock().unwrap();
        panic!("poison the lock");
    })
    .join();

    assert!(queue.push(song("after the panic")));
}
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

/// Repeat one, not repeat-all: the song only comes back once the list runs down.
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

    let now = queue.advance();
    assert_eq!(now.current.expect("repeats last").title, "second");
}

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

fn titles(queue: &Queue) -> Vec<String> {
    queue.pending_songs().into_iter().map(|s| s.title).collect()
}

fn id_of(queue: &Queue, title: &str) -> u64 {
    queue
        .waiting()
        .into_iter()
        .find(|waiting| waiting.song.title == title)
        .expect("the song is waiting")
        .id
}

#[test]
fn every_waiting_song_gets_its_own_id() {
    let queue = Queue::default();
    queue.push(song("a"));
    queue.push(song("a"));
    queue.push(song("b"));
    let ids: Vec<u64> = queue.waiting().iter().map(|w| w.id).collect();
    let mut unique = ids.clone();
    unique.dedup();
    assert_eq!(ids.len(), 3);
    assert_eq!(unique, ids, "the same song queued twice is two entries");
}

#[test]
fn removing_takes_out_only_the_song_picked() {
    let queue = Queue::default();
    queue.push(song("first"));
    queue.push(song("mistake"));
    queue.push(song("last"));

    let removed = queue.remove(id_of(&queue, "mistake"));
    assert_eq!(removed.expect("removed").title, "mistake");
    assert_eq!(titles(&queue), ["first", "last"]);
}

/// A stale id from an open view must not hit whatever slid into its place.
#[test]
fn removing_a_song_that_already_started_changes_nothing() {
    let queue = Queue::default();
    queue.push(song("started"));
    queue.push(song("next"));
    let picked = id_of(&queue, "started");

    queue.advance();
    assert!(queue.remove(picked).is_none());
    assert_eq!(titles(&queue), ["next"], "the song behind it is untouched");
    assert_eq!(queue.current().expect("playing").title, "started");
    assert!(queue.shift(picked, Shift::Front).is_none());
    assert_eq!(titles(&queue), ["next"]);
}

#[test]
fn removing_twice_removes_once() {
    let queue = Queue::default();
    queue.push(song("gone"));
    queue.push(song("stays"));
    let picked = id_of(&queue, "gone");
    assert!(queue.remove(picked).is_some());
    assert!(queue.remove(picked).is_none());
    assert_eq!(titles(&queue), ["stays"]);
}

#[test]
fn an_id_is_not_reused_for_a_later_song() {
    let queue = Queue::default();
    queue.push(song("again"));
    let first = id_of(&queue, "again");
    queue.remove(first);
    queue.push(song("again"));
    assert!(queue.remove(first).is_none());
    assert_eq!(titles(&queue), ["again"]);
}

#[test]
fn a_song_moves_up_down_and_to_the_front() {
    let queue = Queue::default();
    for title in ["a", "b", "c", "d"] {
        queue.push(song(title));
    }
    let c = id_of(&queue, "c");

    assert_eq!(queue.shift(c, Shift::Up), Some(1));
    assert_eq!(titles(&queue), ["a", "c", "b", "d"]);

    assert_eq!(queue.shift(c, Shift::Down), Some(2));
    assert_eq!(titles(&queue), ["a", "b", "c", "d"]);

    let d = id_of(&queue, "d");
    assert_eq!(queue.shift(d, Shift::Front), Some(0));
    assert_eq!(titles(&queue), ["d", "a", "b", "c"]);
    assert_eq!(queue.advance().current.expect("next").title, "d");
}

#[test]
fn a_song_at_either_end_stays_put() {
    let queue = Queue::default();
    queue.push(song("first"));
    queue.push(song("last"));
    assert_eq!(queue.shift(id_of(&queue, "first"), Shift::Up), Some(0));
    assert_eq!(queue.shift(id_of(&queue, "last"), Shift::Down), Some(1));
    assert_eq!(queue.shift(id_of(&queue, "first"), Shift::Front), Some(0));
    assert_eq!(titles(&queue), ["first", "last"]);
}

#[test]
fn a_moved_song_keeps_its_id() {
    let queue = Queue::default();
    for title in ["a", "b", "c"] {
        queue.push(song(title));
    }
    let c = id_of(&queue, "c");
    queue.shift(c, Shift::Up);
    queue.shift(c, Shift::Up);
    assert_eq!(titles(&queue), ["c", "a", "b"]);
    assert_eq!(id_of(&queue, "c"), c);
}

/// A song that cannot start must leave "current", or the dashboard shows it
/// playing in silence and every later request queues behind it forever.
#[test]
fn a_song_that_fails_to_start_is_skipped_and_not_repeated() {
    let queue = Queue::default();
    assert!(queue.toggle_repeat(), "even with repeat on");
    assert!(queue.push(song("dead")));
    let dead = queue.advance();

    let after = queue.skip_failed(dead.generation).expect("skip");
    assert!(after.current.is_none(), "the dead song does not come back");
    assert!(queue.is_idle(), "so the next request starts at once");
    assert!(queue.push(song("next")));
    assert_eq!(queue.advance().current.expect("next").title, "next");
    assert!(
        queue.skip_failed(after.generation).is_none(),
        "a stale skip does not drop the song now playing"
    );
}
