//! Per-request stage timings.
//!
//! One request crosses a speech model, three catalog providers, a YouTube
//! search and a yt-dlp download, and "the bot was slow" does not say which.
//! A [`Timing`] is created once per request and passed down the stages, so
//! each stage's cost is attributed rather than guessed at, and one summary
//! line per request says where the time went.
//!
//! Cloning is a handle copy, so a stage deep in the pipeline can be timed
//! without taking ownership of the stages around it.

use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The stages of one in-flight request, in the order they ran.
#[derive(Clone)]
pub struct Timing {
    started: Instant,
    stages: Arc<Mutex<Vec<(String, Duration)>>>,
    route: Arc<Mutex<Option<String>>>,
}

impl Default for Timing {
    /// Starts timing a request. The total is measured from here.
    fn default() -> Self {
        Self {
            started: Instant::now(),
            stages: Arc::new(Mutex::new(Vec::new())),
            route: Arc::new(Mutex::new(None)),
        }
    }
}

impl Timing {
    /// Records which way the audio was played: `stream` or `file`.
    ///
    /// Named in the summary line rather than as a stage, because it is not a
    /// cost but a choice -- and it is the answer to "was this song fast because
    /// it streamed or because it was already cached".
    pub fn set_route(&self, route: &str) {
        *self.route.lock().unwrap_or_else(PoisonError::into_inner) = Some(route.to_string());
    }

    /// Records a stage that has already run.
    pub fn record(&self, stage: &str, took: Duration) {
        self.stages
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((stage.to_string(), took));
    }

    /// Times `work` as `stage` and records however long it took.
    pub async fn stage<T>(&self, stage: &str, work: impl Future<Output = T>) -> T {
        let started = Instant::now();
        let out = work.await;
        self.record(stage, started.elapsed());
        out
    }

    /// The summary line, as it is logged.
    ///
    /// One line per request:
    /// `time_to_first_audio total=4210ms [transcription=640 llm_rewrite=812 ...]`.
    /// A stage that never ran is absent rather than reported as zero, because
    /// "the stage cost nothing" and "the stage did not happen" are different
    /// answers.
    fn summary(&self, path: &str) -> String {
        let total = self.started.elapsed();
        let stages = self.stages.lock().unwrap_or_else(PoisonError::into_inner);
        let breakdown = stages
            .iter()
            .map(|(name, took)| format!("{name}={}ms", took.as_millis()))
            .collect::<Vec<_>>()
            .join(" ");
        let route = self.route.lock().unwrap_or_else(PoisonError::into_inner);
        let path = match route.as_deref() {
            Some(route) => format!("{path}/{route}"),
            None => path.to_string(),
        };
        format!(
            "time_to_first_audio total={}ms [{breakdown}] path={path}",
            total.as_millis()
        )
    }

    /// Logs the one summary line a request produces.
    pub fn log(&self, path: &str) {
        tracing::info!(path, "{}", self.summary(path));
    }
}

#[cfg(test)]
mod tests {
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
}
