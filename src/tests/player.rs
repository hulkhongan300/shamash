use super::*;

/// Opening the queue is a choice, and a song ending underneath it must not
/// close that choice: the redraw keeps whichever page is showing.
#[test]
fn a_redraw_keeps_the_page_the_dashboard_is_on() {
    let guild = GuildId::new(11);
    let dashboards = Dashboards::default();

    assert_eq!(dashboards.page(guild), Page::NowPlaying);

    let queue = Page::Queue { selected: Some(3) };
    dashboards.set(guild, ChannelId::new(1), MessageId::new(2), queue);
    assert_eq!(dashboards.page(guild), queue, "and the song picked in it");

    // Forgetting the dashboard is what happens when a message is deleted;
    // the next post starts on now-playing rather than inheriting a page
    // whose message no longer exists.
    dashboards.forget(guild);
    assert_eq!(dashboards.page(guild), Page::NowPlaying);
}

/// The bar only needs redrawing while it moves: not while paused, not
/// behind the queue view, and not when there is no bar to move.
#[test]
fn only_a_moving_progress_bar_is_refreshed() {
    assert!(worth_refreshing(Page::NowPlaying, true, false));
    assert!(!worth_refreshing(Page::NowPlaying, true, true), "paused");
    assert!(
        !worth_refreshing(Page::Queue { selected: None }, true, false),
        "queue view"
    );
    assert!(
        !worth_refreshing(Page::NowPlaying, false, false),
        "nothing playing, or a song of unknown length"
    );
}

/// A redraw replaces the guild's timer rather than adding a second one, so
/// the edits never come faster than one per interval.
#[tokio::test]
async fn a_new_refresh_timer_stops_the_old_one() {
    let refreshes = Refreshes::default();
    let (one, two) = (GuildId::new(1), GuildId::new(2));
    let forever = || tokio::spawn(std::future::pending::<()>());

    let first = forever();
    refreshes.set(one, first.abort_handle());
    let other = forever();
    refreshes.set(two, other.abort_handle());
    let second = forever();
    refreshes.set(one, second.abort_handle());
    assert!(first.await.unwrap_err().is_cancelled());
    assert!(!other.is_finished(), "another guild's timer is left alone");

    refreshes.stop(one);
    assert!(second.await.unwrap_err().is_cancelled());
    refreshes.stop(one);
    refreshes.stop(two);
    assert!(other.await.unwrap_err().is_cancelled());
}

/// The bug this commit exists for: `play` held the playback lock across the
/// catalog lookup, the YouTube search *and the whole download*, and `skip()`
/// and `stop()` take the same lock. A slow download therefore froze every
/// playback control for that guild.
///
/// `Songbird` cannot be constructed without a voice connection, so the
/// coordination is exercised on its own. That is not a workaround: the
/// whole bug is about which lock is held when, and none of it needs a
/// connection to observe.
#[tokio::test]
async fn a_request_in_flight_does_not_hold_the_playback_lock() {
    let guild = GuildId::new(7);
    let coordination = Coordination::default();
    let queue = Queue::default();

    // What `resolve_and_queue` holds across the search and the download.
    let (_serialised, _started_at) = coordination.begin(guild).await;

    // What a Skip press needs, while that request is still in flight.
    let lock = coordination.busy(guild);
    let guard = tokio::time::timeout(Duration::from_millis(500), lock.lock_owned())
        .await
        .expect("a request in flight must not hold the playback lock");
    drop(guard);
    drop(_serialised);

    assert_eq!(queue.pending(), 0);
}

/// The same, from the fetcher's side: a download that takes the playback
/// lock would block if the download were running inside it.
#[tokio::test]
async fn a_download_can_take_the_playback_lock_itself() {
    let guild = GuildId::new(7);
    let coordination = Coordination::default();
    let probe = LockProbe {
        coordination: coordination.clone(),
    };
    probe
        .fetch(
            guild,
            &candidate("https://youtu.be/x"),
            &[],
            &Timing::default(),
        )
        .await
        .unwrap();
}

/// Stop really cancels: a request that was still downloading when Stop was
/// pressed must not queue its song afterwards.
#[tokio::test]
async fn stop_cancels_a_request_that_is_still_downloading() {
    let guild = GuildId::new(7);
    let coordination = Coordination::default();
    let queue = Queue::default();

    let (_serialised, started_at) = coordination.begin(guild).await;
    // What `stop` does first, before it takes the playback lock.
    coordination.cancel(guild);

    let outcome = coordination
        .commit(
            guild,
            &queue,
            queued("Dracula"),
            started_at,
            &Timing::default(),
        )
        .await;
    assert_eq!(
        outcome,
        Commit::Cancelled,
        "a song queued after a stop is a song nobody asked for"
    );
    assert!(queue.is_idle(), "the cancelled song must not be queued");
}

/// The other half: a request that was not cancelled still queues, and starts
/// playing because nothing else is.
#[tokio::test]
async fn an_uncancelled_request_still_queues_and_starts() {
    let guild = GuildId::new(7);
    let coordination = Coordination::default();
    let queue = Queue::default();
    let (_serialised, started_at) = coordination.begin(guild).await;

    assert_eq!(
        coordination
            .commit(
                guild,
                &queue,
                queued("Dracula"),
                started_at,
                &Timing::default()
            )
            .await,
        Commit::Played { starting: true }
    );
    assert_eq!(queue.pending(), 1);
}

/// A song queued behind another does not start playing; it waits its turn.
#[tokio::test]
async fn a_second_song_waits_rather_than_starting() {
    let guild = GuildId::new(7);
    let coordination = Coordination::default();
    let queue = Queue::default();
    let (_serialised, started_at) = coordination.begin(guild).await;

    coordination
        .commit(
            guild,
            &queue,
            queued("First"),
            started_at,
            &Timing::default(),
        )
        .await;
    assert_eq!(
        coordination
            .commit(
                guild,
                &queue,
                queued("Second"),
                started_at,
                &Timing::default()
            )
            .await,
        Commit::Played { starting: false }
    );
    assert_eq!(queue.pending(), 2);
}

/// Cancellation is per request, not per guild: a request that began after a
/// Stop is unaffected by it.
#[tokio::test]
async fn a_request_started_after_a_stop_is_not_cancelled_by_it() {
    let guild = GuildId::new(7);
    let coordination = Coordination::default();
    let queue = Queue::default();
    coordination.cancel(guild);

    let (_serialised, started_at) = coordination.begin(guild).await;
    assert_eq!(
        coordination
            .commit(
                guild,
                &queue,
                queued("Dracula"),
                started_at,
                &Timing::default()
            )
            .await,
        Commit::Played { starting: true }
    );
}

/// Two requests for one guild do not overlap: the second waits its turn
/// rather than both downloading and both starting a track.
#[tokio::test]
async fn two_requests_for_one_guild_are_served_in_order() {
    let guild = GuildId::new(7);
    let coordination = Coordination::default();
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));

    let first = tokio::spawn({
        let coordination = coordination.clone();
        let order = Arc::clone(&order);
        async move {
            let (_guard, _at) = coordination.begin(guild).await;
            order.lock().unwrap().push("first");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    });
    // Wait until the first request is definitely holding the lock.
    while order.lock().unwrap().is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let second = tokio::spawn({
        let coordination = coordination.clone();
        let order = Arc::clone(&order);
        async move {
            let (_guard, _at) = coordination.begin(guild).await;
            order.lock().unwrap().push("second");
        }
    });

    first.await.unwrap();
    second.await.unwrap();
    assert_eq!(*order.lock().unwrap(), ["first", "second"]);
}

/// Requests for different guilds never wait on each other.
#[tokio::test]
async fn requests_for_different_guilds_do_not_wait() {
    let coordination = Coordination::default();
    let (_one, _at) = coordination.begin(GuildId::new(1)).await;
    let other = coordination.busy(GuildId::new(2));
    tokio::time::timeout(Duration::from_millis(500), other.lock_owned())
        .await
        .expect("another guild's request must not block this one");
}

/// A full queue is reported rather than silently dropping the song.
#[tokio::test]
async fn a_queue_that_filled_up_while_downloading_is_reported() {
    let guild = GuildId::new(7);
    let coordination = Coordination::default();
    let queue = Queue::default();
    let (_serialised, started_at) = coordination.begin(guild).await;
    for i in 0..crate::queue::MAX_QUEUE {
        assert!(queue.push(queued(&format!("filler{i}"))));
    }

    assert_eq!(
        coordination
            .commit(
                guild,
                &queue,
                queued("Dracula"),
                started_at,
                &Timing::default()
            )
            .await,
        Commit::QueueFull
    );
}

/// Records whether the playback lock was free while it ran.
struct LockProbe {
    coordination: Coordination,
}

#[async_trait]
impl AudioFetcher for LockProbe {
    async fn fetch(
        &self,
        guild_id: GuildId,
        found: &search::Candidate,
        _in_use: &[std::path::PathBuf],
        _timing: &Timing,
    ) -> anyhow::Result<Queued> {
        let lock = self.coordination.busy(guild_id);
        let guard = tokio::time::timeout(Duration::from_millis(500), lock.lock_owned())
            .await
            .expect("the playback lock was held while the audio downloaded");
        drop(guard);
        Ok(queued(found.title()))
    }
}

fn upload(title: Option<&str>, channel: Option<&str>, id: &str) -> search::Candidate {
    search::Candidate {
        title: title.map(str::to_string),
        url: format!("https://www.youtube.com/watch?v={id}"),
        view_count: None,
        duration: Some(130.0),
        channel: channel.map(str::to_string),
        id: Some(id.to_string()),
    }
}

/// A picked catalog song is remembered under the catalog's names, with the
/// video that was actually played.
#[test]
fn a_picked_song_is_remembered_with_the_played_video() {
    let song = resolve::CatalogTrack {
        artist: "Madvillain".to_string(),
        title: "All Caps".to_string(),
        duration_secs: Some(130.0),
        popularity: None,
        video_id: Some("catalogsown".to_string()),
    };
    let track = picked_track(
        Some(&song),
        &upload(Some("ALL CAPS"), Some("Stones Throw"), "playedvideo"),
    )
    .expect("remembered");
    assert_eq!(track.label(), "Madvillain — All Caps");
    assert_eq!(track.video_id.as_deref(), Some("playedvideo"));
}

/// A picked upload with no catalog song behind it is remembered as the
/// upload; one without a title has nothing to remember it by.
#[test]
fn a_picked_upload_is_remembered_as_the_upload() {
    let track = picked_track(
        None,
        &upload(
            Some("Madvillain - All Caps"),
            Some("Stones Throw"),
            "playedvideo",
        ),
    )
    .expect("remembered");
    assert_eq!(track.artist, "Stones Throw");
    assert_eq!(track.title, "Madvillain - All Caps");
    assert_eq!(track.video_id.as_deref(), Some("playedvideo"));
    assert_eq!(picked_track(None, &upload(None, None, "playedvideo")), None);
}

/// A typed search is remembered under the same words a spoken one is, so
/// one taught by typing is found when spoken, and the other way round.
#[test]
fn a_typed_search_is_remembered_like_the_same_words_spoken() {
    let parser = crate::parser::CommandParser::new(["bot".to_string()]);
    for (typed, spoken) in [
        ("Don't Stop Me Now", "Bot, play don't stop me now."),
        ("All Caps by MF DOOM", "bot play all caps by mf doom"),
        ("  AC/DC — Thunderstruck!  ", "bot play acdc thunderstruck"),
    ] {
        let heard = parser.parse(spoken).expect("a request").query;
        assert_eq!(remembered_as(typed), Some(heard), "for {typed:?}");
    }
    assert_eq!(remembered_as("!!!"), None);
}

/// Links are never remembered, YouTube or otherwise.
#[test]
fn a_link_is_never_remembered() {
    for link in [
        "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
        "https://youtu.be/dQw4w9WgXcQ",
        "https://music.youtube.com/playlist?list=OLAK5uy_abcdefghijk",
        "www.example.com/song",
        "https://soundcloud.com/artist/song",
    ] {
        assert_eq!(remembered_as(link), None, "for {link:?}");
    }
}

fn catalog(artist: &str, title: &str, video_id: Option<&str>) -> resolve::CatalogTrack {
    resolve::CatalogTrack {
        artist: artist.to_string(),
        title: title.to_string(),
        duration_secs: Some(130.0),
        popularity: None,
        video_id: video_id.map(str::to_string),
    }
}

/// A remembered song already in the results moves to the top, once, and
/// keeps the video that was picked last time.
#[test]
fn a_recalled_song_found_again_moves_to_the_front() {
    let mut tracks = vec![
        catalog("MF DOOM", "All Caps (Remix)", None),
        catalog("Madvillain", "All Caps", Some("catalogsown")),
    ];
    put_first(
        &mut tracks,
        catalog("madvillain", "ALL CAPS", Some("playedvideo")),
    );
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].artist, "Madvillain");
    assert_eq!(tracks[0].video_id.as_deref(), Some("playedvideo"));
    assert_eq!(tracks[1].title, "All Caps (Remix)");
}

/// A remembered song the catalog did not find this time is offered anyway.
#[test]
fn a_recalled_song_missing_from_the_results_is_added_first() {
    let mut tracks = vec![catalog("MF DOOM", "Rhymes Like Dimes", None)];
    put_first(
        &mut tracks,
        catalog("Stones Throw", "Madvillain - All Caps", Some("playedvideo")),
    );
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].title, "Madvillain - All Caps");

    let mut empty = Vec::new();
    put_first(&mut empty, catalog("Madvillain", "All Caps", None));
    assert_eq!(empty.len(), 1);
}

/// Asking again after a wrong song leaves that song out, matched by name
/// rather than by exact spelling.
#[test]
fn asking_again_leaves_out_the_wrong_song() {
    let wrong = crate::library::Track {
        artist: "MF DOOM".to_string(),
        title: "All Caps".to_string(),
        duration_secs: Some(130.0),
        video_id: Some("wrongvideo".to_string()),
    };
    let offered = without_wrong(
        vec![
            catalog("MF Doom", "All Caps!", None),
            catalog("Madvillain", "All Caps", None),
        ],
        Some(&wrong),
    );
    assert_eq!(offered, vec![catalog("Madvillain", "All Caps", None)]);
}

/// The wrong song is still offered when the catalog has nothing else, and
/// nothing is left out when there is no remembered song to leave out.
#[test]
fn asking_again_never_leaves_an_empty_question() {
    let wrong = crate::library::Track {
        artist: "MF DOOM".to_string(),
        title: "All Caps".to_string(),
        duration_secs: None,
        video_id: None,
    };
    let only = vec![catalog("MF DOOM", "All Caps", None)];
    assert_eq!(without_wrong(only.clone(), Some(&wrong)), only);
    let both = vec![
        catalog("MF DOOM", "All Caps", None),
        catalog("Madvillain", "All Caps", None),
    ];
    assert_eq!(without_wrong(both.clone(), None), both);
}

/// A song already in hand, as `commit` receives one.
fn queued(title: &str) -> Queued {
    Queued {
        source: Source::File(std::path::PathBuf::from(format!("/tmp/{title}.m4a"))),
        title: title.to_string(),
        artist: "someone".to_string(),
        url: "https://youtu.be/only".to_string(),
        video_id: "only".to_string(),
        duration_secs: None,
        origin: crate::queue::Origin::Voice,
        remembered: None,
    }
}

/// Reads a little-endian `u32` at `offset`.
fn u32_at(wav: &[u8], offset: usize) -> u32 {
    wav[offset..offset + 4]
        .try_into()
        .map(u32::from_le_bytes)
        .unwrap()
}

/// Reads a little-endian `u16` at `offset`.
fn u16_at(wav: &[u8], offset: usize) -> u16 {
    wav[offset..offset + 2]
        .try_into()
        .map(u16::from_le_bytes)
        .unwrap()
}

/// The tone has to survive the same probe songbird runs on it.
///
/// Songbird depends on symphonia with `default-features = false` and exposes
/// no feature to switch the format handlers back on, so its probe registry
/// is empty unless symphonia is also a direct dependency of this crate. That
/// build decodes nothing at all: every input fails with "no suitable format
/// reader found", so the tone is silent and the music never starts.
#[test]
fn songbird_can_probe_and_decode_the_tone() {
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::default::{get_codecs, get_probe};

    let wav = beep_wav(Tone::Wake.frequency());
    let source = MediaSourceStream::new(Box::new(std::io::Cursor::new(wav)), Default::default());
    let mut probed = get_probe()
        .format(
            &symphonia::core::probe::Hint::new(),
            source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .expect("songbird's probe must recognise the tone");
    let track = probed
        .format
        .default_track()
        .expect("the tone must expose a track");
    let mut decoder = get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .expect("the pcm decoder must be registered");

    let mut packets = 0;
    while let Ok(packet) = probed.format.next_packet() {
        decoder.decode(&packet).expect("pcm packet must decode");
        packets += 1;
    }
    assert!(packets > 0, "the tone must decode to audio samples");
}

#[test]
fn beep_is_a_playable_wav_header() {
    let wav = beep_wav(Tone::Wake.frequency());
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    assert_eq!(&wav[12..16], b"fmt ");
    assert_eq!(u32_at(&wav, 16), 16, "PCM fmt chunk size");
    assert_eq!(u16_at(&wav, 20), 1, "uncompressed PCM");
    assert_eq!(u16_at(&wav, 22), BEEP_CHANNELS);
    assert_eq!(u32_at(&wav, 24), BEEP_SAMPLE_RATE);
    assert_eq!(u16_at(&wav, 34), 16, "bits per sample");
    assert_eq!(&wav[36..40], b"data");
    let data_len = u32_at(&wav, 40) as usize;
    assert_eq!(wav.len(), 44 + data_len, "declared data length");
    assert_eq!(u32_at(&wav, 4) as usize, 36 + data_len, "RIFF length");
}

#[test]
fn beep_holds_audible_tone_for_the_requested_duration() {
    let wav = beep_wav(Tone::Wake.frequency());
    let samples: Vec<i16> = wav[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c))
        .collect();
    let frames = samples.len() / BEEP_CHANNELS as usize;
    let expected = (BEEP_SAMPLE_RATE as f32 * BEEP_DURATION.as_secs_f32()) as usize;
    assert!(
        (frames as i32 - expected as i32).abs() <= 1,
        "{frames} frames"
    );

    let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
    assert!(peak > i16::MAX as u16 / 5, "tone is too quiet: peak {peak}");

    let left = samples.iter().step_by(2);
    let right = samples.iter().skip(1).step_by(2);
    assert!(
        left.eq(right),
        "both channels carry the same tone, so it plays centred"
    );
}

fn candidate(url: &str) -> search::Candidate {
    search::Candidate {
        title: Some("Dracula".to_string()),
        url: url.to_string(),
        view_count: Some(1),
        duration: Some(206.0),
        channel: Some("Tame Impala".to_string()),
        id: None,
    }
}

#[test]
fn beep_fades_in_and_out_instead_of_clicking() {
    let wav = beep_wav(Tone::Wake.frequency());
    let samples: Vec<i16> = wav[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c))
        .collect();
    let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
    let first = samples[0].unsigned_abs();
    let last = samples[samples.len() - 2].unsigned_abs();
    assert!(first < peak / 4, "starts at {first}, peak {peak}");
    assert!(last < peak / 4, "ends at {last}, peak {peak}");
}

/// The frequency a tone actually contains, counted from its zero crossings.
///
/// A sine crosses zero twice per cycle, so the crossing rate over the
/// measured span is twice its frequency. Two details make the count honest:
/// the span is the middle 80%, because the fades at each end are what stops
/// the tone clicking; and exact zeros are dropped first, because truncating
/// the tone into i16 rounds the samples either side of a crossing to zero,
/// and a zero next to a positive and then next to a negative reads as two
/// crossings where the signal only makes one.
fn frequency_of(wav: &[u8]) -> f32 {
    let samples: Vec<i16> = wav[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c))
        .collect();
    let middle = &samples[samples.len() / 10..samples.len() * 9 / 10];
    let measured: Vec<i16> = middle.iter().copied().filter(|s| *s != 0).collect();
    let crossings = measured
        .windows(2)
        .filter(|w| w[0].signum() != w[1].signum())
        .count();
    crossings as f32 / 2.0 / (BEEP_DURATION.as_secs_f32() * 0.8)
}

/// The second tone has to be told apart from the first by ear alone. If the
/// two were the same, or too close to separate, a speaker who starts their
/// request too early would have no way to tell which stage answered them.
#[test]
fn the_two_acknowledgement_tones_are_distinguishable() {
    let wake = frequency_of(&beep_wav(Tone::Wake.frequency()));
    let ready = frequency_of(&beep_wav(Tone::Ready.frequency()));

    assert!(
        (wake - Tone::Wake.frequency()).abs() < 20.0,
        "wake tone measured {wake} Hz"
    );
    assert!(
        (ready - Tone::Ready.frequency()).abs() < 20.0,
        "ready tone measured {ready} Hz"
    );
    assert!(
        ready - wake > 60.0,
        "{wake} Hz and {ready} Hz are too close to tell apart"
    );
}

/// The miss tone has to be tellable from both acknowledgements, in either
/// order. This is the whole point of adding it: a person who spoke and heard
/// nothing has no way to tell the bot apart from one that did not hear them,
/// so "I could not find that song" has to arrive as a sound and not only as
/// a message in a text channel they may not be reading.
#[test]
fn the_miss_tone_is_tellable_from_both_acknowledgements() {
    let miss = frequency_of(&beep_wav(Tone::Miss.frequency()));
    assert!(
        (miss - Tone::Miss.frequency()).abs() < 20.0,
        "miss tone measured {miss} Hz"
    );
    for other in [Tone::Wake, Tone::Ready] {
        let their_frequency = other.frequency();
        let gap = (miss - their_frequency).abs();
        assert!(
            gap > 60.0,
            "{miss} Hz and {their_frequency} Hz are too close to tell apart"
        );
    }
    // And it is the odd one out, so it reads as "not an acknowledgement"
    // rather than as a third step in the same sequence.
    assert!(miss < Tone::Wake.frequency());
}

/// Every tone has to survive the same probe songbird runs on it, or a
/// message that never arrives is worse than no message at all.
#[test]
fn every_tone_decodes() {
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;
    use symphonia::default::{get_codecs, get_probe};

    for tone in [Tone::Wake, Tone::Ready, Tone::Miss] {
        let wav = beep_wav(tone.frequency());
        let source =
            MediaSourceStream::new(Box::new(std::io::Cursor::new(wav)), Default::default());
        let mut probed = get_probe()
            .format(
                &Hint::new(),
                source,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .expect("the probe must recognise the tone");
        let track = probed.format.default_track().expect("a track");
        let _decoder = get_codecs()
            .make(&track.codec_params, &Default::default())
            .expect("the pcm decoder must be registered");
        assert!(probed.format.next_packet().is_ok(), "{tone:?} must decode");
    }
}

/// The listener has to be told what was heard: the bot's transcription is
/// frequently wrong, and echoing it back is what lets them say the right
/// thing next time.
#[test]
fn every_failure_says_what_was_heard_and_what_to_do() {
    for (problem, expected) in [
        (
            Problem::NoMatch,
            "I heard “dracula by tame impala”, but couldn’t match it to a song. \
                 Try “play <title> by <artist>”.",
        ),
        (
            Problem::FetchFailed,
            "I found “dracula by tame impala” but couldn’t download it. \
                 Try again in a moment.",
        ),
        (
            Problem::QueueFull,
            "The queue is full, so “dracula by tame impala” wasn’t added. \
                 Skip or stop something first, then ask again.",
        ),
    ] {
        let failure = Failure::new(problem, anyhow::anyhow!("because"));
        assert_eq!(failure.message("dracula by tame impala"), expected);
    }
}

/// The three have to stay distinguishable as prose too, since a person
/// reading them should not have to guess which went wrong.
#[test]
fn the_three_failures_read_differently() {
    let heard = "dracula by tame impala";
    let said = |problem| Failure::new(problem, anyhow::anyhow!("because")).message(heard);
    assert_ne!(said(Problem::NoMatch), said(Problem::FetchFailed));
    assert_ne!(said(Problem::NoMatch), said(Problem::QueueFull));
    assert_ne!(said(Problem::FetchFailed), said(Problem::QueueFull));
}

/// The tone the second stage plays has to decode like the first, or a model
/// that is ready would be announced in silence.
#[test]
fn the_ready_tone_decodes_too() {
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::default::{get_codecs, get_probe};

    let wav = beep_wav(Tone::Ready.frequency());
    let source = MediaSourceStream::new(Box::new(std::io::Cursor::new(wav)), Default::default());
    let mut probed = get_probe()
        .format(
            &symphonia::core::probe::Hint::new(),
            source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .expect("songbird's probe must recognise the ready tone");
    let track = probed.format.default_track().expect("a track");
    let mut decoder = get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .expect("the pcm decoder must be registered");

    let mut packets = 0;
    while let Ok(packet) = probed.format.next_packet() {
        decoder.decode(&packet).expect("pcm packet must decode");
        packets += 1;
    }
    assert!(packets > 0, "the ready tone must decode to audio samples");
}
fn song_offer(artist: &str, title: &str) -> Offer {
    Offer::Song {
        track: resolve::CatalogTrack {
            artist: artist.to_string(),
            title: title.to_string(),
            duration_secs: Some(206.0),
            popularity: None,
            video_id: None,
        },
        requested: "dracula".to_string(),
    }
}

fn ask_with(count: usize, owner: Owner) -> Ask {
    Ask {
        query: "dracula".to_string(),
        said: Some("dracula".to_string()),
        offers: (0..count)
            .map(|i| song_offer("Tame Impala", &format!("Song {i}")))
            .collect(),
        page: 0,
        owner,
        posted: None,
    }
}

fn titled(offer: &Offer) -> &str {
    match offer {
        Offer::Video(candidate) => candidate.title(),
        Offer::Song { track, .. } => &track.title,
        Offer::Album(album) => &album.title,
    }
}

/// The pages are windows onto one list, so the first song of page two is
/// the tenth song and not the first one again.
#[test]
fn a_page_is_a_window_onto_the_whole_list() {
    let ask = ask_with(20, Owner::User(UserId::new(7)));
    let first = ask.page();
    assert_eq!(first.len(), dashboard::ASK_PAGE);
    assert_eq!(titled(&first[0]), "Song 0");

    let mut second = ask.clone();
    second.page = 1;
    let window = second.page();
    // Songs 9 through 17: a full page, with songs 18 and 19 behind it.
    assert_eq!(window.len(), dashboard::ASK_PAGE, "page two is also full");
    assert_eq!(titled(&window[0]), "Song 9", "not the first song again");
}

/// A list shorter than a page must not run off the end of itself.
#[test]
fn a_short_list_pages_without_overrunning() {
    let ask = ask_with(3, Owner::Voice);
    assert_eq!(ask.page().len(), 3);

    let mut beyond = ask.clone();
    beyond.page = 9;
    assert!(beyond.page().is_empty());
}

/// The ownership rule on its own, so it can be tested without a player.
fn asks_answers(asks: &Asks, guild_id: GuildId, user: UserId) -> bool {
    match asks.lock().get(&guild_id).map(|ask| ask.owner) {
        Some(Owner::User(asker)) => asker == user,
        Some(Owner::Voice) => true,
        None => false,
    }
}

/// Next page has to say no on the last page rather than render an empty one.
#[test]
fn the_last_page_has_no_next_one() {
    for (count, page, expected) in [
        (20, 0, true),
        (20, 1, true),
        (20, 2, false), // 20 songs, 9 to a page: three pages, and this is the last.
        (9, 0, false),
        (1, 0, false),
    ] {
        let mut ask = ask_with(count, Owner::Voice);
        ask.page = page;
        assert_eq!(ask.has_next(), expected, "{count} songs, page {page}");
    }
}

/// A typed search belongs to whoever typed it, and a spoken one to the
/// channel. They are the same buttons and the same chooser, and getting the
/// owner wrong either lets anyone in the server queue music off someone
/// else's search, or locks the channel out of its own.
#[test]
fn ownership_differs_between_a_typed_and_a_spoken_question() {
    let guild = GuildId::new(3);
    let asks = Asks::default();
    let asker = UserId::new(42);
    let other = UserId::new(43);
    assert!(!asks_answers(&asks, guild, asker), "nothing to answer");

    asks.set(guild, ask_with(5, Owner::User(asker)));
    assert!(asks_answers(&asks, guild, asker));
    assert!(!asks_answers(&asks, guild, other), "someone else must not");

    // A spoken one has nobody to compare against: the bot cannot tell who
    // spoke it, so the channel's own check is the only one there is.
    asks.set(guild, ask_with(5, Owner::Voice));
    assert!(asks_answers(&asks, guild, other));
}

/// A question that expired on its timer must not clear a newer question
/// that has replaced it, and replacing hands back the old question so its
/// message can be cleared.
#[test]
fn an_expired_question_does_not_clear_a_newer_one() {
    let guild = GuildId::new(3);
    let asks = Asks::default();
    let mut old = ask_with(2, Owner::Voice);
    old.posted = Some((ChannelId::new(1), MessageId::new(10)));
    let mut newer = ask_with(4, Owner::Voice);
    newer.posted = Some((ChannelId::new(1), MessageId::new(11)));

    let previous = asks.replace(guild, old);
    assert!(previous.is_none());

    let replaced = asks.replace(guild, newer);
    assert_eq!(
        replaced.and_then(|a| a.posted),
        Some((ChannelId::new(1), MessageId::new(10)))
    );

    // The old timer fires; it must not clear the newer question.
    assert!(!asks.clear_if(guild, ChannelId::new(1), MessageId::new(10)));
    assert!(asks.lock().get(&guild).is_some());

    // The newer timer fires; it clears the question.
    assert!(asks.clear_if(guild, ChannelId::new(1), MessageId::new(11)));
    assert!(asks.lock().get(&guild).is_none());
}

#[test]
fn autocomplete_cache_stores_and_retrieves_tracks() {
    let cache = AutocompleteCache::default();
    let track = resolve::CatalogTrack {
        artist: "Tame Impala".to_string(),
        title: "Dracula".to_string(),
        duration_secs: Some(206.0),
        popularity: Some(100),
        video_id: Some("cuMuMnCRfqk".to_string()),
    };
    cache.insert("cuMuMnCRfqk".to_string(), track.clone());
    let retrieved = cache.get("cuMuMnCRfqk").expect("must retrieve track");
    assert_eq!(retrieved.artist, "Tame Impala");
    assert_eq!(retrieved.title, "Dracula");
    assert!(cache.get("nonexistent").is_none());
}

#[test]
fn the_music_stays_low_until_every_speaker_is_done() {
    let ducks = Ducks::default();
    let guild = GuildId::new(1);
    assert!(ducks.set(guild, 1, true));
    assert!(ducks.set(guild, 2, true));
    assert!(ducks.set(guild, 1, false), "speaker 2 is still talking");
    assert!(!ducks.set(guild, 2, false));
    assert!(!ducks.is_ducked(guild));
}
