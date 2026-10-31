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
    pub(super) hls: Arc<super::hls::Cache>,
    wake: Mutex<Option<mpsc::Sender<Event>>>,
}

enum Event {
    Folders,
    Changed(Vec<PathBuf>),
    Stop,
}

impl Api {
    pub(super) fn wake(&self) {
        if let Some(tx) = self.wake.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = tx.send(Event::Folders);
        }
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        if let Some(tx) = self.wake.get_mut().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = tx.send(Event::Stop);
        }
    }
}

pub(super) fn start(state: &Arc<State>) -> Result<(), Error> {
    let mut slot = state.api.wake.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_some() {
        return Ok(());
    }
    let (tx, rx) = mpsc::channel();
    let path = state.data_dir.join(crate::DB_FILE);
    let artwork = state.data_dir.join("artwork");
    let stills = crate::stills::start(&state.data_dir)?;
    let events = tx.clone();
    std::thread::Builder::new().name("flicks-catalog".into()).spawn(move || {
        let run = || -> Result<(), Error> {
            let mut conn = Connection::open(path)?;
            conn.busy_timeout(Duration::from_secs(5))?;
            conn.pragma_update(None, "foreign_keys", true)?;
            let scanner = Scanner::new(Tmdb::from_env(), artwork);
            let mut roots = Vec::<PathBuf>::new();
            let mut watcher = None;
            let mut event = Event::Folders;
            loop {
                if matches!(event, Event::Stop) {
                    break;
                }
                let next: Vec<_> = db::folders(&conn)?.into_iter().map(|f| PathBuf::from(f.path)).collect();
                let full = next != roots || matches!(event, Event::Folders);
                if full || watcher.is_none() {
                    flicks_library::retain_roots(&mut conn, &next).map_err(super::library_error)?;
                    let events = events.clone();
                    watcher = match Watcher::start(&next, move |dirs| {
                        let _ = events.send(Event::Changed(dirs));
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
                if full {
                    for root in &roots {
                        if let Err(error) = scanner.scan(&mut conn, root) {
                            eprintln!("catalog scan: {error}");
                        }
                    }
                } else if let Event::Changed(dirs) = event
                    && let Err(error) = scanner.scan_changed(&mut conn, &roots, &dirs)
                {
                    eprintln!("catalog scan: {error}");
                }
                // No State reference is held: Api::drop wakes this blocking receive to stop it.
                // Only enqueue after publication. Still decoding never occupies this scan thread.
                let _ = stills.try_send(());
                event = match rx.recv() {
                    Ok(event) => event,
                    Err(_) => break,
                };
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
