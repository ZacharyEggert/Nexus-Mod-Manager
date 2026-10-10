pub mod adjust;
pub mod archive;
pub mod discover;
pub mod fomod;
pub mod game;
pub mod manager;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// (step label, done, total); `total == 0` when unknown.
pub type ProgressFn = dyn Fn(&str, u64, u64) + Send + Sync;

/// Progress + cancellation for a long operation. `Task::default()` reports nowhere and never cancels.
#[derive(Clone)]
pub struct Task {
    pub cancel: Arc<AtomicBool>,
    pub progress: Arc<ProgressFn>,
}

impl Default for Task {
    fn default() -> Self {
        Self { cancel: Arc::default(), progress: Arc::new(|_, _, _| {}) }
    }
}

impl Task {
    pub const CANCELLED: &str = "cancelled";

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Report progress; `Err(CANCELLED)` once cancel was requested.
    pub fn step(&self, label: &str, done: u64, total: u64) -> Result<(), String> {
        if self.cancelled() {
            return Err(Self::CANCELLED.into());
        }
        (self.progress)(label, done, total);
        Ok(())
    }
}
