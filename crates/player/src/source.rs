use std::collections::VecDeque;
use std::io::{self, BufReader, Read, Seek};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use flicks_container::matroska::ByteSource;

/// Network size comes from the server's `/play` item. Opening and seeking are blocking;
/// display calls never perform source I/O.
pub enum Source {
    Local(PathBuf),
    /// Automatically use the server's signed HLS ladder unless only direct is available or
    /// a measured, fast LAN link can comfortably carry the original over byte ranges.
    Network {
        connection: Arc<flicks_client::Connection>,
        file: i64,
        size: u64,
    },
}

impl Source {
    /// Prefer a complete, verified download for this server file. Offline progress belongs to
    /// the app; a local source otherwise uses exactly the same playback path.
    pub fn prefer_download(self, downloads: &flicks_client::download::Downloads, server: &str) -> Self {
        if let Self::Network { file, .. } = &self
            && let Some(path) = downloads.local_file(server, *file)
        {
            return Self::Local(path);
        }
        self
    }
}

pub(crate) enum Bytes {
    Local { reader: Mutex<(BufReader<std::fs::File>, u64)>, size: u64 },
    Network(ReadAhead),
}

impl Bytes {
    pub fn open(source: Source) -> io::Result<Self> {
        match source {
            Source::Local(path) => {
                let file = std::fs::File::open(path)?;
                let size = file.metadata()?.len();
                Ok(Self::Local { reader: Mutex::new((BufReader::with_capacity(BLOCK as usize, file), 0)), size })
            }
            Source::Network { connection, file, size } => Ok(Self::Network(ReadAhead::new(connection, file, size)?)),
        }
    }

    pub fn invalidate(&self) -> io::Result<()> {
        if let Self::Network(n) = self {
            let mut s = n.shared.0.lock().map_err(|_| io::Error::other("read-ahead poisoned"))?;
            s.blocks.clear();
            s.generation += 1;
            s.error = None;
            s.cursor = None;
            s.demand = None;
            n.shared.1.notify_all();
        }
        Ok(())
    }
}

impl ByteSource for Bytes {
    fn len(&self) -> u64 {
        match self {
            Self::Local { size, .. } => *size,
            Self::Network(n) => n.size,
        }
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        match self {
            Self::Local { reader, size } => {
                if offset.checked_add(buf.len() as u64).is_none_or(|end| end > *size) {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "range past EOF"));
                }
                let mut guard = reader.lock().map_err(|_| io::Error::other("local reader poisoned"))?;
                let (reader, position) = &mut *guard;
                let delta = i64::try_from(i128::from(offset) - i128::from(*position))
                    .map_err(|_| io::Error::other("file offset too large"))?;
                reader.seek_relative(delta)?;
                let result = reader.read_exact(buf);
                *position = if result.is_ok() { offset + buf.len() as u64 } else { reader.stream_position()? };
                result
            }
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
const BLOCKS: usize = 32; // 8 MiB LRU, not a whole-file cache.

struct Cache {
    blocks: VecDeque<(u64, Vec<u8>)>,
    cursor: Option<u64>,
    demand: Option<u64>,
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
            Mutex::new(Cache {
                blocks: VecDeque::new(),
                cursor: Some(0),
                demand: None,
                generation: 0,
                error: None,
                stop: false,
            }),
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
                        if s.error.is_none() {
                            if let Some(offset) = s.demand.take() {
                                break (offset, s.generation);
                            }
                            if let Some(offset) = s.cursor.take()
                                && offset < size
                                && !s.blocks.iter().any(|(at, _)| *at == offset)
                            {
                                break (offset, s.generation);
                            }
                        }
                        let Ok(next) = state.1.wait(s) else { return };
                        s = next;
                    }
                };
                let end = offset.saturating_add(BLOCK).min(size);
                // The client bounds connect/header and idle-byte waits. Do not abort an
                // actively progressing response just because the link takes >10 seconds.
                let mut result = Err(io::Error::other("range read failed"));
                for attempt in 0..2 {
                    if attempt > 0 {
                        let Ok(s) = state.0.lock() else { return };
                        if s.stop {
                            return;
                        }
                        if s.generation != generation {
                            break;
                        }
                    }
                    result = runtime
                        .block_on(connection.read(file, offset, end - 1))
                        .map_err(io::Error::other)
                        .and_then(|bytes| {
                            if bytes.len() as u64 == end - offset {
                                Ok(bytes.to_vec())
                            } else {
                                Err(io::Error::other("truncated range response"))
                            }
                        });
                    if result.is_ok() {
                        break;
                    }
                }
                let Ok(mut s) = state.0.lock() else { return };
                if s.generation != generation {
                    continue;
                }
                match result {
                    Ok(bytes) => {
                        if s.blocks.len() == BLOCKS {
                            s.blocks.pop_front();
                        }
                        s.blocks.push_back((offset, bytes));
                        if s.demand == Some(offset) {
                            s.demand = None;
                        }
                    }
                    Err(e) => s.error = Some(e.to_string()),
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
                let block = s.blocks.remove(i).ok_or_else(|| io::Error::other("missing cache block"))?;
                s.blocks.push_back(block);
                // Follow each active track one block ahead, including when the LRU is
                // full. Do not fill the cache with speculative blocks: that can evict
                // the other track's working set before it is consumed.
                s.cursor = Some(aligned.saturating_add(BLOCK));
                self.shared.1.notify_all();
                continue;
            }
            // Interleaved tracks can be far apart in the file. Keep both working sets
            // and let the in-flight range finish; only an explicit seek invalidates it.
            s.demand = Some(aligned);
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
