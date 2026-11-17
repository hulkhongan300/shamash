use super::*;
use crate::config::DEFAULT_MATCH_THRESHOLD;

fn candidate(title: &str, views: Option<u64>, duration: Option<f64>) -> Candidate {
    Candidate {
        title: Some(title.to_string()),
        url: format!("https://youtu.be/{title}"),
        view_count: views,
        duration,
        channel: Some("someone".to_string()),
        id: None,
    }
}

fn on_channel(mut entry: Candidate, channel: &str) -> Candidate {
    entry.channel = Some(channel.to_string());
    entry
}

/// The catalog said *Dracula* by Tame Impala, 205 s.
fn catalogued(requested: &str) -> Target {
    Target {
        artist: Some("Tame Impala".to_string()),
        title: Some("Dracula".to_string()),
        durations: vec![205.0],
        requested: requested.to_string(),
        catalogued: true,
    }
}

/// The title of whichever candidate wins, for readable assertions.
fn best_title(candidates: &[Candidate], target: &Target) -> String {
    pick(candidates, target, DEFAULT_MATCH_THRESHOLD)
        .best()
        .expect("at least one candidate should survive")
        .candidate
        .title()
        .to_string()
}

/// Flat listings carry both `url` and `webpage_url`; aliasing one to the other fails every parse.
#[test]
fn a_flat_result_with_both_url_spellings_still_parses() {
    let line = r#"{
            "url": "https://www.youtube.com/watch?v=I0FKqmoK56I",
            "webpage_url": "https://www.youtube.com/watch?v=I0FKqmoK56I",
            "title": "Look At Me!",
            "channel": "XXXTENTACION",
            "duration": 126
        }"#;
    let candidate: Candidate = serde_json::from_str(line).expect("a flat result parses");
    assert_eq!(candidate.video_id(), Some("I0FKqmoK56I"));
    assert_eq!(candidate.duration, Some(126.0));
}

#[test]
fn the_query_uses_the_catalog_names() {
    let track = resolve::CatalogTrack {
        artist: "BTS".to_string(),
        title: "Hooligan".to_string(),
        duration_secs: Some(182.0),
        video_id: None,
        popularity: None,
    };
    let target = Target::from_track(&track, vec![182.0], "hoolig by bts");
    assert_eq!(target.search_query(), "BTS Hooligan");
}

/// The 75 M-view result is a different cut; only running time picks the right recording.
#[test]
fn a_remix_with_twenty_times_the_views_still_loses_to_the_right_recording() {
    let entries = [
        candidate(
            "Tame Impala - Dracula (Official Video)",
            Some(75_531_372),
            Some(234.0),
        ),
        candidate(
            "Tame Impala - Dracula (Lyrics)",
            Some(24_438_628),
            Some(206.0),
        ),
        candidate(
            "Tame Impala and JENNIE - Dracula (JENNIE Remix - Official Lyric Video)",
            Some(50_831_672),
            Some(211.0),
        ),
        candidate("Tame Impala - Dracula", Some(2_777_206), Some(206.0)),
    ];
    assert_eq!(
        best_title(&entries, &catalogued("dracula tame impala")),
        "Tame Impala - Dracula"
    );
}

/// A few seconds of padding is still the same recording.
#[test]
fn a_length_within_tolerance_counts_as_a_match() {
    let target = catalogued("dracula");
    for duration in [201.0, 205.0, 209.0] {
        let entry = candidate("Dracula", Some(1000), Some(duration));
        assert_eq!(
            score(&entry, &target).duration,
            DURATION_EXACT,
            "{duration}s should be within tolerance"
        );
    }
}

#[test]
fn a_length_outside_tolerance_is_penalised_proportionally() {
    let target = catalogued("dracula");
    // 210 s is 5 s out, so 1 s past the tolerance is charged.
    assert_eq!(
        score(&candidate("Dracula", None, Some(210.0)), &target).duration,
        -3.0
    );
    // 234 s is 29 s out, and 25 of those are past the tolerance.
    assert_eq!(
        score(&candidate("Dracula", None, Some(234.0)), &target).duration,
        -75.0
    );
}

/// Capped so a ten-minute mix cannot swamp the arithmetic.
#[test]
fn the_length_penalty_is_capped() {
    let target = catalogued("dracula");
    let parts = score(&candidate("1 Hour Mix", Some(10), Some(3_600.0)), &target);
    assert_eq!(parts.duration, -DURATION_PENALTY_MAX);
}

/// No duration means no length score, not a penalty.
#[test]
fn an_unknown_length_is_neither_rewarded_nor_punished() {
    let target = catalogued("dracula");
    assert_eq!(
        score(&candidate("Dracula", None, None), &target).duration,
        0.0
    );
}

#[test]
fn a_topic_channel_is_the_strongest_provenance_signal() {
    let target = catalogued("dracula");
    let parts = score(
        &on_channel(candidate("Dracula", None, None), "Tame Impala - Topic"),
        &target,
    );
    // A topic channel also names the artist, so it earns both bonuses.
    assert_eq!(parts.channel, CHANNEL_TOPIC + CHANNEL_ARTIST);
    assert!(parts.channel > CHANNEL_ARTIST);
}

#[test]
fn the_artists_own_channel_is_rewarded() {
    let target = catalogued("dracula");
    let parts = score(
        &on_channel(candidate("Dracula", None, None), "TameImpala"),
        &target,
    );
    assert_eq!(parts.channel, CHANNEL_ARTIST);
}

#[test]
fn a_vevo_channel_is_rewarded() {
    let target = catalogued("dracula");
    let parts = score(
        &on_channel(candidate("Dracula", None, None), "TameImpalaVEVO"),
        &target,
    );
    // A VEVO handle also names the artist, so it earns both bonuses.
    assert_eq!(parts.channel, CHANNEL_VEVO + CHANNEL_ARTIST);
}

/// The boost is for provenance, not for having a channel.
#[test]
fn an_unrelated_channel_earns_nothing() {
    let target = catalogued("dracula");
    let parts = score(
        &on_channel(candidate("Dracula", None, None), "7clouds"),
        &target,
    );
    assert_eq!(parts.channel, 0.0);
}

/// Every version the spec lists, against a title carrying it.
#[test]
fn every_version_marker_is_penalised() {
    let target = catalogued("dracula");
    for (title, expected) in [
        ("Dracula (Cover)", "cover"),
        ("Dracula (Live)", "live"),
        ("Dracula (Remix)", "remix"),
        ("Dracula (Lyrics)", "lyric"),
        ("Dracula (Lyric Video)", "lyric"),
        ("Dracula (Karaoke)", "karaoke"),
        ("Dracula (Sped Up)", "sped up"),
        ("Dracula (Slowed)", "slowed"),
        ("Dracula (Reverb)", "reverb"),
        ("Dracula (Instrumental)", "instrumental"),
        ("Dracula 1 Hour Mix", "1 hour"),
        ("Dracula Mix", "mix"),
        ("Dracula (Reaction)", "reaction"),
        ("Dracula (Tutorial)", "tutorial"),
    ] {
        let parts = score(&candidate(title, None, None), &target);
        assert!(
            parts.penalties.contains(&expected),
            "{title:?} should be penalised for {expected:?}, got {:?}",
            parts.penalties
        );
    }
}

/// Asking for the live version and getting the studio cut is the worse failure.
#[test]
fn a_marker_the_request_asked_for_is_not_penalised() {
    let target = catalogued("dracula live");
    for title in ["Dracula (Live)", "Dracula - Live at Glastonbury"] {
        let parts = score(&candidate(title, None, None), &target);
        assert!(
            parts.penalties.is_empty(),
            "{title:?} was asked for: {parts:?}"
        );
    }
}

/// "remix" contains "mix", but one fault is billed once.
#[test]
fn mix_does_not_also_fire_on_remix() {
    let target = catalogued("dracula");
    let parts = score(&candidate("Dracula (Remix)", None, None), &target);
    assert_eq!(parts.penalties, vec!["remix"]);
}

/// "live" matches whole words only.
#[test]
fn markers_only_match_whole_words() {
    let target = catalogued("delivery");
    assert!(
        score(&candidate("Dracula (Delivery)", None, None), &target)
            .penalties
            .is_empty()
    );
}

/// One fault, one charge, however many ways the title spells it.
#[test]
fn a_repeated_marker_is_charged_once() {
    let target = catalogued("dracula");
    let parts = score(
        &candidate("Dracula (Lyrics) [Lyric Video]", None, None),
        &target,
    );
    assert_eq!(parts.penalties, vec!["lyric"]);
}

/// Views break ties; 75 M must not beat the right length.
#[test]
fn views_cannot_outvote_a_length_mismatch() {
    let target = catalogued("dracula");
    let viral_mismatch = score(
        &candidate("Dracula (Official Video)", Some(75_000_000), Some(240.0)),
        &target,
    );
    let quiet_match = score(&candidate("Dracula", Some(900), Some(205.0)), &target);
    assert!(quiet_match.total() > viral_mismatch.total());
}

#[test]
fn views_break_ties_between_equally_good_uploads() {
    let target = catalogued("dracula");
    let popular = score(&candidate("Dracula", Some(9_000_000), Some(205.0)), &target);
    let obscure = score(&candidate("Dracula", Some(300), Some(205.0)), &target);
    assert!(popular.total() > obscure.total());
}

/// An upload reporting zero views is odd, not infinitely bad.
#[test]
fn a_zero_view_count_does_not_produce_a_negative_infinity() {
    let target = catalogued("dracula");
    let parts = score(&candidate("Dracula", Some(0), Some(205.0)), &target);
    assert!(parts.views.is_finite());
    assert_eq!(parts.views, 0.0);
}

/// A real match plays, a near miss asks.
#[test]
fn a_confident_match_is_not_offered_as_a_choice() {
    let entries = [on_channel(
        candidate("Dracula", Some(2_777_206), Some(206.0)),
        "Tame Impala - Topic",
    )];
    let choice = pick(&entries, &catalogued("dracula"), DEFAULT_MATCH_THRESHOLD);
    assert!(choice.confident, "{:?}", choice.best().unwrap().parts);
}

/// Nothing close, nothing to play: the top three go to the user.
#[test]
fn a_low_score_offers_a_choice_instead_of_guessing() {
    let entries = [
        candidate("completely unrelated one", Some(1_000), Some(200.0)),
        candidate("completely unrelated two", Some(9_000), Some(200.0)),
        candidate("completely unrelated three", Some(5_000), Some(200.0)),
    ];
    let choice = pick(&entries, &catalogued("dracula"), DEFAULT_MATCH_THRESHOLD);
    assert!(!choice.confident);
    let offered: Vec<&str> = choice
        .top(3)
        .iter()
        .map(|scored| scored.candidate.title())
        .collect();
    assert_eq!(
        offered,
        vec![
            "completely unrelated two",
            "completely unrelated three",
            "completely unrelated one"
        ]
    );
}

/// No length to score on, so a lower bar; relevance alone is untrustworthy, so never autoplayed.
#[test]
fn an_unresolved_request_is_judged_on_a_lower_bar_but_never_autoplayed() {
    let target = Target {
        artist: Some("BTS".to_string()),
        title: Some("Hooligan".to_string()),
        durations: Vec::new(),
        requested: "bts hoolig".to_string(),
        catalogued: false,
    };
    let entry = [on_channel(
        candidate("BTS 방탄소년단 'Hooligan'", Some(178_000), Some(182.0)),
        "BANGTANTV",
    )];
    let choice = pick(&entry, &target, DEFAULT_MATCH_THRESHOLD);
    assert!(
        !choice.confident,
        "an unverified pick is not played on its own"
    );
    assert!(
        choice.best().is_some(),
        "but it is still offered: {:?}",
        choice.best().map(|b| &b.parts)
    );
    assert!(target.effective_threshold(DEFAULT_MATCH_THRESHOLD) < DEFAULT_MATCH_THRESHOLD);
}

#[test]
fn the_ranking_is_ordered_best_first() {
    let entries = [
        candidate("Dracula (Lyrics)", Some(24_000_000), Some(206.0)),
        candidate("Dracula", Some(2_777_206), Some(206.0)),
        candidate("Dracula (Official Video)", Some(75_531_372), Some(234.0)),
    ];
    let choice = pick(&entries, &catalogued("dracula"), DEFAULT_MATCH_THRESHOLD);
    let titles: Vec<&str> = choice
        .ranked
        .iter()
        .map(|scored| scored.candidate.title())
        .collect();
    assert_eq!(
        titles,
        vec!["Dracula", "Dracula (Lyrics)", "Dracula (Official Video)"]
    );
}

/// An unrequested remix is a different song, dropped before scoring.
#[test]
fn an_unrequested_remix_is_dropped_rather_than_ranked() {
    let entries = [
        candidate("Dracula (Remix)", Some(50_000_000), Some(211.0)),
        candidate("Dracula", Some(2_777_206), Some(206.0)),
    ];
    let choice = pick(&entries, &catalogued("dracula"), DEFAULT_MATCH_THRESHOLD);
    assert_eq!(choice.ranked.len(), 1);
    assert_eq!(choice.ranked[0].candidate.title(), "Dracula");
}

/// Searching the raw transcript is what found the wrong song.
#[test]
fn the_query_uses_the_canonical_names() {
    let target = catalogued("hoolig bts");
    assert_eq!(target.search_query(), "Tame Impala Dracula");
}

#[test]
fn an_unresolved_request_searches_what_was_heard() {
    let target = Target {
        artist: None,
        title: None,
        durations: Vec::new(),
        requested: "the name of that one song".to_string(),
        catalogued: false,
    };
    assert_eq!(target.search_query(), "the name of that one song");
}

/// Clips and six-hour uploads are not songs, whatever they score.
#[test]
fn clips_and_long_mixes_are_excluded() {
    let entries = [
        candidate("clip", Some(100), Some(10.0)),
        candidate("long mix", Some(50), Some(4_000.0)),
    ];
    assert!(
        pick(&entries, &catalogued("dracula"), DEFAULT_MATCH_THRESHOLD)
            .best()
            .is_none()
    );
}

/// With nothing relevant, ranking degrades to popularity.
#[test]
fn an_unresolvable_request_falls_back_to_popularity() {
    let entries = [
        candidate("completely unrelated one", Some(1_000), Some(200.0)),
        candidate("completely unrelated two", Some(9_000), Some(200.0)),
    ];
    let target = Target {
        artist: None,
        title: Some("hooligan".to_string()),
        durations: Vec::new(),
        requested: "hooligan".to_string(),
        catalogued: false,
    };
    assert_eq!(best_title(&entries, &target), "completely unrelated two");
}

#[test]
fn no_results_selects_nothing() {
    assert!(
        pick(&[], &catalogued("dracula"), DEFAULT_MATCH_THRESHOLD)
            .best()
            .is_none()
    );
}

/// A title claiming both is a video; the video markers have to win.
#[test]
fn media_rank_orders_audio_above_video() {
    let rank = |title: &str| candidate(title, Some(1), Some(200.0)).media_rank();
    assert_eq!(rank("Song (Official Audio)"), 3);
    assert_eq!(rank("Song (Lyrics)"), 2);
    assert_eq!(rank("Song"), 1);
    assert_eq!(rank("Song (Official MV)"), 0);
    assert_eq!(rank("Song (Visualizer)"), 0);
    assert_eq!(rank("Song (Dance Practice)"), 0);
}

/// "Official Audio [Official MV]" is still a video.
#[test]
fn a_video_marker_beats_an_audio_marker_in_the_same_title() {
    let entry = candidate("Song (Official Audio) [Official MV]", Some(1), Some(200.0));
    assert_eq!(entry.media_rank(), 0);
}

/// Whole-string `jaro_winkler` cannot see past a leading artist; `matcher::best_window` handles that.
#[test]
fn a_misspelled_request_scores_badly_against_a_title_that_leads_with_the_artist() {
    let target = Target {
        artist: Some("BTS".to_string()),
        title: Some("Hooligan".to_string()),
        durations: Vec::new(),
        requested: "bts hoolig".to_string(),
        catalogued: false,
    };
    // Low, not zero; the old 0.72 shared-word floor let wrong songs clear the bar.
    let relevance = score(&candidate("BTS 방탄소년단 'Hooligan'", None, None), &target).relevance;
    assert!(
        relevance < RELEVANCE_MAX * 0.6,
        "a whole-string comparison cannot see through the leading artist: {relevance}"
    );
    // Hence unverified results are offered, not played (see `unverified_results_are_never_confident`).
}

#[test]
fn reads_the_video_id_from_every_url_shape() {
    // yt-dlp versions report different shapes; the dashboard needs the id for thumbnails.
    for (url, expected) in [
        ("https://www.youtube.com/watch?v=09839DpTctU", "09839DpTctU"),
        ("https://youtube.com/watch?v=09839DpTctU", "09839DpTctU"),
        ("https://youtu.be/09839DpTctU", "09839DpTctU"),
        ("https://www.youtube.com/shorts/09839DpTctU", "09839DpTctU"),
        (
            "https://www.youtube.com/watch?v=09839DpTctU&t=42s",
            "09839DpTctU",
        ),
        ("09839DpTctU", "09839DpTctU"),
    ] {
        assert_eq!(url_video_id(url), Some(expected), "from {url}");
    }
}

#[test]
fn an_unrecognised_url_has_no_video_id() {
    for url in ["", "https://example.com/watch", "https://vimeo.com/12345"] {
        assert_eq!(url_video_id(url), None, "from {url}");
    }
}

#[test]
fn a_reported_id_beats_parsing_the_url() {
    let mut entry = candidate("song", Some(1), Some(200.0));
    entry.id = Some("reported-id".to_string());
    assert_eq!(entry.video_id(), Some("reported-id"));
}

#[test]
fn the_video_id_falls_back_to_the_url() {
    let entry = candidate("09839DpTctU", Some(1), Some(200.0));
    assert_eq!(entry.video_id(), Some("09839DpTctU"));
}

#[test]
fn an_empty_reported_id_falls_back_to_the_url() {
    let mut entry = candidate("09839DpTctU", Some(1), Some(200.0));
    entry.id = Some(String::new());
    assert_eq!(entry.video_id(), Some("09839DpTctU"));
}

// ---- the resolution contract ------------------------------------------

/// A catalog that returns whatever was asked for, without network.
struct Stub {
    hits: Vec<resolve::CatalogTrack>,
}

#[async_trait::async_trait]
impl resolve::CatalogProvider for Stub {
    fn name(&self) -> &'static str {
        "stub"
    }
    fn rate(&self) -> resolve::Rate {
        resolve::Rate::new(20.0, 20.0)
    }
    async fn search(
        &self,
        _q: &resolve::CatalogQuery,
    ) -> Result<Vec<resolve::CatalogTrack>, resolve::ProviderError> {
        Ok(self.hits.clone())
    }
}

/// A catalog that finds nothing at all.
struct Empty;

#[async_trait::async_trait]
impl resolve::CatalogProvider for Empty {
    fn name(&self) -> &'static str {
        "empty"
    }
    async fn search(
        &self,
        _q: &resolve::CatalogQuery,
    ) -> Result<Vec<resolve::CatalogTrack>, resolve::ProviderError> {
        Ok(Vec::new())
    }
}

fn catalog_track(artist: &str, title: &str, duration: Option<f64>) -> resolve::CatalogTrack {
    resolve::CatalogTrack {
        artist: artist.to_string(),
        title: title.to_string(),
        duration_secs: duration,
        video_id: None,
        popularity: None,
    }
}

fn stub_catalog(hits: Vec<resolve::CatalogTrack>) -> resolve::Catalog {
    resolve::Catalog::with_budget(vec![Box::new(Stub { hits })], 2_500)
}

/// Records the queries it is asked for and answers with a fixed list.
struct FakeSearch {
    candidates: Vec<Candidate>,
    seen: std::sync::Mutex<Vec<String>>,
}

impl FakeSearch {
    fn with(candidates: Vec<Candidate>) -> Self {
        Self {
            candidates,
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl VideoSearch for FakeSearch {
    async fn candidates(&self, query: &str, _count: usize) -> anyhow::Result<Vec<Candidate>> {
        self.seen.lock().unwrap().push(query.to_string());
        Ok(self.candidates.clone())
    }
}

fn request(query: &str) -> PlayRequest {
    PlayRequest {
        query: query.to_string(),
        title: None,
        artist: None,
        splits: crate::parser::readings(query)
            .into_iter()
            .filter_map(|(title, artist)| Some((title?, artist?)))
            .collect(),
    }
}

/// A song-and-artist request resolves with its running time.
#[tokio::test]
async fn a_named_song_resolves_to_one_track() {
    let catalog = stub_catalog(vec![catalog_track("Tame Impala", "Dracula", Some(205.0))]);
    let resolved = resolve_request(
        &request("drakula by tame impala"),
        &catalog,
        None,
        None,
        GuildId::new(1),
        &crate::timing::Timing::default(),
    )
    .await;
    let track = resolved.as_track().expect("one song");
    assert_eq!(track.artist, "Tame Impala");
    assert_eq!(track.title, "Dracula");
    assert_eq!(track.duration_secs, Some(205.0));
}

/// Two artists, one title, nothing to tell them apart: ask (the README's "Look at Me").
#[tokio::test]
async fn a_title_two_artists_share_is_asked_about() {
    let catalog = stub_catalog(vec![
        catalog_track("Ben E. King", "Stand By Me", Some(191.0)),
        catalog_track("Oasis", "Stand By Me", Some(254.0)),
    ]);
    match resolve_request(
        &request("stand by me"),
        &catalog,
        None,
        None,
        GuildId::new(1),
        &crate::timing::Timing::default(),
    )
    .await
    {
        Resolved::Ask(tracks) => {
            assert_eq!(tracks.len(), 2);
            assert!(tracks.iter().any(|t| t.artist == "Ben E. King"));
            assert!(tracks.iter().any(|t| t.artist == "Oasis"));
        }
        other => panic!("{other:?}"),
    }
}

/// Everything failing is the same as nothing matching, not an error.
#[tokio::test]
async fn a_catalog_that_finds_nothing_is_unknown_not_an_error() {
    let catalog = resolve::Catalog::with_budget(vec![Box::new(Empty)], 2_500);
    assert!(matches!(
        resolve_request(
            &request("drakula tame impala"),
            &catalog,
            None,
            None,
            GuildId::new(1),
            &crate::timing::Timing::default()
        )
        .await,
        Resolved::Unknown
    ));
}

/// One failing provider must not stop the request.
#[tokio::test]
async fn a_failing_provider_leaves_the_request_alone() {
    struct Broken;
    #[async_trait::async_trait]
    impl resolve::CatalogProvider for Broken {
        fn name(&self) -> &'static str {
            "broken"
        }
        async fn search(
            &self,
            _q: &resolve::CatalogQuery,
        ) -> Result<Vec<resolve::CatalogTrack>, resolve::ProviderError> {
            Err(resolve::ProviderError::Unavailable)
        }
    }
    let catalog = resolve::Catalog::with_budget(vec![Box::new(Broken)], 2_500);
    let resolved = resolve_request(
        &request("drakula tame impala"),
        &catalog,
        None,
        None,
        GuildId::new(1),
        &crate::timing::Timing::default(),
    )
    .await;
    assert!(matches!(resolved, Resolved::Unknown));
}

/// The rewrite is a second candidate, verified like the heard words.
#[tokio::test]
async fn a_rewrite_can_rescue_a_request_the_words_could_not() {
    // The words as heard match nothing; the rewrite names the song.
    let catalog = stub_catalog(vec![catalog_track("Tame Impala", "Dracula", Some(205.0))]);
    let resolved = resolve_request(
        &request("drakoola tame implala"),
        &catalog,
        Some("dracula tame impala"),
        None,
        GuildId::new(1),
        &crate::timing::Timing::default(),
    )
    .await;
    assert_eq!(
        resolved.as_track().map(|t| t.title.as_str()),
        Some("Dracula")
    );
}

/// ...and cannot override heard words that already resolved.
#[tokio::test]
async fn a_rewrite_does_not_override_words_that_already_resolved() {
    let catalog = stub_catalog(vec![
        catalog_track("Tame Impala", "Dracula", Some(205.0)),
        catalog_track("Queen", "Bohemian Rhapsody", Some(354.0)),
    ]);
    let resolved = resolve_request(
        &request("drakula tame impala"),
        &catalog,
        Some("bohemian rhapsody queen"),
        None,
        GuildId::new(1),
        &crate::timing::Timing::default(),
    )
    .await;
    assert_eq!(
        resolved.as_track().map(|t| t.artist.as_str()),
        Some("Tame Impala"),
        "the words as heard win when they resolve"
    );
}

/// A rewrite that is nonsense resolves to nothing and changes nothing.
#[tokio::test]
async fn a_useless_rewrite_changes_nothing() {
    let catalog = stub_catalog(vec![catalog_track("Tame Impala", "Dracula", Some(205.0))]);
    let resolved = resolve_request(
        &request("drakula tame impala"),
        &catalog,
        Some("hello there"),
        None,
        GuildId::new(1),
        &crate::timing::Timing::default(),
    )
    .await;
    assert_eq!(
        resolved.as_track().map(|t| t.title.as_str()),
        Some("Dracula")
    );
}

// ---- the upload search ------------------------------------------------

#[tokio::test]
async fn a_confident_song_is_searched_for_by_its_canonical_names() {
    let resolved = Resolved::Track {
        track: catalog_track("Tame Impala", "Dracula", Some(205.0)),
        durations: vec![205.0],
    };
    let searcher = FakeSearch::with(vec![candidate(
        "Tame Impala - Dracula",
        Some(2_777_206),
        Some(206.0),
    )]);
    let choice = find_video(
        &resolved,
        "drakula tame impala",
        &searcher,
        8,
        DEFAULT_MATCH_THRESHOLD,
        &crate::timing::Timing::default(),
    )
    .await
    .expect("a result");
    assert_eq!(
        searcher.seen.lock().unwrap().as_slice(),
        ["Tame Impala Dracula"]
    );
    assert!(choice.confident);
    assert_eq!(
        choice.resolved.map(|t| t.label()),
        Some("Tame Impala — Dracula".to_string())
    );
}

/// A provider-named upload skips the search, which could only pick a different video.
#[tokio::test]
async fn a_catalogued_video_is_played_without_searching_again() {
    let mut track = catalog_track("XXXTENTACION", "Look At Me!", Some(127.0));
    track.video_id = Some("I0FKqmoK56I".to_string());
    let resolved = Resolved::Track {
        track,
        durations: vec![127.0],
    };
    let searcher = FakeSearch::with(vec![candidate("Look At Me!", Some(9_000_000), Some(127.0))]);
    let choice = find_video(
        &resolved,
        "xxxtentacion look at me",
        &searcher,
        8,
        DEFAULT_MATCH_THRESHOLD,
        &crate::timing::Timing::default(),
    )
    .await
    .expect("a result");
    assert!(searcher.seen.lock().unwrap().is_empty());
    assert!(choice.confident);
    assert_eq!(
        choice.best().and_then(|scored| scored.candidate.video_id()),
        Some("I0FKqmoK56I")
    );
}

/// Providers disagree by seconds; the closest of the set decides.
#[tokio::test]
async fn an_upload_matching_any_reported_length_is_the_right_recording() {
    let resolved = Resolved::Track {
        track: catalog_track("Tame Impala", "Dracula", Some(205.0)),
        durations: vec![205.0, 209.0, 241.0],
    };
    let entries = [
        candidate(
            "Tame Impala - Dracula (JENNIE Remix)",
            Some(50_000_000),
            Some(241.0),
        ),
        candidate("Tame Impala - Dracula", Some(2_777_206), Some(209.0)),
    ];
    let searcher = FakeSearch::with(entries.to_vec());
    let choice = find_video(
        &resolved,
        "drakula tame impala",
        &searcher,
        8,
        DEFAULT_MATCH_THRESHOLD,
        &crate::timing::Timing::default(),
    )
    .await
    .expect("a result");
    assert_eq!(
        choice.best().unwrap().candidate.title(),
        "Tame Impala - Dracula",
        "the shorter recording wins; 241 s is the remix"
    );
}

/// The song is known, so only the recording is asked about.
#[tokio::test]
async fn no_convincing_upload_means_offering_recordings_not_songs() {
    let resolved = Resolved::Track {
        track: catalog_track("Tame Impala", "Dracula", Some(205.0)),
        durations: vec![205.0],
    };
    let entries = [
        candidate("a completely unrelated upload", Some(1_000), Some(1200.0)),
        candidate("another unrelated one", Some(9_000), Some(90.0)),
    ];
    let searcher = FakeSearch::with(entries.to_vec());
    let choice = find_video(
        &resolved,
        "drakula tame impala",
        &searcher,
        8,
        DEFAULT_MATCH_THRESHOLD,
        &crate::timing::Timing::default(),
    )
    .await
    .expect("a result");
    assert!(
        !choice.confident,
        "the song is known but this upload is not"
    );
    assert_eq!(
        choice.resolved.map(|t| t.label()),
        Some("Tame Impala — Dracula".to_string()),
        "the caller still knows which song these uploads are meant to be"
    );
}

/// No catalog behind them, so `find_unverified` never marks results confident.
#[tokio::test]
async fn unverified_results_are_never_confident() {
    let entries = [on_channel(
        candidate("BTS 방탄소년단 'Hooligan'", Some(178_000), Some(182.0)),
        "BANGTANTV",
    )];
    let searcher = FakeSearch::with(entries.to_vec());
    let choice = find_unverified(
        &request("hoolig bts"),
        &searcher,
        8,
        0.0,
        &crate::timing::Timing::default(),
    )
    .await
    .expect("a result");
    assert!(!choice.confident);
    assert!(choice.best().is_some(), "but it is still offered");
}

/// No results is a search failure, kept as an error.
#[tokio::test]
async fn no_results_from_the_search_is_an_error() {
    let resolved = Resolved::Track {
        track: catalog_track("Tame Impala", "Dracula", Some(205.0)),
        durations: vec![205.0],
    };
    let searcher = FakeSearch::with(Vec::new());
    let err = find_video(
        &resolved,
        "drakula",
        &searcher,
        8,
        DEFAULT_MATCH_THRESHOLD,
        &crate::timing::Timing::default(),
    )
    .await
    .expect_err("an empty search is a failure")
    .to_string();
    assert!(err.contains("no results"), "unhelpful: {err}");
}

/// A failed search names the query in its error, for the log.
#[tokio::test]
async fn a_failing_search_names_the_query() {
    struct Broken;
    #[async_trait::async_trait]
    impl VideoSearch for Broken {
        async fn candidates(&self, _query: &str, _n: usize) -> anyhow::Result<Vec<Candidate>> {
            anyhow::bail!("yt-dlp is not installed")
        }
    }
    let resolved = Resolved::Track {
        track: catalog_track("BTS", "Hooligan", Some(182.0)),
        durations: vec![182.0],
    };
    let err = find_video(
        &resolved,
        "hoolig bts",
        &Broken,
        8,
        DEFAULT_MATCH_THRESHOLD,
        &crate::timing::Timing::default(),
    )
    .await
    .expect_err("a failed search is a failure")
    .to_string();
    assert_eq!(err, "could not search YouTube for 'BTS Hooligan'");
}

/// Given a question, `find_video` would silently pick a song for the user.
#[tokio::test]
async fn a_question_cannot_be_turned_into_a_video() {
    let asked = Resolved::Ask(vec![catalog_track("Oasis", "Stand By Me", Some(254.0))]);
    let searcher = FakeSearch::with(vec![candidate("Stand By Me", Some(1), Some(254.0))]);
    let err = find_video(
        &asked,
        "stand by me",
        &searcher,
        8,
        DEFAULT_MATCH_THRESHOLD,
        &crate::timing::Timing::default(),
    )
    .await
    .expect_err("a question is not a song")
    .to_string();
    assert!(err.contains("a resolved song"), "unhelpful: {err}");
    assert!(
        searcher.seen.lock().unwrap().is_empty(),
        "and nothing was searched for"
    );
}

// ---- the popularity policy -------------------------------------------
fn hit(artist: &str, title: &str, popularity: Option<u32>) -> resolve::Hit {
    resolve::Hit {
        artist: artist.to_string(),
        title: title.to_string(),
        durations: Vec::new(),
        duration_uncertain: false,
        popularity,
        video_id: None,
    }
}

/// The live "biome fest" bug: auto played a 4k-play cover over the 4.5 M official; popularity settles only same-title ties.
#[test]
fn auto_picks_the_most_played_recording_of_the_same_title() {
    let hits = [
        hit(
            "Celestial Fury",
            "Biome Fest (from \"Minecraft\")",
            Some(4_100),
        ),
        hit("C418", "Biome Fest", Some(4_500_000)),
        hit("Sheet Music Boss", "Biome Fest", Some(91_000)),
        hit("C418", "Dreiton", Some(999_999_999)),
    ];
    let options: Vec<&resolve::Hit> = hits.iter().collect();
    let picked = most_played_same_title(&options).expect("an option");
    assert_eq!(picked.artist, "C418");
    assert_eq!(picked.title, "Biome Fest");
}

/// With no counts at all, auto keeps the matcher's first option.
#[test]
fn auto_without_plays_keeps_the_matcher_order() {
    let hits = [hit("A", "Shared", None), hit("B", "Shared", None)];
    let options: Vec<&resolve::Hit> = hits.iter().collect();
    assert_eq!(most_played_same_title(&options).unwrap().artist, "A");
}

/// `AMBIGUOUS_POLICY=popular` is timid: the winner must be 3x bigger.
#[test]
fn popularity_only_settles_a_tie_when_it_is_overwhelming() {
    let big = resolve::CatalogTrack {
        popularity: Some(900),
        ..catalog_track("A", "Shared", Some(200.0))
    };
    let small = resolve::CatalogTrack {
        popularity: Some(100),
        ..catalog_track("B", "Shared", Some(200.0))
    };
    assert!(popular_beats_a_rival(&big, std::slice::from_ref(&small)));
    assert!(!popular_beats_a_rival(&small, std::slice::from_ref(&big)));

    // Barely more than twice as big is not enough.
    let middling = resolve::CatalogTrack {
        popularity: Some(400),
        ..catalog_track("B", "Shared", Some(200.0))
    };
    assert!(!popular_beats_a_rival(&big, &[middling]));
}

/// Same-artist recordings are no rivalry, and a rival without popularity gives nothing to compare.
#[test]
fn popularity_never_settles_a_question_within_one_artist() {
    let big = resolve::CatalogTrack {
        popularity: Some(900),
        ..catalog_track("A", "Shared", Some(200.0))
    };
    let same_artist = resolve::CatalogTrack {
        popularity: Some(10),
        ..catalog_track("A", "Shared", Some(240.0))
    };
    assert!(!popular_beats_a_rival(&big, &[same_artist]));

    let unknown = catalog_track("B", "Shared", Some(200.0));
    assert!(!popular_beats_a_rival(&big, &[unknown]));
    assert!(!popular_beats_a_rival(
        &catalog_track("A", "Shared", None),
        &[big]
    ));
}

// ---- live --------------------------------------------------------------

/// The whole path against the real catalog and YouTube: `cargo test -- --ignored search`.
#[tokio::test]
#[ignore = "needs network, yt-dlp and the YouTube Music API"]
async fn the_whole_path_picks_the_requested_recording() {
    // Not via `Config`, which would demand a Discord token and channel id.
    let catalog = resolve::Catalog::with_budget(
        vec![Box::new(resolve::YtMusic::new().expect("HTTP client"))],
        2_500,
    );
    for (heard, expected) in [
        // Misheard proper nouns, and a video that outranks the real track.
        ("drakula tame impala", "Dracula"),
        ("hoolig bts", "Hooligan"),
    ] {
        let resolved = resolve_request(
            &request(heard),
            &catalog,
            None,
            None,
            GuildId::new(1),
            &crate::timing::Timing::default(),
        )
        .await;
        assert!(resolved.as_track().is_some(), "{heard}: {resolved:?}");
        let choice = find_video(
            &resolved,
            heard,
            &YtDlpSearch,
            8,
            DEFAULT_MATCH_THRESHOLD,
            &crate::timing::Timing::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("{heard}: {e:#}"));
        for scored in &choice.ranked {
            println!(
                "{:7.1} {:7.1}s {}",
                scored.score(),
                scored.candidate.duration.unwrap_or(0.0),
                scored.candidate.title()
            );
        }
        let best = choice.best().expect("a result");
        assert!(
            choice.confident,
            "{heard:?} was not confident: {:?}",
            best.parts
        );
        assert!(
            best.candidate
                .title()
                .to_lowercase()
                .contains(&expected.to_lowercase()),
            "{heard:?} picked {:?}, expected something naming {expected:?}",
            best.candidate.title()
        );
    }
}

/// Hits the real YouTube via yt-dlp.
#[tokio::test]
#[ignore = "needs network and yt-dlp"]
async fn ranks_a_live_search_against_the_catalog() {
    // The search that motivated all of this, and its results as observed.
    let resolved = Resolved::Track {
        track: resolve::CatalogTrack {
            artist: "Tame Impala".to_string(),
            title: "Dracula".to_string(),
            duration_secs: Some(205.0),
            popularity: None,
            video_id: None,
        },
        durations: vec![205.0],
    };
    let target = Target::from_track(
        resolved.as_track().expect("a song"),
        vec![205.0],
        "dracula tame impala",
    );
    let entries = youtube_candidates(&target.search_query(), 8).await.unwrap();
    println!("searched: {}", target.search_query());
    let choice = pick(&entries, &target, DEFAULT_MATCH_THRESHOLD);
    for scored in &choice.ranked {
        println!("{:7.1} {}", scored.score(), scored.parts.explain());
    }
    let best = choice.best().unwrap();
    assert_eq!(best.candidate.duration, Some(206.0));
    assert!(
        choice.confident,
        "the requested recording should have been recognised: {:?}",
        best.parts
    );
}
