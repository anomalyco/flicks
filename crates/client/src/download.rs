//! Downloads (docs/architecture.md, "App"): a title's source copied to this device in chunks over
//! signed range reads, in the background, and resumable. Each download keeps a small record
//! (`<id>.json`) next to its partial file (`<id>.part`) in `downloads/` in the data directory; a
//! restart continues from the bytes already on disk. A finished copy is checked for size and
//! OSHash against the catalog item before it gets its real name, so [`Downloads::local_path`] only
//! ever answers with a complete, verified file. Progress made on a downloaded title while its
//! server is out of reach is kept here and pushed by [`Downloads::sync`] after the next refresh
//! (the server keeps the newest, as in [`crate::merge`]).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::task::JoinHandle;

use crate::merge::{Library, Source, Title};
use crate::{Connection, Error, Item, Progress};

/// The downloads directory inside the data directory.
pub const DIR: &str = "downloads";

/// Progress made offline, waiting for its server.
const PROGRESS_FILE: &str = "progress.json";

/// The extensions a finished file may have (the containers the library scans for, and `ts`).
const EXTENSIONS: [&str; 6] = ["mkv", "webm", "mp4", "m4v", "mov", "ts"];

/// Bytes per range read. Each read is its own request, so this is also how often a pause or
/// removal is noticed.
const CHUNK: u64 = 2 * 1024 * 1024;

const RUN: u8 = 0;
const PAUSE: u8 = 1;
const REMOVE: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    /// Running, or interrupted by a restart (it shows as [`State::Paused`] when nothing runs it).
    Downloading,
    Paused,
    /// Verified; [`Downloads::local_path`] has it.
    Complete,
    /// Stopped with this message; [`Downloads::start`] tries again from the bytes on disk, or from
    /// scratch if the copy was rejected.
    Failed(String),
}

#[derive(Serialize, Deserialize)]
struct Record {
    /// The server's key ([`crate::Server::key`]).
    server: String,
    server_name: String,
    item: Item,
    state: State,
}

/// One download, as [`Downloads::list`] shows it.
#[derive(Debug, Clone)]
pub struct Download {
    /// The server's key and the name it had when this was started.
    pub server: String,
    pub server_name: String,
    /// The file there; `item.size` is the total.
    pub item: Item,
    pub state: State,
    /// Bytes on disk.
    pub done: u64,
}

struct Job {
    done: AtomicU64,
    stop: AtomicU8,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl Job {
    fn running(&self) -> bool {
        lock(&self.handle).as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Asks the task to stop and waits until it has.
    async fn stop(&self, why: u8) {
        self.stop.store(why, Ordering::SeqCst);
        let handle = lock(&self.handle).take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The downloads of this device. Dropping it stops the running ones; what is on disk resumes.
pub struct Downloads {
    dir: PathBuf,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    /// Serializes changes to the offline progress file.
    pending: Mutex<()>,
}

impl Drop for Downloads {
    fn drop(&mut self) {
        for job in lock(&self.jobs).values() {
            if let Some(handle) = lock(&job.handle).as_ref() {
                handle.abort();
            }
        }
    }
}

impl Downloads {
    /// Opens (creating it) `downloads/` in the client data directory.
    pub fn open(data_dir: &Path) -> Result<Downloads, Error> {
        let dir = data_dir.join(DIR);
        std::fs::create_dir_all(&dir)?;
        Ok(Downloads { dir, jobs: Mutex::new(HashMap::new()), pending: Mutex::new(()) })
    }

    /// Starts downloading `source` over `connection` (to its server), or continues an earlier
    /// attempt from the bytes already on disk. Does nothing if it's running or complete. Needs a
    /// Tokio runtime.
    pub fn start(&self, connection: &Connection, source: &Source) -> Result<(), Error> {
        let base = base(&source.server, source.item.id);
        let mut jobs = lock(&self.jobs);
        if jobs.get(&base).is_some_and(|job| job.running()) {
            return Ok(());
        }
        if self.complete_copy(source).is_some() {
            return Ok(());
        }
        // The server's file changed since the copy began (or finished): start over.
        if let Some(old) = self.read(&base).filter(|old| !same_file(&old.item, &source.item)) {
            remove_file(&self.complete(&base, &old.item))?;
            remove_file(&self.part(&base))?;
        }
        let record = Record {
            server: source.server.clone(),
            server_name: source.server_name.clone(),
            item: source.item.clone(),
            state: State::Downloading,
        };
        write_json(&self.record(&base), &record)?;
        let job = Arc::new(Job { done: AtomicU64::new(0), stop: AtomicU8::new(RUN), handle: Mutex::new(None) });
        let files =
            Files { part: self.part(&base), complete: self.complete(&base, &record.item), record: self.record(&base) };
        let handle = tokio::spawn(run(job.clone(), connection.clone(), files, record));
        *lock(&job.handle) = Some(handle);
        jobs.insert(base, job);
        Ok(())
    }

    /// Stops a running download, keeping what is on disk for [`Downloads::start`] to continue.
    pub async fn pause(&self, server: &str, file: i64) {
        let job = lock(&self.jobs).get(&base(server, file)).cloned();
        if let Some(job) = job {
            job.stop(PAUSE).await;
        }
    }

    /// Cancels and deletes a download: stops it and deletes its partial or complete file and
    /// record.
    pub async fn remove(&self, server: &str, file: i64) -> Result<(), Error> {
        let base = base(server, file);
        let job = lock(&self.jobs).remove(&base);
        if let Some(job) = job {
            job.stop(REMOVE).await;
        }
        if let Some(record) = self.read(&base) {
            remove_file(&self.complete(&base, &record.item))?;
        }
        remove_file(&self.part(&base))?;
        remove_file(&self.record(&base))?;
        Ok(())
    }

    /// Every download, by title.
    pub fn list(&self) -> Vec<Download> {
        let mut downloads: Vec<Download> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let base = path.file_name()?.to_str()?.strip_suffix(".json")?.to_string();
                // `progress.json` isn't a record, so it reads as none.
                Some(self.download(&base, self.read(&base)?))
            })
            .collect();
        downloads.sort_by_cached_key(|d| (d.item.title.to_lowercase(), d.item.season, d.item.episode, d.item.id));
        downloads
    }

    /// One download of this file of this server.
    pub fn get(&self, server: &str, file: i64) -> Option<Download> {
        let base = base(server, file);
        self.read(&base).map(|record| self.download(&base, record))
    }

    /// The complete, verified local copy of this file of this server, if there is one.
    pub fn local_file(&self, server: &str, file: i64) -> Option<PathBuf> {
        let base = base(server, file);
        self.complete_file(&base, &self.read(&base)?)
    }

    /// The local copy of any source of the title, to play instead of reading over the network.
    pub fn local_path(&self, title: &Title) -> Option<PathBuf> {
        title.sources.iter().find_map(|s| self.complete_copy(s))
    }

    /// Saves progress made on this file of this server to push later, keeping the newest of what
    /// is waiting. A time ahead of this clock is taken as now, so a server never refuses it for
    /// being from the future.
    pub fn set_progress(&self, server: &str, file: i64, progress: Progress) -> Result<(), Error> {
        let progress = Progress { updated_at: progress.updated_at.min(now_ms()), ..progress };
        self.change_pending(|pending| match pending.iter_mut().find(|p| p.server == server && p.file == file) {
            Some(p) if p.progress.updated_at > progress.updated_at => {}
            Some(p) => p.progress = progress,
            None => pending.push(Pending { server: server.into(), file, progress }),
        })
    }

    /// The title's progress, with what was made offline if that is newer: what the player resumes
    /// from.
    pub fn progress(&self, title: &Title) -> Option<Progress> {
        let pending = self.read_pending();
        let local = title
            .sources
            .iter()
            .flat_map(|s| pending.iter().filter(|p| p.server == s.server && p.file == s.item.id))
            .map(|p| p.progress);
        local.chain(title.progress).max_by_key(|p| p.updated_at)
    }

    /// Pushes progress made offline to the servers that `library` reached on its last refresh
    /// (call this after [`Library::refresh`]): to every online source of the title it was made on
    /// (the server keeps the newest), and into the library's own copy of the title. It stays
    /// waiting when no server took it; it is dropped when the file is gone from its server (404).
    /// Returns how many were pushed.
    pub async fn sync(&self, library: &mut Library) -> Result<usize, Error> {
        let mut pushed = 0;
        for waiting in self.read_pending() {
            let title = library.titles().iter().find(|t| t.sources.iter().any(|s| waiting.is_for(s)));
            let done = match title.map(|t| t.key.clone()) {
                Some(key) => {
                    let progress =
                        Progress { updated_at: waiting.progress.updated_at.min(now_ms()), ..waiting.progress };
                    match library.set_progress(&key, progress).await {
                        Ok(()) => {
                            pushed += 1;
                            true
                        }
                        Err(Error::Status(404, _)) => true,
                        Err(_) => false,
                    }
                }
                // Its server answered and no longer lists the file.
                None => library.connection(&waiting.server).is_some(),
            };
            if done {
                self.change_pending(|pending| pending.retain(|p| *p != waiting))?;
            }
        }
        Ok(pushed)
    }

    fn record(&self, base: &str) -> PathBuf {
        self.dir.join(format!("{base}.json"))
    }

    fn part(&self, base: &str) -> PathBuf {
        self.dir.join(format!("{base}.part"))
    }

    /// Where the finished file goes: named by its container, to be recognized by what opens it.
    /// Only a known extension is used, so a server can't pick a name that collides with the
    /// record.
    fn complete(&self, base: &str, item: &Item) -> PathBuf {
        let container = item.container.to_ascii_lowercase();
        let extension = EXTENSIONS.iter().find(|e| **e == container).unwrap_or(&"bin");
        self.dir.join(format!("{base}.{extension}"))
    }

    fn read(&self, base: &str) -> Option<Record> {
        serde_json::from_slice(&std::fs::read(self.record(base)).ok()?).ok()
    }

    /// The finished file, if the record says complete and the file is all there.
    fn complete_file(&self, base: &str, record: &Record) -> Option<PathBuf> {
        let path = self.complete(base, &record.item);
        let whole = std::fs::metadata(&path).is_ok_and(|m| m.len() == record.item.size);
        (record.state == State::Complete && whole).then_some(path)
    }

    /// The finished file of this source, if what was downloaded is still the file the server has.
    fn complete_copy(&self, source: &Source) -> Option<PathBuf> {
        let base = base(&source.server, source.item.id);
        let record = self.read(&base).filter(|r| same_file(&r.item, &source.item))?;
        self.complete_file(&base, &record)
    }

    fn download(&self, base: &str, record: Record) -> Download {
        let running = lock(&self.jobs).get(base).filter(|job| job.running()).cloned();
        let on_disk = |path: PathBuf| std::fs::metadata(path).map_or(0, |m| m.len());
        let (state, done) = match (record.state, running) {
            (State::Complete, _) => (State::Complete, record.item.size),
            (State::Downloading, Some(job)) => (State::Downloading, job.done.load(Ordering::SeqCst)),
            (State::Downloading, None) => (State::Paused, on_disk(self.part(base))),
            (state, _) => (state, on_disk(self.part(base))),
        };
        Download { server: record.server, server_name: record.server_name, item: record.item, state, done }
    }

    fn read_pending(&self) -> Vec<Pending> {
        let _guard = lock(&self.pending);
        self.load_pending()
    }

    fn load_pending(&self) -> Vec<Pending> {
        std::fs::read(self.dir.join(PROGRESS_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn change_pending(&self, change: impl FnOnce(&mut Vec<Pending>)) -> Result<(), Error> {
        let _guard = lock(&self.pending);
        let mut pending = self.load_pending();
        change(&mut pending);
        write_json(&self.dir.join(PROGRESS_FILE), &pending)
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Pending {
    server: String,
    file: i64,
    progress: Progress,
}

impl Pending {
    fn is_for(&self, source: &Source) -> bool {
        self.server == source.server && self.file == source.item.id
    }
}

/// Whether two catalog entries are the same bytes.
fn same_file(a: &Item, b: &Item) -> bool {
    a.size == b.size && a.oshash.eq_ignore_ascii_case(&b.oshash)
}

/// Milliseconds since the Unix epoch, as [`Progress::updated_at`] counts them.
fn now_ms() -> i64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    i64::try_from(now).unwrap_or(i64::MAX)
}

/// The file name stem of a download: the server (hashed, since its key is long) and the file id.
fn base(server: &str, file: i64) -> String {
    let digest = Sha256::digest(server.as_bytes());
    format!("{}-{file}", data_encoding::HEXLOWER.encode(digest.get(..6).unwrap_or_default()))
}

struct Files {
    part: PathBuf,
    complete: PathBuf,
    record: PathBuf,
}

enum Ended {
    Complete,
    Stopped,
}

/// The background task of one download: fetches, then records how it ended.
async fn run(job: Arc<Job>, connection: Connection, files: Files, mut record: Record) {
    let state = match fetch(&job, &connection, &files, &record.item).await {
        Ok(Ended::Complete) => State::Complete,
        Ok(Ended::Stopped) if job.stop.load(Ordering::SeqCst) == REMOVE => return,
        Ok(Ended::Stopped) => State::Paused,
        Err(e) => State::Failed(e.to_string()),
    };
    record.state = state;
    let _ = write_json(&files.record, &record);
}

/// Reads the rest of the file from the end of the partial one, then checks and renames it.
async fn fetch(job: &Job, connection: &Connection, files: &Files, item: &Item) -> Result<Ended, Error> {
    let mut part = OpenOptions::new().create(true).append(true).read(true).open(&files.part)?;
    let mut done = part.metadata()?.len();
    if done > item.size {
        part.set_len(0)?;
        done = 0;
    }
    job.done.store(done, Ordering::SeqCst);
    while done < item.size {
        if job.stop.load(Ordering::SeqCst) != RUN {
            return Ok(Ended::Stopped);
        }
        let end = done.saturating_add(CHUNK).min(item.size) - 1;
        let bytes = connection.read(item.id, done, end).await?;
        if bytes.len() as u64 != end - done + 1 {
            return Err(invalid("the server sent less than asked for; the file may have changed there"));
        }
        part = tokio::task::spawn_blocking(move || part.write_all(&bytes).map(|()| part))
            .await
            .map_err(|_| invalid("the write stopped"))??;
        done += end - done + 1;
        job.done.store(done, Ordering::SeqCst);
    }
    part.sync_all()?;
    let hash = oshash(&mut part, item.size)?;
    if hash_text(hash) != item.oshash.to_ascii_lowercase() {
        remove_file(&files.part)?;
        return Err(invalid("the copy doesn't match the server's hash; deleted"));
    }
    drop(part);
    std::fs::rename(&files.part, &files.complete)?;
    Ok(Ended::Complete)
}

fn invalid(message: &str) -> Error {
    Error::Io(io::Error::new(io::ErrorKind::InvalidData, message))
}

fn hash_text(hash: u64) -> String {
    format!("{hash:016x}")
}

/// The OpenSubtitles hash, as the library computes it for the catalog: the file size plus every
/// little-endian u64 of the first and the last 64 KiB, wrapping.
fn oshash(file: &mut File, size: u64) -> io::Result<u64> {
    const CHUNK: u64 = 64 * 1024;
    let n = size.min(CHUNK);
    let mut hash = size;
    let mut buf = vec![0u8; n as usize];
    for start in [0, size - n] {
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buf)?;
        for word in buf.as_chunks::<8>().0 {
            hash = hash.wrapping_add(u64::from_le_bytes(*word));
        }
    }
    Ok(hash)
}

fn remove_file(path: &Path) -> Result<(), Error> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Replaces `path` with the JSON of `value`, readable by this user only.
fn write_json(path: &Path, value: &impl Serialize) -> Result<(), Error> {
    let bytes = serde_json::to_vec(value).map_err(|_| Error::BadResponse)?;
    crate::write_private(path, &bytes)
}
