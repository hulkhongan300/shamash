use super::*;
use symphonia::core::io::MediaSource;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

/// A one-shot HTTP server on a loopback port; hand-rolled so a test can make it never answer.
async fn serve(response: Option<Vec<u8>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!(
        "http://{}/videoplayback",
        listener.local_addr().expect("addr")
    );
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut request = [0u8; 1024];
        let _ = socket.read(&mut request).await;
        let Some(response) = response else {
            // Accept and say nothing: the stall.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            return;
        };
        let _ = socket.write_all(&response).await;
        let _ = socket.flush().await;
    });
    url
}

/// A minimal MP4 header, so the body is not empty.
const AUDIO: &[u8] = b"\x00\x00\x00\x18ftypM4A \x00\x00\x02\x00isom";

/// The response a server sends for a range request it is willing to serve.
fn serves(status: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

/// The happy path: the format URL answers, so the song streams.
#[tokio::test]
async fn a_stream_that_answers_is_the_one_that_plays() {
    let url = serve(Some(serves("206 Partial Content", AUDIO))).await;
    let timing = Timing::default();
    let cache = std::env::temp_dir().join("shamash-ok-test/abc.m4a");

    let resolved = resolve(
        &Source::Stream {
            url,
            headers: HeaderMap::new(),
            length: AUDIO.len() as u64,
            cache,
        },
        "https://youtu.be/abc",
        &[],
        &timing,
    )
    .await
    .expect("resolve");

    assert!(
        matches!(resolved, Source::Stream { .. }),
        "an answering URL must play as a stream, got {resolved:?}"
    );
}

/// An expired signed URL gets a 403; the song must still play from the file.
#[tokio::test]
async fn a_stream_the_server_refuses_falls_back_to_the_file() {
    let url = serve(Some(serves("403 Forbidden", b""))).await;
    let dir = std::env::temp_dir().join("shamash-403-test");
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await.expect("create dir");
    // On disk, so the fallback needs no yt-dlp.
    let cache = dir.join("abc.m4a");
    tokio::fs::write(&cache, AUDIO).await.expect("write");
    let timing = Timing::default();
    let stream = Streamable {
        url,
        headers: HeaderMap::new(),
        ext: "m4a".to_string(),
        length: AUDIO.len() as u64,
    };

    let resolved = resolve(
        &Source::Stream {
            url: stream.url.clone(),
            headers: stream.headers.clone(),
            length: AUDIO.len() as u64,
            cache: cache.clone(),
        },
        "https://youtu.be/abc",
        &[],
        &timing,
    )
    .await
    .expect("resolve");

    assert_eq!(resolved, Source::File(cache), "the file is what plays");
}

/// A body that stops arriving must error, so `save` can fall back to yt-dlp.
#[tokio::test(start_paused = true)]
async fn a_stream_download_that_stalls_gives_up() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!(
        "http://{}/videoplayback",
        listener.local_addr().expect("addr")
    );
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut request = [0u8; 1024];
        let _ = socket.read(&mut request).await;
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n")
            .await;
        std::future::pending::<()>().await;
    });
    let stream = Streamable {
        url,
        headers: HeaderMap::new(),
        ext: "m4a".to_string(),
        length: 1000,
    };
    let dest = std::env::temp_dir().join("shamash-body-stall-test.part");
    let result = tokio::time::timeout(DOWNLOAD_TIMEOUT * 2, fetch_to_file(&stream, &dest)).await;
    assert!(matches!(result, Ok(Err(_))), "{result:?}");
}

/// A silent connection must hit the budget and fall back, not hang.
#[tokio::test]
async fn a_stream_that_stalls_falls_back_to_the_file() {
    let url = serve(None).await;
    let dir = std::env::temp_dir().join("shamash-stall-test");
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await.expect("create dir");
    let cache = dir.join("abc.m4a");
    tokio::fs::write(&cache, AUDIO).await.expect("write");
    let timing = Timing::default();
    let stream = Streamable {
        url,
        headers: HeaderMap::new(),
        ext: "m4a".to_string(),
        length: AUDIO.len() as u64,
    };

    let started = std::time::Instant::now();
    assert!(
        !usable_within(&stream, Duration::from_millis(200), &timing).await,
        "a connection that never answers must not count as usable"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the budget did not apply: {:?}",
        started.elapsed()
    );

    let resolved = resolve(
        &Source::Stream {
            url: stream.url,
            headers: stream.headers,
            length: AUDIO.len() as u64,
            cache: cache.clone(),
        },
        "https://youtu.be/abc",
        &[],
        &timing,
    )
    .await
    .expect("resolve");
    assert_eq!(resolved, Source::File(cache), "the file is what plays");
}

/// A song that is already on disk never asks YouTube anything.
#[tokio::test]
async fn a_file_is_played_as_it_is() {
    let path = PathBuf::from("/tmp/already-here.m4a");
    let timing = Timing::default();
    assert_eq!(
        resolve(
            &Source::File(path.clone()),
            "https://youtu.be/x",
            &[],
            &timing
        )
        .await
        .expect("resolve"),
        Source::File(path)
    );
}

/// The streamed format must be decodable and keep its signed headers.
#[test]
fn the_streamed_format_is_decodable_and_carries_its_headers() {
    let json = serde_json::json!({
        "formats": [
            {"format_id": "251", "ext": "webm", "acodec": "opus", "vcodec": "none",
             "abr": 160.0, "filesize": 200, "url": "https://video.example/opus", "http_headers": {}},
            {"format_id": "140", "ext": "m4a", "acodec": "mp4a.40.2", "vcodec": "none",
             "abr": 128.0, "filesize": 4096, "url": "https://video.example/aac",
             "http_headers": {"User-Agent": "Mozilla/5.0", "Range": "bytes=0-99"}},
            {"format_id": "137", "ext": "mp4", "acodec": "none", "vcodec": "avc1",
             "url": "https://video.example/video", "http_headers": {}}
        ]
    });

    let picked = pick_streamable(&json).expect("a decodable audio format");
    assert_eq!(picked.url, "https://video.example/aac", "AAC in MP4");
    assert_eq!(picked.ext, "m4a");
    // Dropping these gets a 403.
    assert_eq!(picked.headers.get("user-agent").unwrap(), "Mozilla/5.0");
    assert_eq!(picked.headers.get("range").unwrap(), "bytes=0-99");
}

/// Nothing decodable means download and transcode, not stream.
#[test]
fn a_video_with_nothing_decodable_offers_no_stream() {
    let json = serde_json::json!({
        "formats": [
            {"ext": "webm", "acodec": "opus", "vcodec": "none", "filesize": 1, "url": "https://v/o"},
            {"ext": "mp4", "acodec": "none", "vcodec": "avc1", "filesize": 1, "url": "https://v/v"}
        ]
    });
    assert!(pick_streamable(&json).is_none());
}

/// Always a closed range: YouTube 403s a plain GET and `bytes=0-`.
#[test]
fn a_stream_is_asked_for_as_a_closed_range_over_the_whole_file() {
    assert_eq!(whole_stream(1), "bytes=0-0");
    assert_eq!(whole_stream(4_096), "bytes=0-4095");
    // Zero length must not produce a backwards range.
    assert_eq!(whole_stream(0), "bytes=0-0");
}

/// A format with no known length cannot close the range, so it is not offered.
#[test]
fn a_format_with_no_length_offers_no_stream() {
    let json = serde_json::json!({
        "formats": [
            {"ext": "m4a", "acodec": "mp4a.40.2", "vcodec": "none",
             "url": "https://v/aac", "http_headers": {}}
        ]
    });
    assert!(pick_streamable(&json).is_none());
}

#[test]
fn decodable_format_avoids_opus_and_prefers_aac() {
    // Symphonia cannot decode Opus.
    assert!(DECODABLE_FORMAT.contains("acodec!=opus"));
    assert!(!DECODABLE_FORMAT.contains("[acodec=opus]"));
    assert!(DECODABLE_FORMAT.starts_with("bestaudio[acodec^=mp4a]"));
    // Audio-only: a video stream would be silent.
    for branch in DECODABLE_FORMAT.split('/') {
        assert!(
            branch.contains("bestaudio"),
            "selector branch is not audio-only: {branch}"
        );
    }
}

#[test]
fn each_song_downloads_to_its_own_file() {
    // A shared per-guild path let a second request overwrite the playing track.
    let one = download_args(
        "https://youtu.be/abc",
        Path::new("/tmp/shamash-1"),
        "abc",
        DECODABLE_FORMAT,
    );
    let two = download_args(
        "https://youtu.be/xyz",
        Path::new("/tmp/shamash-1"),
        "xyz",
        DECODABLE_FORMAT,
    );
    let output_of = |args: &[OsString]| {
        args.iter()
            .find(|a| a.to_string_lossy().contains("%(ext)s"))
            .map(|a| a.to_string_lossy().into_owned())
            .expect("an output path")
    };
    assert_ne!(
        output_of(&one),
        output_of(&two),
        "two songs must not share an output path"
    );
    assert!(output_of(&one).ends_with("abc.%(ext)s"));
}

#[test]
fn each_download_forces_a_real_one() {
    // yt-dlp skips existing outputs by default, passing off a stale file as fresh.
    let args = download_args(
        "https://youtu.be/abc",
        Path::new("/tmp/shamash-1"),
        "abc",
        DECODABLE_FORMAT,
    );
    let args: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(
        args.iter().any(|a| a == "--force-overwrites"),
        "a stale file would be replayed: {args:?}"
    );
    // Keep `.part`: writing in place would truncate a playing track's file.
    assert!(
        !args.iter().any(|a| a == "--no-part"),
        "the output must be renamed into place, not written in place: {args:?}"
    );
    assert_eq!(args.last().unwrap(), "https://youtu.be/abc");
}

#[test]
fn each_guild_gets_its_own_directory() {
    assert_ne!(cache_dir(1), cache_dir(2));
    assert!(cache_dir(42).ends_with("shamash-42"));
}

#[test]
fn a_video_id_cannot_escape_the_cache_directory() {
    // The id is untrusted: it must not escape the cache.
    for id in ["../../etc/passwd", "a/b", "..", "with space"] {
        let stem = file_stem(id);
        assert!(
            !stem.contains('/') && !stem.contains('\\'),
            "id {id} produced a path: {stem}"
        );
        assert!(!stem.contains(".."), "id {id} produced {stem}");
    }
    assert_eq!(file_stem(""), "track");
    assert_eq!(file_stem("dQw4w9WgXcQ"), "dQw4w9WgXcQ");
}

#[tokio::test]
async fn audio_already_downloaded_is_reused() {
    // A cached song is not downloaded again.
    let dir = std::env::temp_dir().join("shamash-existing-test");
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await.expect("create dir");
    tokio::fs::write(dir.join("abc.m4a"), b"audio")
        .await
        .expect("write");

    assert_eq!(existing("abc", &dir).await, Some(dir.join("abc.m4a")));
    assert_eq!(existing("other", &dir).await, None);
}

#[tokio::test]
async fn a_half_downloaded_file_is_not_reused() {
    // A `.part` file is an unfinished download.
    let dir = std::env::temp_dir().join("shamash-part-test");
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await.expect("create dir");
    tokio::fs::write(dir.join("abc.m4a.part"), b"half")
        .await
        .expect("write");

    assert_eq!(existing("abc", &dir).await, None);
}

#[tokio::test]
async fn an_empty_file_is_not_reused() {
    let dir = std::env::temp_dir().join("shamash-empty-test");
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await.expect("create dir");
    tokio::fs::write(dir.join("abc.m4a"), b"")
        .await
        .expect("write");

    assert_eq!(existing("abc", &dir).await, None);
}

#[tokio::test]
async fn a_subprocess_that_never_finishes_is_killed_at_the_deadline() {
    // `Player::play` holds request ordering during a fetch, so a hang must become an error.
    let budget = Duration::from_millis(200);
    let mut command = Command::new("sh");
    command.kill_on_drop(true).arg("-c").arg("sleep 30");
    let started = std::time::Instant::now();
    let err = run(command, "a sleeping test process", budget)
        .await
        .expect_err("the sleep outlives the budget");
    assert!(
        err.to_string().contains("longer than"),
        "the error must say it timed out: {err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the deadline did not apply"
    );
}

/// A command inside its budget is not disturbed by the wrapper.
#[tokio::test]
async fn a_subprocess_that_finishes_in_time_is_not_disturbed() {
    let mut command = Command::new("sh");
    command.kill_on_drop(true).arg("-c").arg("exit 0");
    let output = run(command, "a test process", Duration::from_secs(30))
        .await
        .expect("a quick process must be allowed through");
    assert!(output.status.success());
}

/// Pins the documented subprocess deadlines.
#[test]
fn every_subprocess_has_a_bounded_budget() {
    assert_eq!(DOWNLOAD_TIMEOUT, Duration::from_secs(90));
    assert_eq!(TRANSCODE_TIMEOUT, Duration::from_secs(60));
}

#[tokio::test]
async fn pruning_keeps_the_newest_downloads() {
    let dir = std::env::temp_dir().join("shamash-prune-test");
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await.expect("create dir");
    for i in 0..(KEEP_FILES + 4) {
        tokio::fs::write(dir.join(format!("song{i}.m4a")), b"audio")
            .await
            .expect("write");
        // Make the order of the files unambiguous.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    prune(&dir, &[]).await;

    let left = names_in(&dir).await;
    assert!(
        left.contains(&"song11.m4a".to_string()),
        "newest kept: {left:?}"
    );
    // song0..song11 written, newest 8 kept.
    for pruned in ["song0.m4a", "song1.m4a", "song2.m4a", "song3.m4a"] {
        assert!(
            !left.contains(&pruned.to_string()),
            "{pruned} should have been pruned: {left:?}"
        );
    }
    assert_eq!(left.len(), KEEP_FILES, "the cache must stay bounded");
}

/// Nothing in use is pruned, however old: newest-eight once deleted queued songs.
#[tokio::test]
async fn pruning_never_deletes_a_queued_songs_audio() {
    let dir = std::env::temp_dir().join("shamash-prune-queued-test");
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await.expect("create dir");

    let queued: Vec<PathBuf> = (0..12)
        .map(|i| dir.join(format!("queued{i}.m4a")))
        .collect();
    for path in &queued {
        tokio::fs::write(path, b"audio").await.expect("write");
        // Deliberately old; they must survive anyway.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // The thirteenth triggers the prune.
    tokio::fs::write(dir.join("thirteenth.m4a"), b"audio")
        .await
        .expect("write");

    prune(&dir, &queued).await;

    let left = names_in(&dir).await;
    for path in &queued {
        assert!(
            left.contains(&path.file_name().unwrap().to_string_lossy().into_owned()),
            "{} was queued and must survive: {left:?}",
            path.display()
        );
    }
    assert_eq!(left.len(), 13, "nothing was in use to prune: {left:?}");
}

/// Once the songs leave the queue the cache shrinks again.
#[tokio::test]
async fn pruning_resumes_once_the_songs_are_no_longer_queued() {
    let dir = std::env::temp_dir().join("shamash-prune-after-test");
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await.expect("create dir");
    for i in 0..(KEEP_FILES + 4) {
        tokio::fs::write(dir.join(format!("song{i}.m4a")), b"audio")
            .await
            .expect("write");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    prune(&dir, &[]).await;
    assert_eq!(names_in(&dir).await.len(), KEEP_FILES);
}

/// The file names left in a directory, for readable assertions.
async fn names_in(dir: &Path) -> Vec<String> {
    let mut left: Vec<String> = Vec::new();
    let mut entries = tokio::fs::read_dir(dir).await.expect("read dir");
    while let Ok(Some(entry)) = entries.next_entry().await {
        left.push(entry.file_name().to_string_lossy().into_owned());
    }
    left.sort();
    left
}

/// Opus-only fallback transcodes to WAV. Needs ffmpeg (`--ignored`).
#[tokio::test]
#[ignore = "needs ffmpeg"]
async fn transcodes_opus_to_a_decodable_wav() {
    let dir = std::env::temp_dir().join("shamash-transcode-test");
    tokio::fs::create_dir_all(&dir).await.expect("create dir");

    // Stand in for the Opus download that triggers the fallback.
    let source = dir.join("src.webm");
    let made = Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-y", "-f", "lavfi"])
        .args(["-i", "sine=frequency=440:duration=2", "-c:a", "libopus"])
        .arg(&source)
        .output()
        .await
        .expect("run ffmpeg");
    assert!(made.status.success(), "could not build the test source");

    let dest = dir.join("track.wav");
    transcode(&source, &dest).await.expect("transcode");

    let header = tokio::fs::read(&dest).await.expect("read");
    assert_eq!(&header[0..4], b"RIFF", "the result must be a WAV file");
    assert_eq!(&header[8..12], b"WAVE");
    assert!(header.len() > 44, "the WAV must contain audio");
}

/// Two downloads keep their own audio. Needs network and yt-dlp (`--ignored`).
#[tokio::test]
#[ignore = "needs network and yt-dlp"]
async fn two_songs_keep_separate_audio() {
    let dir = std::env::temp_dir().join("shamash-two-songs-test");
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await.expect("create dir");

    let first = fetched_file("wJGcwEv7838", "https://youtu.be/wJGcwEv7838", &dir).await;
    let second = fetched_file("aqz-KE-bpKQ", "https://youtu.be/aqz-KE-bpKQ", &dir).await;

    assert_ne!(first, second, "each song needs its own file");
    assert_ne!(
        tokio::fs::metadata(&first).await.expect("stat").len(),
        tokio::fs::metadata(&second).await.expect("stat").len(),
        "the files must hold different audio"
    );
}

/// A song left on disk the way playback's background cache write leaves it.
async fn fetched_file(video_id: &str, url: &str, dir: &Path) -> PathBuf {
    let timing = Timing::default();
    let source = fetch(video_id, url, dir, &[], &timing)
        .await
        .expect("fetch");
    let source = resolve(&source, url, &[], &timing).await.expect("resolve");
    match source {
        Source::File(path) => path,
        source => {
            let stream = Streamable::of(&source).expect("that source was a stream");
            let Source::Stream { cache, .. } = &source else {
                unreachable!()
            };
            let cache = cache.clone();
            save(&stream, &cache, url, &[], &timing)
                .await
                .expect("cache the streamed audio");
            cache
        }
    }
}

/// Plays a real format URL unseekably, as playback does: a trailing `moov` atom
/// only fails against the real thing.
#[tokio::test]
#[ignore = "needs network; run serially, YouTube refuses concurrent extractions"]
async fn a_real_youtube_stream_demuxes_without_being_fully_buffered() {
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;
    use symphonia::default::{get_codecs, get_probe};

    for url in [
        "https://youtu.be/wJGcwEv7838",
        "https://youtu.be/aqz-KE-bpKQ",
    ] {
        let json = extract(url, &Timing::default())
            .await
            .expect("extract the video");
        let stream = pick_streamable(&json)
            .unwrap_or_else(|| panic!("{url} offers nothing symphonia can decode"));

        let bytes = read_range(&stream).await.expect("read the stream");
        let mut probed = get_probe()
            .format(
                &Hint::new(),
                // As songbird wraps it: no seek.
                MediaSourceStream::new(Box::new(Unseekable::new(bytes)), Default::default()),
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .unwrap_or_else(|e| panic!("{url} did not demux as a stream: {e:?}"));
        let track = probed
            .format
            .default_track()
            .unwrap_or_else(|| panic!("{url} exposed no track as a stream"));
        get_codecs()
            .make(&track.codec_params, &Default::default())
            .unwrap_or_else(|e| panic!("{url} has no decoder for its codec: {e:?}"));
        assert!(
            probed.format.next_packet().is_ok(),
            "{url} produced no audio packets"
        );
    }
}

/// The stream as playback asks for it: the whole file, over a range.
async fn read_range(stream: &Streamable) -> anyhow::Result<Vec<u8>> {
    let response = client()
        .get(&stream.url)
        .headers(stream.headers.clone())
        .header(RANGE, whole_stream(stream.length))
        .send()
        .await?;
    anyhow::ensure!(response.status().is_success(), "{}", response.status());
    let mut bytes = Vec::new();
    let mut response = response;
    while let Some(chunk) = response.chunk().await? {
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// A read-only media source, which is what an HTTP response is.
struct Unseekable {
    bytes: Vec<u8>,
    at: usize,
}

impl Unseekable {
    fn new(bytes: Vec<u8>) -> Self {
        Self { bytes, at: 0 }
    }
}

impl std::io::Read for Unseekable {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let end = (self.at + buf.len()).min(self.bytes.len());
        let read = end.saturating_sub(self.at);
        buf[..read].copy_from_slice(&self.bytes[self.at..end]);
        self.at = end;
        Ok(read)
    }
}

impl std::io::Seek for Unseekable {
    fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
        // Symphonia checks this before seeking.
        Err(std::io::ErrorKind::Unsupported.into())
    }
}

impl MediaSource for Unseekable {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.bytes.len() as u64)
    }
}

/// A real download decodes. Needs network and yt-dlp (`--ignored`).
#[tokio::test]
#[ignore = "needs network and yt-dlp"]
async fn fetches_audio_the_decoder_accepts() {
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;
    use symphonia::default::{get_codecs, get_probe};

    let dir = std::env::temp_dir().join("shamash-fetch-test");
    let path = fetched_file("wJGcwEv7838", "https://youtu.be/wJGcwEv7838", &dir).await;
    let bytes = std::fs::read(&path).expect("read");
    assert!(!bytes.is_empty(), "the download must not be empty");

    let source = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), Default::default());
    let mut probed = get_probe()
        .format(
            &Hint::new(),
            source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .expect("the probe must recognise the download");
    let track = probed
        .format
        .default_track()
        .expect("the download must expose a track");
    get_codecs()
        .make(&track.codec_params, &Default::default())
        .expect("the track's codec must be registered");
    assert!(
        probed.format.next_packet().is_ok(),
        "packets must be readable"
    );
}
