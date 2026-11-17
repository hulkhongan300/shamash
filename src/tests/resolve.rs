use super::*;

/// A trimmed copy of a real songs-search response for "dracula tame
/// impala". The shelf holding the rows is an array, which an earlier
/// parser did not descend into, so it found no songs at all and the
/// official upload never reached the ranking.
#[test]
fn songs_are_read_from_a_real_youtube_music_response() {
    let body: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/ytmusic_songs_dracula.json")).unwrap();
    let songs = parse_songs(&body);
    assert!(!songs.is_empty(), "no songs parsed");

    // YouTube Music lists the JENNIE feature first. It must not become the
    // song: that is how a request for Dracula played the remix.
    let hits = merge(songs.clone());
    let best = &hits[0];
    assert_eq!(best.title, "Dracula", "{hits:?}");
    assert_eq!(best.video_id.as_deref(), Some("5nCOjdHeYjM"), "{hits:?}");
    assert_eq!(best.duration_secs(), Some(206.0), "{hits:?}");
    let dracula = songs
        .iter()
        .find(|s| s.title == "Dracula")
        .unwrap_or_else(|| panic!("no plain Dracula in {songs:?}"));
    assert!(dracula.artist.contains("Tame Impala"), "{}", dracula.artist);
    assert!(dracula.video_id.is_some());
    assert!(dracula.duration_secs.is_some());
}

/// A trimmed copy of a real albums-search response for "17 xxxtentacion",
/// with an audiobook row from another search added to it.
#[test]
fn albums_are_read_from_a_real_youtube_music_response() {
    let body: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/ytmusic_albums_17.json")).unwrap();
    let albums = parse_albums(&body);
    assert_eq!(
        albums[0],
        CatalogAlbum {
            title: "17".to_string(),
            artist: "XXXTENTACION".to_string(),
            kind: ReleaseKind::Album,
            year: Some(2017),
            playlist_id: "OLAK5uy_kW9hN-oBmekJ06jhhfStpwRd5pcRKIztY".to_string(),
        }
    );
    assert_eq!(
        albums[0].url(),
        "https://music.youtube.com/playlist?list=OLAK5uy_kW9hN-oBmekJ06jhhfStpwRd5pcRKIztY"
    );
    assert!(
        albums
            .iter()
            .any(|a| a.kind == ReleaseKind::Single && a.artist == "Young Taylor"),
        "{albums:?}"
    );
    // The audiobook is not music, and has no playlist to play anyway.
    assert_eq!(albums.len(), 5, "{albums:?}");
    // And none of it is a song: there is no video to play on its own.
    assert!(parse_songs(&body).is_empty());
}

fn track(artist: &str, title: &str, duration: Option<f64>) -> CatalogTrack {
    CatalogTrack {
        artist: artist.to_string(),
        title: title.to_string(),
        duration_secs: duration,
        popularity: None,
        video_id: None,
    }
}

fn target(title: Option<&str>, artist: Option<&str>) -> Target {
    let raw = [title, artist]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    Target::from_request(&PlayRequest {
        query: raw.clone(),
        title: title.map(str::to_string),
        artist: artist.map(str::to_string),
        splits: crate::parser::readings(&raw)
            .into_iter()
            .filter_map(|(title, artist)| Some((title?, artist?)))
            .collect(),
    })
}

fn heard(phrase: &str) -> Target {
    Target::from_request(&PlayRequest {
        query: phrase.to_string(),
        title: None,
        artist: None,
        splits: crate::parser::readings(phrase)
            .into_iter()
            .filter_map(|(title, artist)| Some((title?, artist?)))
            .collect(),
    })
}

// ---- normalisation -----------------------------------------------------

#[test]
fn normalisation_drops_parenthesised_asides() {
    assert_eq!(matcher::normalize("Dracula (feat. The Weeknd)"), "dracula");
    assert_eq!(matcher::normalize("Song (Remastered 2011)"), "song");
    assert_eq!(matcher::normalize("Song - Single Version"), "song");
    assert_eq!(matcher::normalize("Song [Explicit]"), "song");
}

/// A bracket that *opens* the title is part of it. Dropping it reduced
/// "(I Can't Get No) Satisfaction" to "satisfaction".
#[test]
fn normalisation_keeps_a_leading_parenthetical() {
    assert_eq!(
        matcher::normalize("(I Can't Get No) Satisfaction"),
        "i can t get no satisfaction"
    );
    assert_eq!(
        matcher::normalize("(How Does It Feel) Pios Bolly"),
        "how does it feel pios bolly"
    );
}

#[test]
fn normalisation_drops_a_trailing_feature_credit() {
    assert_eq!(matcher::normalize("Song feat. Someone"), "song someone");
}

#[test]
fn normalisation_strips_punctuation_and_case() {
    assert_eq!(matcher::normalize("BTS!"), "bts");
    assert_eq!(matcher::normalize("Don't Stop Me Now"), "don t stop me now");
    assert_eq!(matcher::normalize("  spaced   out  "), "spaced out");
}

#[test]
fn normalisation_keeps_non_latin_scripts() {
    assert_eq!(
        matcher::normalize("방탄소년단 Dynamite"),
        "방탄소년단 dynamite"
    );
    assert_eq!(
        matcher::normalize("BTS (방탄소년단) Dynamite"),
        "bts dynamite"
    );
}

#[test]
fn normalisation_keeps_the_words_that_identify_a_song() {
    assert_eq!(
        matcher::normalize("The Dark Side of the Moon"),
        "the dark side of the moon"
    );
}

// ---- the query plan ----------------------------------------------------

/// A correctly heard artist with a misheard title is rescued by asking for
/// the artist's catalogue: that query does not need the title to be right.
#[test]
fn the_plan_asks_for_the_artists_own_tracks() {
    let catalog = Catalog::with_budget(vec![Box::new(Nothing)], 2_500);
    let kinds: Vec<&str> = catalog
        .plan(&target(Some("drakula"), Some("tame impala")))
        .iter()
        .map(CatalogQuery::kind)
        .collect();
    assert_eq!(kinds, ["loose", "loose", "title"]);
}

/// Four requests is the whole budget, whatever the request looks like.
#[test]
fn the_plan_never_exceeds_four_queries() {
    let catalog = Catalog::with_budget(vec![Box::new(Nothing)], 2_500);
    let request = PlayRequest {
        query: "a by b by c by d".to_string(),
        title: Some("a".to_string()),
        artist: Some("b".to_string()),
        splits: vec![
            ("a".to_string(), "b".to_string()),
            ("a by b".to_string(), "c".to_string()),
            ("a by b by c".to_string(), "d".to_string()),
        ],
    };
    assert!(catalog.plan(&Target::from_request(&request)).len() <= MAX_QUERIES_PER_PROVIDER);
}

/// Asking the same question twice costs a request for nothing.
#[test]
fn the_plan_never_repeats_a_query() {
    let catalog = Catalog::with_budget(vec![Box::new(Nothing)], 2_500);
    let request = PlayRequest {
        query: "stand by me".to_string(),
        title: Some("stand".to_string()),
        artist: Some("me".to_string()),
        splits: vec![("stand".to_string(), "me".to_string())],
    };
    let plan = catalog.plan(&Target::from_request(&request));
    let texts: Vec<String> = plan.iter().map(CatalogQuery::text).collect();
    let mut sorted = texts.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), texts.len(), "{texts:?}");
}

// ---- merging -----------------------------------------------------------

/// Rows for the same song collapse, and every distinct running time is kept.
#[test]
fn merging_keeps_every_distinct_duration() {
    let hits = merge(vec![
        track("Tame Impala", "Dracula", Some(205.0)),
        track("Tame Impala", "Dracula", Some(205.6)),
        track("Tame Impala", "Borderline", Some(238.0)),
    ]);
    assert_eq!(hits.len(), 2);
    let dracula = &hits[0];
    assert_eq!(dracula.durations.len(), 1, "3 s apart is one recording");
    assert_eq!(dracula.duration_secs(), Some(205.0));
}

/// Providers that disagree about the length by more than the tolerance are
/// not to be trusted for ranking uploads, and the hit says so rather than
/// picking one.
#[test]
fn a_length_nobody_agrees_on_is_marked_uncertain() {
    let hits = merge(vec![
        track("Tame Impala", "Dracula", Some(205.0)),
        track("Tame Impala", "Dracula", Some(241.0)),
    ]);
    assert_eq!(hits.len(), 1);
    assert!(hits[0].duration_uncertain);
    assert_eq!(
        hits[0].duration_secs(),
        None,
        "an uncertain length must not be used to rank uploads"
    );
}

/// "Song" and "Song (Live)" are one song.
#[test]
fn merging_de_dupes_on_the_normalized_song() {
    let hits = merge(vec![
        track("Queen", "Don't Stop Me Now", Some(214.0)),
        track("Queen", "Don't Stop Me Now (Live)", Some(228.0)),
    ]);
    assert_eq!(hits.len(), 1);
}

#[test]
fn a_hit_with_no_title_is_dropped() {
    assert!(merge(vec![track("Tame Impala", "  ", Some(205.0))]).is_empty());
}

// ---- the fan-out -------------------------------------------------------

struct Nothing;
#[async_trait]
impl CatalogProvider for Nothing {
    fn name(&self) -> &'static str {
        "nothing"
    }
    async fn search(&self, _q: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
        Ok(Vec::new())
    }
}

/// Albums come from the providers that know some; a provider that knows
/// none (the trait's default) adds nothing, and a failing one costs
/// nothing either.
#[tokio::test]
async fn albums_come_from_the_providers_that_know_them() {
    struct Albums;
    #[async_trait]
    impl CatalogProvider for Albums {
        fn name(&self) -> &'static str {
            "albums"
        }
        async fn search(&self, _q: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
            Ok(Vec::new())
        }
        async fn albums(&self, query: &str) -> Result<Vec<CatalogAlbum>, ProviderError> {
            assert_eq!(query, "17 xxxtentacion");
            Ok(vec![CatalogAlbum {
                title: "17".to_string(),
                artist: "XXXTENTACION".to_string(),
                kind: ReleaseKind::Album,
                year: Some(2017),
                playlist_id: "OLAK5uy_x".to_string(),
            }])
        }
    }
    struct Failing;
    #[async_trait]
    impl CatalogProvider for Failing {
        fn name(&self) -> &'static str {
            "failing"
        }
        async fn search(&self, _q: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
            Err(ProviderError::Unavailable)
        }
        async fn albums(&self, _query: &str) -> Result<Vec<CatalogAlbum>, ProviderError> {
            Err(ProviderError::Unavailable)
        }
    }
    let catalog = Catalog::with_budget(
        vec![Box::new(Nothing), Box::new(Failing), Box::new(Albums)],
        1_000,
    );
    let albums = catalog.albums("17 xxxtentacion").await;
    assert_eq!(albums.len(), 1, "{albums:?}");
    assert_eq!(albums[0].title, "17");
}

/// The whole point of the fan-out: one dead provider must not cost the
/// request. Before this, `?` on a provider error aborted the whole thing.
#[tokio::test]
async fn a_failing_provider_does_not_stop_the_others() {
    struct Broken;
    #[async_trait]
    impl CatalogProvider for Broken {
        fn name(&self) -> &'static str {
            "broken"
        }
        async fn search(&self, _q: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
            Err(ProviderError::Unavailable)
        }
    }
    struct Working;
    #[async_trait]
    impl CatalogProvider for Working {
        fn name(&self) -> &'static str {
            "working"
        }
        async fn search(&self, q: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
            if q.text() == "drakula tame impala" {
                Ok(vec![track("Tame Impala", "Dracula", Some(205.0))])
            } else {
                Ok(Vec::new())
            }
        }
    }

    let catalog = Catalog::with_budget(vec![Box::new(Broken), Box::new(Working)], 2_500);
    let hits = catalog.hits(&heard("drakula tame impala")).await;
    assert_eq!(hits.len(), 1, "the working provider still answered");
    assert_eq!(hits[0].label(), "Tame Impala — Dracula");
}

/// The fan-out returns inside its budget however slow the providers are.
/// A straggler is dropped, not waited for.
#[tokio::test(start_paused = true)]
async fn the_fan_out_returns_inside_its_budget() {
    struct Slow;
    #[async_trait]
    impl CatalogProvider for Slow {
        fn name(&self) -> &'static str {
            "slow"
        }
        fn rate(&self) -> Rate {
            // One a second, so the limiter alone would eat the budget.
            Rate::new(1.0, 1.0)
        }
        async fn search(&self, _q: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(vec![track("Tame Impala", "Dracula", Some(205.0))])
        }
    }
    let catalog = Catalog::with_budget(vec![Box::new(Slow)], 500);
    let hits = tokio::time::timeout(
        Duration::from_secs(5),
        catalog.hits(&heard("drakula tame impala")),
    )
    .await
    .expect("the fan-out must not wait for a straggler");
    assert!(hits.is_empty(), "a straggler is dropped, not used");
}

/// Providers are asked at the same time, so two slow ones cost one wait,
/// not two.
#[tokio::test(start_paused = true)]
async fn providers_are_asked_concurrently() {
    struct Slow;
    #[async_trait]
    impl CatalogProvider for Slow {
        fn name(&self) -> &'static str {
            "slow"
        }
        async fn search(&self, _q: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok(vec![track("Tame Impala", "Dracula", Some(205.0))])
        }
        async fn albums(&self, _query: &str) -> Result<Vec<CatalogAlbum>, ProviderError> {
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok(Vec::new())
        }
    }
    let catalog = Catalog::with_budget(vec![Box::new(Slow), Box::new(Slow)], 10_000);
    let started = TokioInstant::now();
    assert_eq!(catalog.hits(&heard("dracula")).await.len(), 1);
    assert!(started.elapsed() < Duration::from_millis(1500));
    let started = TokioInstant::now();
    catalog.albums("dracula").await;
    assert!(started.elapsed() < Duration::from_millis(1500));
}

/// A provider that never answers must not hold the request open.
#[tokio::test(start_paused = true)]
async fn a_provider_that_never_answers_times_out_instead_of_hanging() {
    struct Stalls;
    #[async_trait]
    impl CatalogProvider for Stalls {
        fn name(&self) -> &'static str {
            "stalls"
        }
        async fn search(&self, _q: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
            std::future::pending().await
        }
    }
    let catalog = Catalog::with_budget(vec![Box::new(Stalls)], 2_000);
    let hits = tokio::time::timeout(Duration::from_secs(30), catalog.hits(&heard("dracula")))
        .await
        .expect("resolve must return rather than hang");
    assert!(hits.is_empty());
}

/// Three failures in a row stop the bot asking that provider, and the
/// recovery is a half-open: one request through, to see.
#[tokio::test]
async fn the_breaker_opens_after_repeated_failures_and_half_opens_afterwards() {
    let guard = Guard::new(Rate::new(10.0, 10.0));
    assert!(!guard.is_open().await, "a fresh guard is closed");

    for _ in 0..BREAKER_FAILURES {
        guard.record(Err(ProviderError::Unavailable)).await;
    }
    assert!(guard.is_open().await, "three failures open it");

    // Nothing but time closes it, and there is no time to wait here.
    let waited = tokio::time::timeout(Duration::from_millis(50), async {}).await;
    assert!(waited.is_ok());
    assert!(
        guard.is_open().await,
        "the breaker stays open until its window passes"
    );
}

/// A success closes it again, so one bad minute is not a permanent loss of
/// a provider.
#[tokio::test]
async fn one_success_resets_the_failure_count() {
    let guard = Guard::new(Rate::new(10.0, 10.0));
    guard.record(Err(ProviderError::Unavailable)).await;
    guard.record(Err(ProviderError::Unavailable)).await;
    guard.record(Ok(())).await;
    guard.record(Err(ProviderError::Unavailable)).await;
    guard.record(Err(ProviderError::Unavailable)).await;
    assert!(
        !guard.is_open().await,
        "the run was broken by a success, not by three in a row"
    );
}

/// The limiter refuses a request it cannot serve in time rather than
/// holding the whole fan-out for one token.
#[tokio::test(start_paused = true)]
async fn the_limiter_refuses_rather_than_waiting_past_the_deadline() {
    let guard = Guard::new(Rate::new(1.0, 0.1));
    let deadline = TokioInstant::now();
    assert!(guard.take(deadline).await, "the burst is available");
    assert!(
        !guard.take(deadline).await,
        "the next token is ten seconds away and the deadline is now"
    );
}

/// With time left, the limiter waits rather than refusing.
#[tokio::test(start_paused = true)]
async fn the_limiter_waits_when_there_is_time() {
    let guard = Guard::new(Rate::new(1.0, 100.0));
    let deadline = TokioInstant::now() + Duration::from_secs(5);
    assert!(guard.take(deadline).await);
    assert!(guard.take(deadline).await, "100 a second refills fast");
}

/// Innertube nests its rows inside arrays, and a walk that stops at the
/// first array finds no songs at all. This is the shape a real songs search
/// returns: contents -> tabs (array) -> section list -> shelf -> rows.
#[test]
fn songs_are_found_inside_innertubes_arrays() {
    let body = serde_json::json!({
        "contents": {
            "tabbedSearchResultsRenderer": {
                "tabs": [{
                    "tabRenderer": {
                        "content": {
                            "sectionListRenderer": {
                                "contents": [{
                                    "musicShelfRenderer": {
                                        "contents": [{
                                            "musicResponsiveListItemRenderer": {
                                                "flexColumns": [
                                                    {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Dracula"}]}}},
                                                    {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                                                        {"text": "Tame Impala"},
                                                        {"text": " • "},
                                                        {"text": "Deadbeat"},
                                                        {"text": " • "},
                                                        {"text": "3:26"},
                                                    ]}}},
                                                    {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                                                        {"text": "217M plays"},
                                                    ]}}},
                                                ],
                                                "playlistItemData": {"videoId": "cuMuMnCRfqk"},
                                            },
                                        }],
                                    },
                                }],
                            },
                        },
                    },
                }],
            },
        },
    });
    let tracks = parse_songs(&body);
    assert_eq!(
        tracks.len(),
        1,
        "a shelf nested in an array is still a shelf"
    );
    assert_eq!(tracks[0].artist, "Tame Impala");
    assert_eq!(tracks[0].title, "Dracula");
    assert_eq!(tracks[0].duration_secs, Some(206.0));
    assert_eq!(tracks[0].popularity, Some(217_000_000));
    assert_eq!(tracks[0].video_id.as_deref(), Some("cuMuMnCRfqk"));
}

/// The site writes counts in four shapes; all of them are the same number
/// to the chooser and to `AMBIGUOUS_POLICY=popular`.
#[test]
fn play_counts_are_parsed_in_every_shape_the_site_uses() {
    assert_eq!(parse_plays("217M plays"), Some(217_000_000));
    assert_eq!(parse_plays("2.2M plays"), Some(2_200_000));
    assert_eq!(parse_plays("33K plays"), Some(33_000));
    assert_eq!(parse_plays("1,234 plays"), Some(1_234));
    assert_eq!(parse_plays("1 play"), Some(1));
    assert_eq!(parse_plays("1.2B views"), Some(1_200_000_000));
    assert_eq!(parse_plays("No plays"), None);
    assert_eq!(parse_plays(""), None);
}

/// The live endpoint moves without notice; this is how to notice. Ignored
/// by default because it needs the network: run it with `--ignored`.
#[tokio::test]
#[ignore = "hits the live YouTube Music endpoint; run with --ignored"]
async fn the_live_ytmusic_search_still_answers() {
    let hits = YtMusic::new()
        .expect("an HTTP client")
        .search(&CatalogQuery::Loose("dracula tame impala".to_string()))
        .await
        .expect("a live search");
    for hit in &hits {
        println!(
            "{:>4.0}s  {:<20} {:<30} {:>12} {}",
            hit.duration_secs.unwrap_or_default(),
            hit.artist,
            hit.title,
            hit.popularity.unwrap_or_default(),
            hit.video_id.as_deref().unwrap_or("-")
        );
    }
    assert!(!hits.is_empty(), "the live endpoint returned nothing");
    assert!(
        hits.iter().any(|hit| hit.artist == "Tame Impala"
            && hit.title == "Dracula"
            && hit.duration_secs == Some(206.0)),
        "the official Tame Impala track is missing"
    );
}
