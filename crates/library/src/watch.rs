//! Watching the library folders. FSEvents on macOS (inotify and ReadDirectoryChangesW elsewhere,
//! all through `notify`).

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::Error;

/// A burst of events is reported once it has been quiet this long...
const QUIET: Duration = Duration::from_millis(100);
/// ...or this long after it began, so a long copy is rescanned while it runs.
const MAX_WAIT: Duration = Duration::from_millis(500);

/// Reports the directories that changed under the library folders. Dropping it stops watching.
pub struct Watcher {
    _watcher: RecommendedWatcher,
}

impl Watcher {
    /// Watches `roots` and calls `on_change` with the changed directories (each file's parent, so
    /// created, removed, renamed and rewritten files all report the folder to rescan) from a
    /// background thread. The paths are canonical, as [`crate::Scanner::scan_changed`] wants.
    pub fn start(roots: &[PathBuf], on_change: impl Fn(Vec<PathBuf>) + Send + 'static) -> Result<Self, Error> {
        let (tx, rx) = mpsc::channel::<Vec<PathBuf>>();
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if let Ok(event) = event
                && !matches!(event.kind, EventKind::Access(_))
            {
                let _ = tx.send(event.paths);
            }
        })?;
        for root in roots {
            watcher.watch(&fs::canonicalize(root)?, RecursiveMode::Recursive)?;
        }
        std::thread::Builder::new().name("flicks-watch".into()).spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut dirs = BTreeSet::new();
                add_parents(&mut dirs, first);
                let start = Instant::now();
                while let Some(left) = MAX_WAIT.checked_sub(start.elapsed()) {
                    match rx.recv_timeout(left.min(QUIET)) {
                        Ok(paths) => add_parents(&mut dirs, paths),
                        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
                    }
                }
                on_change(dirs.into_iter().collect());
            }
        })?;
        Ok(Self { _watcher: watcher })
    }
}

fn add_parents(dirs: &mut BTreeSet<PathBuf>, paths: Vec<PathBuf>) {
    dirs.extend(paths.iter().filter_map(|p| p.parent()).map(PathBuf::from));
}
