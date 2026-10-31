//! Playback engine with bounded background decode and nonblocking frame polling.
//! Opening, seeking and track changes block; keep those off the display thread.
//! Timestamps and positions are seconds; the caller's monotonic `Instant` drives silent
//! video and the headless sink. Device playback uses the already latency-adjusted audio clock.
#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod abr;
mod audio;
mod demux;
mod hls;
mod source;
mod video;
mod worker;

pub use abr::{AdaptiveStats, Tier};
use flicks_container::matroska::Codec;
pub use flicks_subtitle::Cue;
pub use source::Source;
use std::collections::VecDeque;
use std::time::Instant;
pub use video::{CpuVideo, VideoFrame};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Client(#[from] flicks_client::Error),
    #[error(transparent)]
    Mkv(#[from] flicks_container::matroska::Error),
    #[error(transparent)]
    Mp4(#[from] flicks_container::isobmff::Error),
    #[error(transparent)]
    Audio(#[from] flicks_audio::Error),
    #[error("media: {0}")]
    Media(&'static str),
    #[error("decode: {0}")]
    Decode(String),
    #[error("invalid or unsupported track")]
    Track,
}
impl Error {
    fn decode(error: impl std::fmt::Display) -> Self {
        Self::Decode(error.to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
    Other,
}

#[derive(Clone, Debug)]
pub struct Track {
    pub id: usize,
    pub kind: TrackKind,
    pub codec: Codec,
    pub language: String,
    pub name: String,
    pub default: bool,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Paused,
    Buffering,
    Playing,
    Ended,
    Failed,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub decoded: u64,
    pub displayed: u64,
    pub dropped: u64,
    /// Displayed frame PTS minus the audio clock, seconds (updated on each display).
    pub av_offset: f64,
    pub hardware_fallbacks: u64,
    /// None for local files, downloads and byte-range playback.
    pub adaptive: Option<AdaptiveStats>,
}

/// The null sink still decodes every sample. Its clock advances only through queued audio,
/// exactly like an underrunning device, using `now` passed to `next_frame_for_display`.
pub enum AudioSink {
    Device(Box<flicks_audio::Output>),
    Null,
}

// Tag at decode, not at download or from a guessed resolution. Delayed pictures flushed
// from the old decoder may be presented after the next rendition has already started decode.
struct QueuedFrame {
    video: VideoFrame,
    tier: Option<Tier>,
}

pub struct Player {
    worker: worker::Worker,
    tracks: Vec<Track>,
    audio_track: Option<usize>,
    subtitle_track: Option<usize>,
    hardware: bool,
    sink: AudioSink,
    frames: VecDeque<QueuedFrame>,
    audio_pending: VecDeque<audio::Chunk>,
    subtitles: flicks_subtitle::Track,
    sidecar: Option<flicks_subtitle::Track>,
    duration: f64,
    position: f64,
    audio_end: f64,
    video_end: f64,
    video_ready: f64,
    audio_finished: bool,
    anchor: Option<(Instant, f64)>,
    tail_anchor: Option<(Instant, f64)>,
    state: State,
    running: bool,
    eof: bool,
    stats: Stats,
    shown_tier: Option<Tier>,
}

impl Player {
    /// Opens paused. Does not scan the media or decode the whole file.
    pub fn open(source: Source) -> Result<Self> {
        Self::with_output(source, flicks_audio::Output::open())
    }

    fn with_output(
        source: Source,
        output: std::result::Result<flicks_audio::Output, flicks_audio::Error>,
    ) -> Result<Self> {
        let sink = match output {
            Ok(output) => AudioSink::Device(Box::new(output)),
            Err(flicks_audio::Error::NoDevice) => AudioSink::Null,
            Err(e) => return Err(e.into()),
        };
        Self::with_audio(source, sink)
    }

    pub fn with_audio(source: Source, sink: AudioSink) -> Result<Self> {
        if let AudioSink::Device(output) = &sink {
            output.pause()?;
        }
        Self::from_decoder(worker::Decoder::open(source)?, sink)
    }

    fn from_decoder(decoder: worker::Decoder, sink: AudioSink) -> Result<Self> {
        let mut player = Self {
            tracks: decoder.tracks.clone(),
            audio_track: decoder.audio_track,
            subtitle_track: decoder.subtitle_track,
            duration: decoder.duration,
            hardware: decoder.video.hardware(),
            worker: worker::Worker::new(decoder)?,
            sink,
            frames: VecDeque::new(),
            audio_pending: VecDeque::new(),
            subtitles: Default::default(),
            sidecar: None,
            position: 0.0,
            audio_end: 0.0,
            video_end: 0.0,
            video_ready: 0.0,
            audio_finished: false,
            anchor: None,
            tail_anchor: None,
            state: State::Paused,
            running: false,
            eof: false,
            stats: Stats::default(),
            shown_tier: None,
        };
        player.worker.request(worker::Command::Fill(0.0))?;
        let batch = player.worker.wait()?;
        player.accept(batch)?;
        Ok(player)
    }

    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }
    pub fn selected_audio(&self) -> Option<usize> {
        self.audio_track
    }
    pub fn selected_subtitle(&self) -> Option<usize> {
        self.subtitle_track
    }
    pub fn position(&self) -> f64 {
        self.position
    }
    pub fn duration(&self) -> f64 {
        self.duration
    }
    pub fn state(&self) -> State {
        self.state
    }
    pub fn stats(&self) -> Stats {
        Stats { adaptive: self.worker.hls.as_ref().map(|m| m.stats(self.shown_tier, self.position)), ..self.stats }
    }
    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    pub fn play(&mut self) {
        if !self.running && !matches!(self.state, State::Failed | State::Ended) {
            self.anchor = None;
            self.tail_anchor = None;
            self.running = true;
            self.state = State::Buffering;
        }
    }

    /// Freeze the audio clock and keep queued frames for a gapless resume.
    pub fn pause(&mut self) -> Result<()> {
        if let AudioSink::Device(output) = &self.sink {
            output.pause()?;
        }
        self.update_clock(Instant::now());
        self.running = false;
        self.state = State::Paused;
        Ok(())
    }

    pub fn seek(&mut self, seconds: f64) -> Result<()> {
        if !seconds.is_finite() || seconds < 0.0 || seconds > i64::MAX as f64 / 1e9 {
            return Err(Error::Media("invalid seek time"));
        }
        let target = if self.duration > 0.0 { seconds.min(self.duration) } else { seconds };
        let result = self.restart(target);
        if result.is_err() {
            self.fail();
        }
        result
    }

    fn restart(&mut self, target: f64) -> Result<()> {
        if let AudioSink::Device(output) = &self.sink {
            output.pause()?;
        }
        self.clear_audio();
        if let Some(hls) = &self.worker.hls {
            hls.interrupt();
        }
        if self.worker.pending {
            // A seek abandons the old region, including its decode/read failure.
            // A stopped worker is reported by the new request below.
            let _ = self.worker.wait();
        }
        self.worker.request(worker::Command::Seek {
            target,
            audio: self.audio_track,
            subtitle: self.subtitle_track,
        })?;
        self.frames.clear();
        self.audio_pending.clear();
        self.subtitles = Default::default();
        self.position = target;
        self.shown_tier = None;
        self.video_end = target;
        self.video_ready = target;
        self.audio_finished = false;
        self.audio_end = target;
        self.anchor = None;
        self.tail_anchor = None;
        self.eof = false;
        self.state = if self.running { State::Buffering } else { State::Paused };
        let batch = self.worker.wait()?;
        self.accept(batch)?;
        Ok(())
    }

    pub fn select_audio(&mut self, id: Option<usize>) -> Result<()> {
        if let Some(id) = id {
            let t = self.tracks.get(id).filter(|t| t.enabled && t.kind == TrackKind::Audio).ok_or(Error::Track)?;
            audio::Decoder::new(&t.codec)?;
        }
        self.audio_track = id;
        self.seek(self.position)
    }

    pub fn select_subtitle(&mut self, id: Option<usize>) -> Result<()> {
        if let Some(id) = id {
            self.tracks
                .get(id)
                .filter(|t| {
                    t.enabled && t.kind == TrackKind::Subtitle && matches!(t.codec, Codec::SubRip | Codec::WebVtt)
                })
                .ok_or(Error::Track)?;
        }
        self.sidecar = None;
        self.subtitle_track = id;
        self.seek(self.position)
    }

    /// Parsed SRT/WebVTT sidecars don't require a media rescan and remain active after seeks.
    pub fn set_subtitles(&mut self, track: Option<flicks_subtitle::Track>) {
        self.sidecar = track;
    }
    pub fn active_cues(&self) -> Vec<&Cue> {
        self.sidecar.as_ref().unwrap_or(&self.subtitles).active_at((self.position.max(0.0) * 1000.0) as u64)
    }

    /// Poll the decode worker and release at most one due frame, without waiting for I/O or decode.
    /// Returns the first frame while paused (for opening/seeking previews), then holds it.
    pub fn next_frame_for_display(&mut self, now: Instant) -> Result<Option<VideoFrame>> {
        if matches!(self.state, State::Failed | State::Ended) {
            return Ok(None);
        }
        let result = self.advance(now);
        if result.is_err() {
            self.fail();
        }
        result
    }

    fn advance(&mut self, now: Instant) -> Result<Option<VideoFrame>> {
        self.update_clock(now);
        if let Some(batch) = self.worker.poll()? {
            self.accept(batch)?;
        }
        if !self.worker.pending
            && !self.eof
            && (self.frames.back().is_none_or(|f| f.video.pts() < self.position + 0.2)
                || (!self.audio_finished && self.audio_end < self.position + 0.2))
        {
            self.worker.request(worker::Command::Fill(self.position))?;
        }
        if self.running {
            let stalled = !self.eof
                && (self.frames.is_empty() || (!self.audio_finished && self.position + 0.000_001 >= self.audio_end));
            if stalled {
                self.state = State::Buffering;
                self.anchor = None;
                self.tail_anchor = None;
                return Ok(None);
            }
            match &mut self.sink {
                AudioSink::Device(output) => {
                    while output.queued().as_secs_f64() < 0.3 {
                        let Some(chunk) = self.audio_pending.pop_front() else { break };
                        output.push(chunk.pts, chunk.rate, chunk.layout, &chunk.samples)?;
                    }
                    if self.state == State::Buffering {
                        output.play()?;
                    }
                }
                AudioSink::Null => self.audio_pending.clear(),
            }
            self.anchor.get_or_insert((now, self.position));
            self.state = State::Playing;
            self.update_clock(now);
        }
        let mut result = None;
        while self.frames.front().is_some_and(|f| f.video.pts() <= self.position + 0.001) {
            let frame = self.frames.pop_front().ok_or(Error::Media("missing queued frame"))?;
            if result.is_some() {
                self.stats.dropped += 1;
            }
            result = Some(frame);
        }
        if !self.running && result.is_none() && self.anchor.is_none() {
            result = self.frames.pop_front();
            self.anchor = Some((now, self.position));
        }
        if let Some(frame) = &result {
            self.shown_tier = frame.tier;
            self.stats.displayed += 1;
            self.stats.av_offset = frame.video.pts() - self.position;
        }
        if self.eof && self.frames.is_empty() && self.position + 0.002 >= self.audio_end.max(self.video_end) {
            self.state = State::Ended;
            self.running = false;
        }
        Ok(result.map(|f| f.video))
    }

    fn accept(&mut self, batch: worker::Batch) -> Result<()> {
        if batch.hardware_fallbacks != self.stats.hardware_fallbacks {
            self.frames.clear();
            self.audio_pending.clear();
            self.clear_audio();
            self.anchor = None;
            self.tail_anchor = None;
        }
        if self.frames.len() + batch.frames.len() > 64
            || self.audio_pending.iter().chain(&batch.audio).map(|c| c.samples.len()).sum::<usize>() > 2_000_000
        {
            return Err(Error::Media("interleaving exceeds playback buffer"));
        }
        if let Some(frame) = batch.frames.back() {
            self.video_ready = frame.video.pts();
        }
        self.frames.extend(batch.frames);
        self.audio_pending.extend(batch.audio);
        self.subtitles = batch.subtitles;
        self.audio_end = batch.audio_end;
        self.video_end = batch.video_end;
        self.eof = batch.eof;
        self.audio_finished = batch.audio_finished;
        self.hardware = batch.hardware;
        self.stats.decoded = batch.decoded;
        self.stats.hardware_fallbacks = batch.hardware_fallbacks;
        Ok(())
    }

    #[cfg(test)]
    fn fill(&mut self) -> Result<()> {
        if self.worker.pending {
            let batch = self.worker.wait()?;
            self.accept(batch)?;
        }
        Ok(())
    }

    fn update_clock(&mut self, now: Instant) {
        if !self.running {
            return;
        }
        let Some((at, start)) = self.anchor else { return };
        let wall = start + now.saturating_duration_since(at).as_secs_f64();
        let video_limit = if self.eof { self.video_end.max(self.audio_end) } else { self.video_ready };
        let position = if self.audio_track.is_some() {
            match &self.sink {
                // Output::position already subtracts device latency.
                AudioSink::Device(output) => output.position().unwrap_or(self.position),
                AudioSink::Null => wall.min(self.audio_end).min(video_limit),
            }
        } else {
            wall.min(video_limit)
        };
        self.position = self.position.max(position);
        // In particular, re-anchor before accepting a refill in this same display
        // tick. Otherwise a long source stall becomes a burst of dropped frames.
        if wall > self.position && (self.audio_track.is_none() || matches!(self.sink, AudioSink::Null)) {
            self.anchor = Some((now, self.position));
        }
        if self.audio_finished && self.audio_pending.is_empty() && self.position + 0.000_001 >= self.audio_end {
            let (at, start) = *self.tail_anchor.get_or_insert((now, self.position));
            self.position =
                self.position.max((start + now.saturating_duration_since(at).as_secs_f64()).min(video_limit));
            if self.position >= video_limit {
                self.tail_anchor = Some((now, self.position));
            }
        }
    }

    fn clear_audio(&mut self) {
        if let AudioSink::Device(output) = &mut self.sink {
            output.clear();
        }
    }
    fn fail(&mut self) {
        if let Some(hls) = &self.worker.hls {
            hls.interrupt();
        }
        self.clear_audio();
        self.running = false;
        self.state = State::Failed;
    }
}

impl worker::Decoder {
    fn open(source: Source) -> Result<Self> {
        let (demux, tracks, duration) = demux::Demux::open(source)?;
        let tier = if let demux::Demux::Hls(hls) = &demux { Some(hls.configured) } else { None };
        let choose = |kind| {
            tracks
                .iter()
                .filter(|t| t.enabled && t.kind == kind)
                .max_by_key(|t| (t.default, std::cmp::Reverse(t.id)))
                .map(|t| t.id)
        };
        let video_track = choose(TrackKind::Video).ok_or(Error::Media("no video track"))?;
        let audio_track = choose(TrackKind::Audio);
        let subtitle_track = choose(TrackKind::Subtitle).filter(|&id| {
            tracks.get(id).is_some_and(|t| t.default && matches!(t.codec, Codec::SubRip | Codec::WebVtt))
        });
        let video = video::Decoder::new(&tracks.get(video_track).ok_or(Error::Track)?.codec, false)?;
        let audio =
            audio_track.map(|id| audio::Decoder::new(&tracks.get(id).ok_or(Error::Track)?.codec)).transpose()?;
        Ok(Self {
            demux,
            tracks,
            video_track,
            audio_track,
            subtitle_track,
            video,
            tier,
            audio,
            frames: VecDeque::new(),
            audio_pending: VecDeque::new(),
            subtitles: Default::default(),
            duration,
            position: 0.0,
            audio_end: 0.0,
            video_end: 0.0,
            trim_start: 0.0,
            eof: false,
            software: false,
            decoded: 0,
            hardware_fallbacks: 0,
        })
    }
    fn restart(&mut self, target: f64) -> Result<()> {
        let subtitle =
            self.subtitle_track.map(|id| self.demux.subtitle_at(id, (target * 1e9) as i64)).transpose()?.flatten();
        self.demux.seek(self.video_track, (target * 1e9) as i64, self.audio_track)?;
        self.video = video::Decoder::new(&self.tracks.get(self.video_track).ok_or(Error::Track)?.codec, self.software)?;
        self.audio = self
            .audio_track
            .map(|id| audio::Decoder::new(&self.tracks.get(id).ok_or(Error::Track)?.codec))
            .transpose()?;
        self.frames.clear();
        self.audio_pending.clear();
        self.subtitles = Default::default();
        if let Some(p) = subtitle
            && let Some(cue) = Cue::from_payload(
                p.pts.max(0) as u64 / 1_000_000,
                (p.pts.max(0) as u64).saturating_add(p.duration) / 1_000_000,
                &p.data,
            )
        {
            self.subtitles = flicks_subtitle::Track::new(vec![cue]);
        }
        self.position = target;
        self.trim_start = target;
        self.video_end = target;
        self.audio_end = target;
        self.eof = false;
        Ok(())
    }

    fn fill(&mut self) -> Result<()> {
        if let demux::Demux::Hls(hls) = &self.demux {
            hls.position(self.position);
        }
        let horizon = self.position + 0.35;
        let selected: Vec<_> =
            std::iter::once(self.video_track).chain(self.audio_track).chain(self.subtitle_track).collect();
        for _ in 0..100_000 {
            if self.eof
                || (self.frames.back().is_some_and(|f| f.video.pts() >= horizon)
                    && (self.audio_track.is_none_or(|id| self.demux.exhausted(id)) || self.audio_end >= horizon))
            {
                return Ok(());
            }
            // Bound interleaving, including malicious files with no audio until hours later.
            if self.frames.len() >= 64 || self.audio_pending.iter().map(|c| c.samples.len()).sum::<usize>() > 2_000_000
            {
                return Err(Error::Media("interleaving exceeds playback buffer"));
            }
            let packet = self.demux.next(&selected)?;
            if let demux::Demux::Hls(hls) = &mut self.demux
                && let Some((tier, codec)) = hls.take_config()
            {
                // Finish delayed B pictures from the old decoder *before* submitting the new IDR.
                // Leave queued video, the audio decoder, its clock and device output untouched.
                let delayed = self.video.decode(None)?;
                self.keep_frames(delayed);
                self.video = video::Decoder::new(&codec, self.software)?;
                self.tier = Some(tier);
                self.tracks.get_mut(self.video_track).ok_or(Error::Track)?.codec = codec;
            }
            let frames =
                match packet {
                    None => {
                        let result = self.video.decode(None);
                        match result {
                            Ok(frames) => {
                                self.eof = true;
                                frames
                            }
                            Err(_) if self.video.hardware() => {
                                self.fallback()?;
                                continue;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    Some(p) if p.track == self.video_track => {
                        self.video_end = self.video_end.max((p.pts as f64 + p.duration as f64) / 1e9);
                        match self.video.decode(Some((&p.data, p.pts))) {
                            Ok(frames) => frames,
                            Err(_) if self.video.hardware() => {
                                self.fallback()?;
                                continue;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    Some(p) if Some(p.track) == self.audio_track => {
                        if let Some(d) = &mut self.audio {
                            let mut chunk = d.decode(&p.data, p.pts as f64 / 1e9)?;
                            let start =
                                self.trim_start.max(chunk.pts + p.discard_padding.saturating_neg().max(0) as f64 / 1e9);
                            let end = (chunk.end() - p.discard_padding.max(0) as f64 / 1e9)
                                .min(if self.duration > 0.0 { self.duration } else { f64::INFINITY });
                            chunk.trim(start, end);
                            if !chunk.samples.is_empty() {
                                self.audio_end = self.audio_end.max(chunk.end());
                                self.audio_pending.push_back(chunk);
                            }
                        }
                        Vec::new()
                    }
                    Some(p) => {
                        if let Some(cue) = Cue::from_payload(
                            p.pts.max(0) as u64 / 1_000_000,
                            (p.pts.max(0) as u64).saturating_add(p.duration) / 1_000_000,
                            &p.data,
                        ) {
                            let mut cues: Vec<_> = self
                                .subtitles
                                .cues()
                                .iter()
                                .filter(|c| c.end_ms > (self.position * 1000.0) as u64)
                                .cloned()
                                .collect();
                            if cues.len() >= 1024 {
                                return Err(Error::Media("too many overlapping subtitles"));
                            }
                            if !cues.contains(&cue) {
                                cues.push(cue);
                            }
                            self.subtitles = flicks_subtitle::Track::new(cues);
                        }
                        Vec::new()
                    }
                };
            self.keep_frames(frames);
        }
        Err(Error::Media("decode preroll exceeds limit"))
    }

    fn keep_frames(&mut self, frames: Vec<VideoFrame>) {
        self.decoded += frames.len() as u64;
        // Decode preroll, but do not count it as dropped display frames.
        self.frames.extend(
            frames
                .into_iter()
                .filter(|f| f.pts() + 0.001 >= self.trim_start)
                .map(|video| QueuedFrame { video, tier: self.tier }),
        );
    }

    fn fallback(&mut self) -> Result<()> {
        self.software = true;
        self.hardware_fallbacks += 1;
        self.restart(self.position)
    }
}

#[cfg(test)]
mod tests;
