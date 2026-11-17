use super::*;

#[tokio::test]
async fn a_request_reports_every_stage_it_ran_in_one_line() {
    let timing = Timing::default();
    timing.stage("transcription", async {}).await;
    timing.stage("catalog.hits", async {}).await;
    timing.record("fetch.transcode", Duration::from_millis(250));

    let line = timing.summary("voice");
    assert!(line.starts_with("time_to_first_audio total="), "{line}");
    assert!(line.contains("transcription="), "{line}");
    assert!(line.contains("catalog.hits="), "{line}");
    assert!(line.contains("fetch.transcode=250ms"), "{line}");
    assert!(line.contains("path=voice"), "{line}");
}

/// Absent, not zero, so "not configured" differs from "cost nothing".
#[tokio::test]
async fn a_stage_that_never_ran_is_absent() {
    let timing = Timing::default();
    assert!(!timing.summary("voice").contains("llm_rewrite"));
}

/// Without the route, a streamed request and a cached-file one look alike.
#[tokio::test]
async fn the_summary_names_the_route_the_audio_was_played_by() {
    let timing = Timing::default();
    assert!(timing.summary("voice").contains("path=voice"));

    timing.set_route("stream");
    assert!(
        timing.summary("voice").contains("path=voice/stream"),
        "{}",
        timing.summary("voice")
    );
    timing.set_route("file");
    assert!(timing.summary("voice").contains("path=voice/file"));
}

#[tokio::test]
async fn a_clone_records_into_the_same_request() {
    let timing = Timing::default();
    let inside = timing.clone();
    timing
        .stage("youtube_candidates", inside.stage("fetch", async {}))
        .await;
    let line = timing.summary("voice");
    assert!(line.contains("fetch="), "{line}");
    assert!(line.contains("youtube_candidates="), "{line}");
}
