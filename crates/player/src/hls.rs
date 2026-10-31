//! Signed Flicks VOD. One cancellable downloader buffers compressed cuts; the existing decode
//! worker consumes them in decode-time order. Neither network I/O nor decoder changes run on display.
mod playlist;
use crate::{
    AdaptiveStats, Error, Result, Tier, Track, TrackKind,
    abr::Abr,
    demux::{Packet, mp4_track, ns},
};
use flicks_client::Connection;
use flicks_container::{
    isobmff::{CodecConfig, Sample, cmaf},
    matroska::Codec,
};
use std::{
    net::IpAddr,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU32, AtomicU64, Ordering::Relaxed},
        mpsc,
    },
    thread,
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

const TARGET_MS: u32 = 30_000;
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);
const CUT_TIMEOUT: Duration = Duration::from_secs(60);
// Includes mdat, headers and sample indexes in both the queue and the active cut. Only one
// additional cut may be in flight/waiting for admission; no speculative decoded video surfaces.
const BUFFER_BYTES: usize = 72 << 20;
fn stopped() -> Error {
    Error::Media("HLS downloader stopped")
}
fn invalid() -> Error {
    Error::Media("invalid or misaligned HLS rendition")
}
fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread().enable_all().build()?)
}

pub(crate) struct Monitor {
    first_tier: Tier,
    end_ms: AtomicU32,
    bps: AtomicU64,
    memory: Arc<Semaphore>,
    position: watch::Sender<u32>,
    cancel: watch::Sender<()>,
}
impl Monitor {
    pub fn interrupt(&self) {
        self.cancel.send_replace(());
    }
    pub fn stats(&self, shown: Option<Tier>, position: f64) -> AdaptiveStats {
        AdaptiveStats {
            tier: shown.unwrap_or(self.first_tier),
            buffer_seconds: (f64::from(self.end_ms.load(Relaxed)) * 0.001 - position).max(0.0),
            throughput_bps: self.bps.load(Relaxed),
            buffer_bytes: BUFFER_BYTES - self.memory.available_permits(),
        }
    }
}

struct Format {
    codec: Codec,
    scale: u32,
    nal_length: usize,
}
struct Plan {
    conn: Arc<Connection>,
    file: i64,
    master: playlist::Master,
    cuts: Vec<u32>,
    audio: Vec<Arc<Format>>,
    video: Vec<OnceLock<Arc<Format>>>,
}

async fn resource(conn: &Connection, file: i64, name: &str) -> Result<Vec<u8>> {
    Ok(conn.hls(file, name).await?.0)
}
async fn timeline(conn: &Connection, file: i64, name: &str) -> Result<Vec<u32>> {
    playlist::media(&resource(conn, file, &format!("{name}.m3u8")).await?, name)
}
async fn format(
    conn: &Connection,
    file: i64,
    name: &str,
    id: usize,
    size: Option<[u32; 2]>,
) -> Result<(Track, Arc<Format>)> {
    let init = cmaf::read_init_segment(&resource(conn, file, &format!("{name}/init.mp4")).await?)?;
    let track = mp4_track(id, &init)?;
    let nal_length = match (size, init.codec()) {
        (Some(_), Some(CodecConfig::Avc(c))) => usize::from(c.length_size),
        (Some(_), Some(CodecConfig::Hevc(c))) => usize::from(c.length_size),
        (None, Some(CodecConfig::Aac(c)))
            if c.object_type == 2
                && c.channel_config == 2
                && c.sample_rate == init.timescale
                && (8000..=48000).contains(&c.sample_rate)
                && c.asc.len() == 2
                && c.asc[1] & 7 == 0 =>
        {
            0
        }
        _ => return Err(invalid()),
    };
    if let Some(size) = size
        && (track.kind != TrackKind::Video || init.video().map(|v| [v.width.into(), v.height.into()]) != Some(size))
    {
        return Err(invalid());
    }
    let fmt = Format { codec: track.codec.clone(), scale: init.timescale, nal_length };
    Ok((track, Arc::new(fmt)))
}

fn lan(addr: &str) -> bool {
    let host = flicks_client::net::host(addr);
    host.eq_ignore_ascii_case("localhost")
        || host.ends_with(".local")
        || match host.parse::<IpAddr>() {
            Ok(IpAddr::V4(v)) => v.is_private() || v.is_loopback() || v.is_link_local(),
            Ok(IpAddr::V6(v)) => v.is_unique_local() || v.is_loopback() || v.is_unicast_link_local(),
            _ => false,
        }
}

impl Plan {
    async fn open(conn: Arc<Connection>, file: i64, size: u64) -> Result<Option<(Self, Vec<Track>)>> {
        let bytes = resource(&conn, file, "master.m3u8").await?;
        let master = playlist::master(&bytes)?;
        if master.video.len() < 2 {
            return Ok(None);
        }
        let direct = master.video.last().ok_or_else(invalid)?;
        if lan(&conn.addr) && size > 0 {
            // A private address alone is not evidence of a fast link (VPNs, proxies, slow Wi-Fi).
            let n = size.min(256 << 10);
            let min_bps = direct.bps.saturating_mul(3).max(50_000_000);
            let fast = Duration::from_secs_f64(n as f64 * 8.0 / min_bps as f64);
            // A slow range need not finish just to tell us that it is slow.
            if let Ok(sample) = tokio::time::timeout(fast, conn.read(file, 0, n - 1)).await {
                if sample?.len() as u64 != n {
                    return Err(invalid());
                }
                return Ok(None);
            }
        }
        let first = master.video.first().ok_or_else(invalid)?;
        let cuts = timeline(&conn, file, &first.name).await?;
        let (video, first_video) = format(&conn, file, &first.name, 0, Some(first.size)).await?;
        let mut tracks = vec![video];
        let mut audio = Vec::new();
        for a in &master.audio {
            if timeline(&conn, file, &a.name).await? != cuts {
                return Err(invalid());
            }
            let (mut track, fmt) = format(&conn, file, &a.name, tracks.len(), None).await?;
            track.default = a.default;
            track.name = a.label.clone();
            track.language = a.language.clone();
            tracks.push(track);
            audio.push(fmt);
        }
        let video = (0..master.video.len()).map(|_| OnceLock::new()).collect::<Vec<_>>();
        video.first().ok_or_else(invalid)?.get_or_init(|| first_video);
        Ok(Some((Self { conn, file, master, cuts, audio, video }, tracks)))
    }

    async fn video(&self, choice: usize, i: usize) -> Result<(Fragment, Duration)> {
        let variant = self.master.video.get(choice).ok_or_else(invalid)?;
        let init = self.video.get(choice).ok_or_else(invalid)?;
        let fmt = if let Some(f) = init.get() {
            f
        } else {
            if timeline(&self.conn, self.file, &variant.name).await? != self.cuts {
                return Err(invalid());
            }
            let f = format(&self.conn, self.file, &variant.name, 0, Some(variant.size)).await?.1;
            init.get_or_init(|| f)
        };
        Fragment::fetch(self, &variant.name, i, fmt.clone()).await
    }
}

struct Fragment {
    bytes: Vec<u8>,
    samples: Vec<Sample>,
    format: Arc<Format>,
    cursor: usize,
}
impl Fragment {
    async fn fetch(plan: &Plan, name: &str, index: usize, format: Arc<Format>) -> Result<(Self, std::time::Duration)> {
        let (bytes, elapsed) = plan.conn.hls(plan.file, &format!("{name}/{index}.m4s")).await?;
        let samples = cmaf::read_media_segment(&bytes, u32::try_from(index + 1).map_err(|_| invalid())?)?;
        let first = samples.first().ok_or_else(invalid)?;
        let last = samples.last().ok_or_else(invalid)?;
        let span = last
            .dts
            .checked_add(last.duration.into())
            .and_then(|end| end.checked_sub(first.dts))
            .ok_or_else(invalid)?;
        if ns(span, format.scale) > 33_000_000_000
            || samples.iter().any(|s| {
                s.size > 16 << 20
                    || (format.nal_length == 0 && (s.size > 65536 || s.dts != s.pts || s.duration != 1024))
            })
        {
            return Err(invalid());
        }
        Ok((Self { bytes, samples, format, cursor: 0 }, elapsed))
    }
    fn cost(&self) -> usize {
        self.bytes.capacity() + self.samples.capacity() * std::mem::size_of::<Sample>()
    }
    fn first_pts(&self) -> Result<i64> {
        Ok(ns(self.samples.first().ok_or_else(invalid)?.pts, self.format.scale))
    }
    fn data(&self, s: Sample) -> Result<&[u8]> {
        let start = usize::try_from(s.offset).map_err(|_| invalid())?;
        self.bytes.get(start..start.checked_add(s.size as usize).ok_or_else(invalid)?).ok_or_else(invalid)
    }
    fn dts(&self) -> Option<i64> {
        self.samples.get(self.cursor).map(|s| ns(s.dts, self.format.scale))
    }
    fn next(&mut self, track: usize, origin: i64) -> Result<Packet> {
        let s = *self.samples.get(self.cursor).ok_or_else(invalid)?;
        self.cursor += 1;
        Ok(Packet {
            track,
            pts: ns(s.pts, self.format.scale).saturating_sub(origin),
            duration: ns(i64::from(s.duration), self.format.scale) as u64,
            discard_padding: 0,
            data: self.data(s)?.to_vec(),
        })
    }
    // A sync sample can be a CRA/recovery point, not IDR. Check the first VCL NAL, without
    // allocating an attacker-sized list. Refuse an invalid encode; don't enter open-GOP direct.
    fn idr(&self) -> Result<bool> {
        let s = *self.samples.first().ok_or_else(invalid)?;
        if !s.is_sync {
            return Err(invalid());
        }
        let mut data = self.data(s)?;
        let n = self.format.nal_length;
        for _ in 0..4096 {
            let prefix = data.get(..n).ok_or_else(invalid)?;
            let len = prefix.iter().fold(0usize, |l, &b| (l << 8) | usize::from(b));
            let end = n.checked_add(len).ok_or_else(invalid)?;
            let h = *data.get(n..end).and_then(|n| n.first()).ok_or_else(invalid)?;
            if h & 0x80 != 0 {
                return Err(invalid());
            }
            match &self.format.codec {
                Codec::Avc { .. } if (1..=5).contains(&(h & 31)) => return Ok(h & 31 == 5),
                Codec::Hevc { .. } if (h >> 1) & 63 < 32 => return Ok(matches!((h >> 1) & 63, 19 | 20)),
                _ => (),
            }
            data = data.get(end..).ok_or_else(invalid)?;
        }
        Err(invalid())
    }
    fn check_times(&self, origin: i64, start: i64, end: i64) -> Result<()> {
        // Allow quantization, audio frame rounding and decoder leading pictures, not a timestamp
        // that makes the audio clock jump hours or asks the worker to preroll indefinitely.
        if self.samples.iter().any(|s| {
            let pts = ns(s.pts, self.format.scale).saturating_sub(origin);
            !(start - 1_000_000_000..=end + 1_000_000_000).contains(&pts)
        }) {
            return Err(invalid());
        }
        Ok(())
    }
}

struct Cut {
    video: Fragment,
    audio: Option<(usize, Fragment)>,
    tier: Tier,
    origin: i64,
    _memory: OwnedSemaphorePermit,
}
impl Cut {
    fn next(&mut self, selected: &[usize]) -> Result<Option<Packet>> {
        let v = self.video.dts();
        if let Some((id, a)) = &mut self.audio
            && selected.contains(id)
            && a.dts().is_some_and(|t| v.is_none_or(|v| t < v))
        {
            return a.next(*id, self.origin).map(Some);
        }
        if v.is_some() {
            return self.video.next(0, self.origin).map(Some);
        }
        Ok(None)
    }
}

struct Stream {
    rx: mpsc::Receiver<Result<Cut>>,
    thread: Option<thread::JoinHandle<()>>,
    monitor: Arc<Monitor>,
}
impl Drop for Stream {
    fn drop(&mut self) {
        self.monitor.interrupt();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
impl Stream {
    fn start(
        plan: Arc<Plan>,
        monitor: Arc<Monitor>,
        index: usize,
        audio: Option<usize>,
        origin: Option<i64>,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let mut cancel = monitor.cancel.subscribe();
        let state = monitor.clone();
        let rt = runtime()?;
        let thread = thread::Builder::new().name("flicks-hls".into()).spawn(move || {
            rt.block_on(async {
                tokio::select! {
                    biased;
                    _ = cancel.changed() => (),
                    result = produce(&plan, &state, &tx, index, audio, origin) => {
                        if let Err(e) = result { let _ = tx.send(Err(e)); }
                    }
                }
            });
        })?;
        Ok(Self { rx, thread: Some(thread), monitor })
    }
}

async fn produce(
    plan: &Plan,
    monitor: &Monitor,
    tx: &mpsc::Sender<Result<Cut>>,
    index: usize,
    audio: Option<usize>,
    mut origin: Option<i64>,
) -> Result<()> {
    let variants = &plan.master.video;
    let mut abr = Abr::new(monitor.bps.load(Relaxed));
    let mut usable = variants.len();
    let mut next_audio_tick = None;
    let mut playback = monitor.position.subscribe();
    for (i, w) in plan.cuts.windows(2).enumerate().skip(index) {
        let ahead = loop {
            let ahead = w[0].saturating_sub(*playback.borrow_and_update());
            if ahead < TARGET_MS {
                break f64::from(ahead) / 1000.0;
            }
            playback.changed().await.map_err(|_| stopped())?;
        };
        let start = i64::from(w[0]) * 1_000_000;
        let end = i64::from(w[1]) * 1_000_000;
        let audio_fragment = if let Some(id) = audio {
            let j = id.checked_sub(1).ok_or_else(invalid)?;
            let a = plan.master.audio.get(j).ok_or_else(invalid)?;
            let fmt = plan.audio.get(j).ok_or_else(invalid)?.clone();
            let fragment = Fragment::fetch(plan, &a.name, i, fmt).await?.0;
            // Compare native AAC ticks, not rounded playlist durations. A missing or repeated
            // audio frame is an error, not a new clock anchor at a video switch.
            let first = fragment.samples.first().ok_or_else(invalid)?;
            if next_audio_tick.is_some_and(|tick| tick != first.dts) {
                return Err(invalid());
            }
            let last = fragment.samples.last().ok_or_else(invalid)?;
            next_audio_tick = last.dts.checked_add(last.duration.into());
            Some((id, fragment))
        } else {
            None
        };
        // Stay below a tier the server cannot produce (e.g. an oversize original cut), or an
        // open-GOP original we cannot switch into. Never downgrade around auth or corrupt data.
        let (video, tier, origin_ns) = loop {
            let available = variants.get(..usable).ok_or_else(invalid)?;
            let choice = abr.choose(available.iter().map(|v| v.bps), ahead);
            let variant = available.get(choice).ok_or_else(invalid)?;
            let (v, time) = match plan.video(choice, i).await {
                Err(Error::Client(flicks_client::Error::Status(404 | 422 | 503, _))) if choice > 0 => {
                    usable = choice;
                    continue;
                }
                other => other?,
            };
            abr.observe(v.bytes.len(), time);
            monitor.bps.store(abr.bps, Relaxed);
            let shift = match origin {
                Some(n) => n,
                None if i == 0 => match &audio_fragment {
                    // The stereo encoder uses the same presentation delay on an absolute AAC grid starting at zero.
                    Some((_, a)) => a.first_pts()?,
                    None => v.first_pts()?,
                },
                None => return Err(invalid()),
            };
            if !(0..=17_000_000_000).contains(&shift) {
                return Err(invalid());
            }
            v.check_times(shift, start, end)?;
            let idr = v.idr()?;
            if variant.tier == Tier::Direct {
                // Direct cuts are in decode order. If the *next* CRA carries pictures that
                // present before its cut, leaving this cut would silently lose those pictures.
                let last = v
                    .samples
                    .iter()
                    .map(|s| ns(s.pts.saturating_add(s.duration.into()), v.format.scale))
                    .max()
                    .ok_or_else(invalid)?;
                if !idr || (i + 2 < plan.cuts.len() && last - shift < end - 1_000_000) {
                    usable = usable.checked_sub(1).filter(|&n| n > 0).ok_or_else(invalid)?;
                    continue;
                }
            } else if !idr {
                return Err(invalid());
            }
            break (v, variant.tier, shift);
        };
        origin = Some(origin_ns);
        if let Some((_, a)) = &audio_fragment {
            a.check_times(origin_ns, start, end)?;
        }
        let cost = video.cost() + audio_fragment.as_ref().map_or(0, |(_, a)| a.cost());
        if cost > BUFFER_BYTES {
            return Err(Error::Media("HLS cut exceeds buffer cap"));
        }
        let memory = monitor.memory.clone().acquire_many_owned(cost as u32).await.map_err(|_| stopped())?;
        tx.send(Ok(Cut { video, audio: audio_fragment, tier, origin: origin_ns, _memory: memory }))
            .map_err(|_| stopped())?;
        monitor.end_ms.store(w[1], Relaxed);
    }
    Ok(())
}

pub(crate) struct Hls {
    plan: Arc<Plan>,
    pub monitor: Arc<Monitor>,
    stream: Option<Stream>,
    current: Option<Cut>,
    origin: Option<i64>,
    pub configured: Tier,
    change: Option<Arc<Format>>,
    next_index: usize,
}
impl Hls {
    pub fn open(connection: Arc<Connection>, file: i64, size: u64) -> Result<Option<(Self, Vec<Track>, f64)>> {
        // Blocking public API can also be called from inside another Tokio runtime.
        let opened = thread::Builder::new()
            .name("flicks-hls-open".into())
            .spawn(move || {
                runtime()?.block_on(async {
                    match tokio::time::timeout(OPEN_TIMEOUT, Plan::open(connection, file, size)).await {
                        // The HLS endpoint is optional. Authentication and malformed data remain errors.
                        Err(_) | Ok(Err(Error::Client(flicks_client::Error::Status(404 | 422 | 503, _)))) => Ok(None),
                        Ok(opened) => opened,
                    }
                })
            })?
            .join()
            .map_err(|_| stopped())??;
        let Some((plan, tracks)) = opened else {
            return Ok(None);
        };
        let configured = plan.master.video.first().ok_or_else(invalid)?.tier;
        let monitor = Arc::new(Monitor {
            first_tier: configured,
            end_ms: 0.into(),
            bps: 0.into(),
            memory: Arc::new(Semaphore::new(BUFFER_BYTES)),
            position: watch::channel(0).0,
            cancel: watch::channel(()).0,
        });
        let duration = f64::from(*plan.cuts.last().ok_or_else(invalid)?) * 0.001;
        let audio = tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Audio)
            .max_by_key(|t| (t.default, std::cmp::Reverse(t.id)))
            .map(|t| t.id);
        let plan = Arc::new(plan);
        let origin = plan.master.origin_ns;
        let stream = Stream::start(plan.clone(), monitor.clone(), 0, audio, origin)?;
        Ok(Some((
            Self {
                plan,
                monitor,
                stream: Some(stream),
                current: None,
                origin,
                configured,
                change: None,
                next_index: 0,
            },
            tracks,
            duration,
        )))
    }
    pub fn next(&mut self, selected: &[usize]) -> Result<Option<Packet>> {
        if let Some(c) = &mut self.current
            && let Some(p) = c.next(selected)?
        {
            return Ok(Some(p));
        }
        self.current = None; // release the byte permit *before* waiting for another cut
        if self.next_index + 1 >= self.plan.cuts.len() {
            return Ok(None);
        }
        let mut cut =
            self.stream.as_ref().ok_or_else(stopped)?.rx.recv_timeout(CUT_TIMEOUT).map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => Error::Media("HLS cut timed out"),
                mpsc::RecvTimeoutError::Disconnected => stopped(),
            })??;
        if cut.tier != self.configured {
            self.configured = cut.tier;
            self.change = Some(cut.video.format.clone());
        }
        self.origin = Some(cut.origin);
        let packet = cut.next(selected)?.ok_or_else(invalid)?;
        self.current = Some(cut);
        self.next_index += 1;
        Ok(Some(packet))
    }
    pub fn take_config(&mut self) -> Option<(Tier, Codec)> {
        self.change.take().map(|f| (self.configured, f.codec.clone()))
    }
    pub fn position(&self, seconds: f64) {
        self.monitor.position.send_replace((seconds * 1000.0) as u32);
    }
    pub fn seek(&mut self, ns: i64, audio: Option<usize>) -> Result<()> {
        self.stream.take();
        self.current = None;
        self.change = None;
        let ms = (ns.max(0) / 1_000_000) as u32;
        let index =
            self.plan.cuts.partition_point(|&p| p <= ms).saturating_sub(1).min(self.plan.cuts.len().saturating_sub(2));
        self.monitor.position.send_replace(ms);
        self.monitor.end_ms.store(ms, Relaxed);
        self.next_index = index;
        self.stream = Some(Stream::start(self.plan.clone(), self.monitor.clone(), index, audio, self.origin)?);
        Ok(())
    }
}
