use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use flicks_container::matroska::ByteSource;

/// Network size comes from the server's `/play` item. Opening and reading are blocking;
/// keep the player on the app's playback thread, not its UI or async executor thread.
pub enum Source {
    Local(PathBuf),
    Network { connection: Arc<flicks_client::Connection>, file: i64, size: u64 },
}

pub(crate) enum Bytes {
    Local(std::fs::File),
    Network(ReadAhead),
}

impl Bytes {
    pub fn open(source: Source) -> io::Result<Self> {
        match source {
            Source::Local(path) => Ok(Self::Local(std::fs::File::open(path)?)),
            Source::Network { connection, file, size } => Ok(Self::Network(ReadAhead::new(connection, file, size)?)),
        }
    }

    pub fn invalidate(&self) -> io::Result<()> {
        if let Self::Network(n) = self {
            let mut s = n.shared.0.lock().map_err(|_| io::Error::other("read-ahead poisoned"))?;
            s.blocks.clear();
            s.generation += 1;
            s.error = None;
            s.cursor = n.size;
            n.shared.1.notify_all();
        }
        Ok(())
    }
}

impl ByteSource for Bytes {
    fn len(&self) -> u64 {
        match self {
            Self::Local(f) => ByteSource::len(f),
            Self::Network(n) => n.size,
        }
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        match self {
            Self::Local(f) => ByteSource::read_at(f, offset, buf),
            Self::Network(n) => n.read_at(offset, buf),
        }
    }
}

impl flicks_container::isobmff::ByteSource for Bytes {
    fn len(&self) -> u64 {
        ByteSource::len(self)
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        ByteSource::read_at(self, offset, buf)
    }
}

const BLOCK: u64 = 256 * 1024;
const BLOCKS: usize = 32; // 8 MiB ahead, not a whole-file cache.

struct Cache {
    blocks: VecDeque<(u64, Vec<u8>)>,
    cursor: u64,
    generation: u64,
    error: Option<String>,
    stop: bool,
}

pub(crate) struct ReadAhead {
    size: u64,
    shared: Arc<(Mutex<Cache>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

impl ReadAhead {
    fn new(connection: Arc<flicks_client::Connection>, file: i64, size: u64) -> io::Result<Self> {
        let shared = Arc::new((
            Mutex::new(Cache { blocks: VecDeque::new(), cursor: 0, generation: 0, error: None, stop: false }),
            Condvar::new(),
        ));
        let state = shared.clone();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let worker = std::thread::Builder::new().name("flicks-read-ahead".into()).spawn(move || {
            loop {
                let (offset, generation) = {
                    let Ok(mut s) = state.0.lock() else { return };
                    loop {
                        if s.stop {
                            return;
                        }
                        let next = s.blocks.back().map_or(s.cursor, |(at, b)| at + b.len() as u64);
                        if s.error.is_none() && s.blocks.len() < BLOCKS && next < size {
                            break (next, s.generation);
                        }
                        let Ok(next) = state.1.wait(s) else { return };
                        s = next;
                    }
                };
                let end = offset.saturating_add(BLOCK).min(size);
                let result = runtime.block_on(async {
                    tokio::time::timeout(Duration::from_secs(10), connection.read(file, offset, end - 1)).await
                });
                let Ok(mut s) = state.0.lock() else { return };
                if s.generation != generation {
                    continue;
                }
                match result {
                    Ok(Ok(bytes)) if bytes.len() as u64 == end - offset => s.blocks.push_back((offset, bytes.to_vec())),
                    Ok(Ok(_)) => s.error = Some("truncated range response".into()),
                    Ok(Err(e)) => s.error = Some(e.to_string()),
                    Err(_) => s.error = Some("range request timed out".into()),
                }
                state.1.notify_all();
            }
        })?;
        Ok(Self { size, shared, worker: Some(worker) })
    }

    fn read_at(&self, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
        if offset.checked_add(buf.len() as u64).is_none_or(|end| end > self.size) {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "range past EOF"));
        }
        let mut s = self.shared.0.lock().map_err(|_| io::Error::other("read-ahead poisoned"))?;
        while !buf.is_empty() {
            let aligned = offset / BLOCK * BLOCK;
            if let Some(i) = s.blocks.iter().position(|(at, b)| *at <= offset && offset < *at + b.len() as u64) {
                let (at, bytes) = s.blocks.get(i).ok_or_else(|| io::Error::other("missing cache block"))?;
                let data = bytes.get((offset - at) as usize..).ok_or_else(|| io::Error::other("cache offset"))?;
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                buf = &mut buf[n..];
                offset += n as u64;
                // Keep one preceding block for small demuxer look-backs.
                for _ in 0..i.saturating_sub(1) {
                    s.blocks.pop_front();
                }
                self.shared.1.notify_all();
                continue;
            }
            let next = s.blocks.back().map_or(s.cursor, |(at, b)| at + b.len() as u64);
            let first = s.blocks.front().map_or(s.cursor, |(at, _)| *at);
            if aligned < first || aligned > next || s.blocks.len() == BLOCKS {
                s.blocks.clear();
                s.cursor = aligned;
                s.generation += 1;
                s.error = None;
            }
            if let Some(error) = &s.error {
                return Err(io::Error::other(error.clone()));
            }
            self.shared.1.notify_all();
            s = self.shared.1.wait(s).map_err(|_| io::Error::other("read-ahead poisoned"))?;
        }
        Ok(())
    }
}

impl Drop for ReadAhead {
    fn drop(&mut self) {
        if let Ok(mut s) = self.shared.0.lock() {
            s.stop = true;
            self.shared.1.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
