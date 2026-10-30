use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use flicks_library::{Scanner, Watcher};
use flicks_metadata::Tmdb;
use rusqlite::Connection;

use crate::{Error, db, server::State};

/// Per-server ownership; its worker owns the watcher, scanner and optional TMDB client.
#[derive(Default)]
pub struct Api {
    wake: Mutex<Option<mpsc::SyncSender<()>>>,
}

impl Api {
    pub(super) fn wake(&self) {
        if let Some(tx) = self.wake.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            // A queued wake already requests a full rescan; coalesce bursts rather than grow a queue.
            let _ = tx.try_send(());
        }
    }
}

pub(super) fn start(state: &Arc<State>) -> Result<(), Error> {
    let mut slot = state.api.wake.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_some() {
        return Ok(());
    }
    let (tx, rx) = mpsc::sync_channel(1);
    let weak = Arc::downgrade(state);
    let path = state.data_dir.join(crate::DB_FILE);
    let artwork = state.data_dir.join("artwork");
    let events = tx.clone();
    std::thread::Builder::new().name("flicks-catalog".into()).spawn(move || {
        let run = || -> Result<(), Error> {
            let mut conn = Connection::open(path)?;
            conn.busy_timeout(Duration::from_secs(5))?;
            conn.pragma_update(None, "foreign_keys", true)?;
            let scanner = Scanner::new(Tmdb::from_env(), artwork);
            let mut roots = Vec::<PathBuf>::new();
            let mut watcher = None;
            loop {
                // Never retain State while scanning or waiting: dropping it stops this worker.
                if weak.strong_count() == 0 {
                    break;
                }
                let next: Vec<_> = db::folders(&conn)?.into_iter().map(|f| PathBuf::from(f.path)).collect();
                if next != roots || watcher.is_none() {
                    let events = events.clone();
                    watcher = match Watcher::start(&next, move |_| {
                        let _ = events.try_send(());
                    }) {
                        Ok(watcher) => Some(watcher),
                        Err(error) => {
                            eprintln!("catalog watch: {error}");
                            None
                        }
                    };
                    roots = next;
                }
                // Watching starts before the scan, so changes during a scan trigger another pass.
                for root in &roots {
                    if let Err(error) = scanner.scan(&mut conn, root) {
                        eprintln!("catalog scan: {error}");
                    }
                }
                loop {
                    match rx.recv_timeout(Duration::from_secs(1)) {
                        Ok(()) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                        Err(mpsc::RecvTimeoutError::Timeout) if weak.strong_count() == 0 => return Ok(()),
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
            }
            Ok(())
        };
        if let Err(error) = run() {
            eprintln!("catalog worker stopped: {error}");
        }
    })?;
    *slot = Some(tx);
    Ok(())
}
