//! Watching the library folders. FSEvents on macOS, inotify on Linux (ReadDirectoryChangesW on
//! Windows), all through `notify`.
//!
//! inotify needs one watch per directory and the user's limit (`fs.inotify.max_user_watches`) can
//! run out. When it does the watcher says so once and rescans every folder every [`RESCAN`] instead.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::Error;

/// A burst of events is reported once it has been quiet this long...
const QUIET: Duration = Duration::from_millis(100);
/// ...or this long after it began, so a long copy is rescanned while it runs.
const MAX_WAIT: Duration = Duration::from_millis(500);
/// How often every folder is rescanned once the OS watch limit is reached. An unchanged folder
/// costs one directory listing, so this is cheap.
const RESCAN: Duration = Duration::from_secs(30);

/// Reports the directories that changed under the library folders. Dropping it stops watching.
pub struct Watcher {
    _watcher: RecommendedWatcher,
}

impl Watcher {
    /// Watches `roots` and calls `on_change` with the changed directories (each file's parent, so
    /// created, removed, renamed and rewritten files all report the folder to rescan) from a
    /// background thread. The paths are canonical, as [`crate::Scanner::scan_changed`] wants.
    pub fn start(roots: &[PathBuf], on_change: impl Fn(Vec<PathBuf>) + Send + 'static) -> Result<Self, Error> {
        let roots = roots.iter().map(fs::canonicalize).collect::<Result<Vec<_>, _>>()?;
        let (tx, rx) = mpsc::channel::<Vec<PathBuf>>();
        let limit = Arc::new(AtomicBool::new(false));
        let handler = {
            let (tx, limit, roots) = (tx, limit.clone(), roots.clone());
            move |event: notify::Result<notify::Event>| match event {
                // The kernel queue overflowed: events were lost, so look at everything.
                Ok(event) if event.need_rescan() => {
                    let _ = tx.send(roots.clone());
                }
                Ok(event) if !matches!(event.kind, EventKind::Access(_)) => {
                    let _ = tx.send(parents(event.paths));
                }
                // A directory created since the watch started couldn't be watched.
                Err(error) if matches!(error.kind, notify::ErrorKind::MaxFilesWatch) => {
                    note_limit(&limit);
                    let _ = tx.send(roots.clone());
                }
                _ => {}
            }
        };
        let mut watcher = notify::recommended_watcher(handler)?;
        for root in &roots {
            match watcher.watch(root, RecursiveMode::Recursive) {
                Err(error) if matches!(error.kind, notify::ErrorKind::MaxFilesWatch) => note_limit(&limit),
                result => result?,
            }
        }
        std::thread::Builder::new().name("flicks-watch".into()).spawn(move || {
            loop {
                let first = if limit.load(Ordering::Relaxed) {
                    match rx.recv_timeout(RESCAN) {
                        Ok(first) => first,
                        Err(RecvTimeoutError::Timeout) => {
                            on_change(roots.clone());
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                } else {
                    match rx.recv() {
                        Ok(first) => first,
                        Err(_) => break,
                    }
                };
                let mut dirs = BTreeSet::from_iter(first);
                let start = Instant::now();
                while let Some(left) = MAX_WAIT.checked_sub(start.elapsed()) {
                    match rx.recv_timeout(left.min(QUIET)) {
                        Ok(more) => dirs.extend(more),
                        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
                    }
                }
                on_change(dirs.into_iter().collect());
            }
        })?;
        Ok(Self { _watcher: watcher })
    }
}

/// Switches to periodic rescans, saying why the first time.
fn note_limit(limit: &AtomicBool) {
    if !limit.swap(true, Ordering::Relaxed) {
        eprintln!(
            "catalog watch: the OS watch limit is reached (raise fs.inotify.max_user_watches); \
             rescanning the folders every {} s instead",
            RESCAN.as_secs()
        );
    }
}

fn parents(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.iter().filter_map(|p| p.parent()).map(PathBuf::from).collect()
}
