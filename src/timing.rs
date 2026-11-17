//! Per-request stage timings, logged as one summary line so "the bot was slow" says which stage.
//!
//! Cloning is a handle copy, so nested stages share one request.

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
    /// Starts timing a request; the total is measured from here.
    fn default() -> Self {
        Self {
            started: Instant::now(),
            stages: Arc::new(Mutex::new(Vec::new())),
            route: Arc::new(Mutex::new(None)),
        }
    }
}

impl Timing {
    /// Records how the audio was played (`stream` or `file`); a choice, not a cost, so not a stage.
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

    /// The logged summary line; stages that never ran are absent, not zero.
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
#[path = "tests/timing.rs"]
mod tests;
