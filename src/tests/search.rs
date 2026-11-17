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

/// The catalog's names are what the YouTube query is built from. Getting
/// them right is the whole reason the catalog is called at all: a
/// misheard "hoolig" would otherwise be searched as those letters and come
/// back as whatever matched them.
/// A flat yt-dlp listing carries both `url` and `webpage_url`. Reading
/// either one as an alias for the other makes serde see a duplicate field
/// and fail every result, which showed up as "failed to parse yt-dlp JSON
/// output" on every search.
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

/// The bug the whole module exists for, as a live search saw it.
///
/// Every result names the requested song and is roughly its length. The
/// 75-million-view video is a different cut, and ranking by views -- what
/// the old selector did -- picks it. Only the running time separates them,
/// and only the penalty separates the two 206-second uploads.
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

/// The tolerance is four seconds either way, so a video that padded the
/// start is not treated as a different recording.
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

/// A ten-minute mix is not a wrong answer, it is a different product, and
/// the penalty has to be capped so it cannot swamp the arithmetic.
#[test]
fn the_length_penalty_is_capped() {
    let target = catalogued("dracula");
    let parts = score(&candidate("1 Hour Mix", Some(10), Some(3_600.0)), &target);
    assert_eq!(parts.duration, -DURATION_PENALTY_MAX);
}

/// A duration-less upload cannot be scored on length and must not be
/// assumed wrong for it.
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
    // A topic channel is YouTube's channel for the artist, so it earns
    // both provenance bonuses; it is the best evidence available.
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
    // A VEVO channel is the artist's channel under a label handle, so it
    // earns both provenance bonuses.
    assert_eq!(parts.channel, CHANNEL_VEVO + CHANNEL_ARTIST);
}

/// An unrelated channel earns nothing, which is the point: the boost is for
/// provenance, not for having a channel at all.
#[test]
fn an_unrelated_channel_earns_nothing() {
    let target = catalogued("dracula");
    let parts = score(
        &on_channel(candidate("Dracula", None, None), "7clouds"),
        &target,
    );
    assert_eq!(parts.channel, 0.0);
}

/// Every version the spec lists, checked against the title that would carry
/// it.
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

/// The listener is the reason for the conditional. Asking for the live
/// version and being given the studio cut is a worse failure than either
/// one alone.
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

/// "remix" contains "mix", so a naive substring test would bill the same
/// title twice for one fault.
#[test]
fn mix_does_not_also_fire_on_remix() {
    let target = catalogued("dracula");
    let parts = score(&candidate("Dracula (Remix)", None, None), &target);
    assert_eq!(parts.penalties, vec!["remix"]);
}

/// "live" is a whole word, so a title that merely contains the letters is
/// not mistaken for a live recording.
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

/// Views are a tie-breaker, not a ranking. 75 million views must not be
/// worth more than being the right length.
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

/// The threshold is the whole point of the chooser: a real match plays, a
/// near miss asks.
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

/// A request the catalog could not resolve has no length to score on, so
/// holding it to the full threshold would ask about every song forever.
///
/// What it may no longer assert is that a good hit plays. This case encoded
/// the shared-token floor that 1.2 removed: relevance used to be
/// `similarity("hooligan", "bts 방탄소년단 hooligan") * 25`, and a pair
/// sharing any one whole word was floored at 0.72 — so an upload whose
/// title carries the artist name *in front of* the song scored as well as
/// an exact title. That is the mechanism behind "hello by adele" answering
/// with *Hello Brother*, and it is exactly what item 1.3 now covers from the
/// other side: an unresolved request is offered as a choice, never played
/// on its own.
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

/// The list is already ordered best first, so the best result is index zero
/// and the alternatives follow it.
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

/// A remix of something nobody asked for is not a near miss, it is a
/// different song, so it is dropped before it is scored.
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

/// The query is the corrected one. Searching the transcript is what
/// produced the wrong song in the first place.
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

/// Search results are not curated: a ten-second clip and a six-hour upload
/// are not songs, whatever they score.
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

/// The pre-catalog ranking still had to cope with junk arriving from the
/// search, so it degrades to popularity when nothing is relevant.
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

/// A video marker wins over an audio marker in the same title, so
/// "Official Audio [Official MV]" is still treated as a video.
#[test]
fn a_video_marker_beats_an_audio_marker_in_the_same_title() {
    let entry = candidate("Song (Official Audio) [Official MV]", Some(1), Some(200.0));
    assert_eq!(entry.media_rank(), 0);
}

/// Speech-to-text drops letters from proper nouns, so a request that is
/// nearly a title still has to match it.
///
/// This test encoded the shared-token floor item 1.2 removed, and it shows
/// exactly why the floor had to go: relevance is a whole-string
/// `jaro_winkler`, and an upload whose title leads with the artist
/// ("BTS 방탄소년단 'Hooligan'") shares no *prefix* with the bare word
/// "hooligan", so it scores zero for the title it is named after and loses
/// to a wrong song on view count. The floor papered over that by crediting
/// any shared word, which is the same mechanism that answered "hello by
/// adele" with *Hello Brother*. Both cases are handled properly by
/// `matcher::best_window`, which matches the title against a window of the
/// spoken words and so ignores a leading artist.
#[test]
fn a_misspelled_request_scores_badly_against_a_title_that_leads_with_the_artist() {
    let target = Target {
        artist: Some("BTS".to_string()),
        title: Some("Hooligan".to_string()),
        durations: Vec::new(),
        requested: "bts hoolig".to_string(),
        catalogued: false,
    };
    // Not zero, but far below what the title deserves. The old floor made
    // any pair sharing one word worth 0.72 of the 25 available, which is
    // what let a wrong song clear the bar on a single shared word.
    let relevance = score(&candidate("BTS 방탄소년단 'Hooligan'", None, None), &target).relevance;
    assert!(
        relevance < RELEVANCE_MAX * 0.6,
        "a whole-string comparison cannot see through the leading artist: {relevance}"
    );
    // Which is why an unverified result is offered as a choice rather than
    // played: relevance is all the scorer has left without a catalog (see
    // `unverified_results_are_never_confident`).
}

#[test]
fn reads_the_video_id_from_every_url_shape() {
    // --flat-playlist search reports these in different shapes depending
    // on version, and the dashboard needs the id for the thumbnail.
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

/// A catalog that hands back whatever the request asked for, without any
/// network. Keeps the catalog half of these tests out of the picture.
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

/// The ordinary case: a request that names a song and an artist resolves to
/// that song, and the running time comes with it for the upload search.
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

/// Two artists, one title, nothing said to tell them apart: the bot must
/// ask rather than pick. This is the README's "Look at Me" case.
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

/// Everything failing is not an error. It is the same outcome as nothing
/// matching, and the caller reports it rather than playing something.
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

/// A provider that fails outright must not stop the request. Before the
/// fan-out, `?` on a provider error aborted everything: one unreadable
/// response and the listener heard nothing at all.
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

/// The rewriting stage is a second *candidate*, not a replacement: it can
/// only ever propose a transcript, which then has to clear the same catalog
/// verification as the words that were heard.
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

/// ...and it cannot override the words as heard when those already
/// resolved. The heard transcript is the evidence; the rewrite is the
/// speculation.
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

/// ---- the upload search ------------------------------------------------
///
/// The catalog names the song; the upload search finds the recording. Both
/// have to agree before anything plays.
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

/// A provider that names the upload skips the search entirely. Searching
/// would rank a second opinion and could play a different video.
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

/// Providers disagree about the length by a few seconds; an upload matching
/// any of them is the right recording, so the closest of the set decides.
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

/// Nothing that clears the bar means the *song* is known but the *upload*
/// is not, which is a far narrower question to ask.
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

/// Unverified results are never played on their own. There is no catalog
/// behind them, so the scorer has a title similarity and a view count and
/// nothing else, and `find_unverified` says so rather than trusting the
/// caller to remember.
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

/// An empty result list is a failure of the search itself, not a ranking
/// result, so it stays an error the caller reports.
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

/// A search that fails is reported with the query in the message, because
/// the query is what has to be logged when it happens.
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

/// `find_video` needs a song. Asking it to find an upload of a *question*
/// would silently pick one of the offered songs for the user.
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

/// The live "biome fest" failure: four rows normalize to the same title,
/// the cover was first in the matcher's list, and auto played 4k plays
/// instead of the official 4.5M. Popularity settles only that same-title
/// tie; a different song's huge count cannot win.
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

/// `AMBIGUOUS_POLICY=popular` is a documented escape hatch, and it is
/// deliberately timid: the popular one has to be *three times* the other.
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

/// Two recordings by the *same* artist are not a choice between artists,
/// and a rival with no popularity gives nothing to compare against.
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

/// The whole path, against the real catalog and the real YouTube.
///
/// Ignored by default because it needs network access, the yt-dlp binary
/// and the YouTube Music API: `cargo test -- --ignored search`.
#[tokio::test]
#[ignore = "needs network, yt-dlp and the YouTube Music API"]
async fn the_whole_path_picks_the_requested_recording() {
    // Built directly rather than through `Config`, which would also want a
    // Discord token and a voice channel id that a test has no use for.
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

/// Hits the real YouTube via yt-dlp. Ignored by default because it needs
/// network access and the yt-dlp binary.
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
