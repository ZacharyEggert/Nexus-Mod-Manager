pub mod adjust;
pub mod archive;
pub mod discover;
pub mod download;
pub mod fomod;
pub mod game;
pub mod ini;
pub mod invalidation;
pub mod launch;
pub mod loot;
pub mod manager;
pub mod modscript;
pub mod nexus;
pub mod omod;
pub mod plugins;
pub mod special;
pub mod tools;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The user cancelled a `Task`.
    #[error("cancelled")]
    Cancelled,
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Nexus rejected the API key.
    #[error("Nexus: invalid or missing API key")]
    Unauthorized,
    /// Nexus quota spent; calls fail locally for a while instead of hammering the API.
    #[error("Nexus: rate limit reached, try again later")]
    RateLimited,
    #[error("Nexus: {message} (HTTP {status})")]
    Http { status: u16, message: String },
    // ponytail: archive/XML/JSON errors are flattened to text; add variants when a caller branches on one.
    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<String> for Error {
    fn from(s: String) -> Self {
        Self::Other(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Self::Other(s.into())
    }
}

/// Attach the path to an I/O error: `fs::read(&p).at(&p)?`.
pub trait IoContext<T> {
    fn at(self, path: impl AsRef<Path>) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn at(self, path: impl AsRef<Path>) -> Result<T> {
        self.map_err(|source| Error::Io {
            path: path.as_ref().into(),
            source,
        })
    }
}

/// tmp + flush + rename, so `path` holds old or new bytes after a power cut, never garbage.
pub fn write_durable(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::File::create(&tmp).at(&tmp)?;
    f.write_all(bytes).at(&tmp)?;
    f.sync_all().at(&tmp)?;
    std::fs::rename(&tmp, path).at(path)?;
    // ponytail: dir flush is Unix-only; NTFS journals the rename itself.
    #[cfg(unix)]
    std::fs::File::open(path.parent().unwrap())
        .and_then(|d| d.sync_all())
        .at(path.parent().unwrap())?;
    Ok(())
}

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
        Self {
            cancel: Arc::default(),
            progress: Arc::new(|_, _, _| {}),
        }
    }
}

impl Task {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Report progress; `Err(Cancelled)` once cancel was requested.
    pub fn step(&self, label: &str, done: u64, total: u64) -> Result<()> {
        if self.cancelled() {
            return Err(Error::Cancelled);
        }
        (self.progress)(label, done, total);
        Ok(())
    }
}

/// Commands hand errors to the UI as their message.
impl serde::Serialize for Error {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_name_the_path_and_serialize_as_text() {
        let p = Path::new("/definitely/not/here.esp");
        let e = std::fs::read(p).at(p).unwrap_err();
        assert!(
            matches!(e, Error::Io { .. })
                && e.to_string().starts_with("/definitely/not/here.esp: "),
            "{e}"
        );
        assert_eq!(
            serde_json::to_string(&Error::Cancelled).unwrap(),
            r#""cancelled""#
        );
    }
}
