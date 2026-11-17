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

/// A stage that never ran is left out, so "not configured" is
/// distinguishable from "cost nothing" -- which is how the rewriting
/// stage, off by default, is told apart from one that returned instantly.
#[tokio::test]
async fn a_stage_that_never_ran_is_absent() {
    let timing = Timing::default();
    assert!(!timing.summary("voice").contains("llm_rewrite"));
}

/// The route names which way the audio was played, so one line says both how
/// long it took and what was done to get there. Without it, a fast request
/// and a slow one with a cached file are indistinguishable.
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

/// Two clones share one request's stages, which is the whole reason the
/// handle is a clone and not a moved value.
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
