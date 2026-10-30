//! Blocking playback engine. Drive from a playback thread; hand owned frames to the UI.
//! Timestamps and positions are seconds; the caller's monotonic `Instant` drives silent
//! video and the headless sink. Device playback uses the already latency-adjusted audio clock.
#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod audio;
mod demux;
mod source;
mod video;

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
}

/// The null sink still decodes every sample. Its clock advances only through queued audio,
/// exactly like an underrunning device, using `now` passed to `next_frame_for_display`.
pub enum AudioSink {
    #[cfg(target_os = "macos")]
    Device(Box<flicks_audio::Output>),
    Null,
}

pub struct Player {
    demux: demux::Demux,
    tracks: Vec<Track>,
    video_track: usize,
    audio_track: Option<usize>,
    subtitle_track: Option<usize>,
    video: video::Decoder,
    audio: Option<audio::Decoder>,
    sink: AudioSink,
    frames: VecDeque<VideoFrame>,
    audio_pending: Vec<audio::Chunk>,
    subtitles: flicks_subtitle::Track,
    sidecar: Option<flicks_subtitle::Track>,
    duration: f64,
    position: f64,
    audio_end: f64,
    video_end: f64,
    trim_start: f64,
    anchor: Option<(Instant, f64)>,
    state: State,
    running: bool,
    eof: bool,
    software: bool,
    stats: Stats,
}

impl Player {
    /// Opens paused. Does not scan the media or decode the whole file.
    pub fn open(source: Source) -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            Self::with_audio(source, AudioSink::Device(Box::new(flicks_audio::Output::open()?)))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self::with_audio(source, AudioSink::Null)
        }
    }

    pub fn with_audio(source: Source, sink: AudioSink) -> Result<Self> {
        let (demux, tracks, duration) = demux::Demux::open(source)?;
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
            audio,
            sink,
            frames: VecDeque::new(),
            audio_pending: Vec::new(),
            subtitles: Default::default(),
            sidecar: None,
            duration,
            position: 0.0,
            audio_end: 0.0,
            video_end: 0.0,
            trim_start: 0.0,
            anchor: None,
            state: State::Paused,
            running: false,
            eof: false,
            software: false,
            stats: Stats::default(),
        })
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
        self.stats
    }
    pub fn is_hardware(&self) -> bool {
        self.video.hardware()
    }

    pub fn play(&mut self) {
        if !self.running && !matches!(self.state, State::Failed | State::Ended) {
            self.anchor = None;
            self.running = true;
            self.state = State::Buffering;
        }
    }

    /// Stop queued audio immediately. Resume re-decodes from the index at the frozen time.
    pub fn pause(&mut self) -> Result<()> {
        self.running = false;
        self.seek(self.position)
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
        self.clear_audio();
        let subtitle =
            self.subtitle_track.map(|id| self.demux.subtitle_at(id, (target * 1e9) as i64)).transpose()?.flatten();
        self.demux.seek(self.video_track, (target * 1e9) as i64)?;
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
        self.anchor = None;
        self.eof = false;
        self.state = if self.running { State::Buffering } else { State::Paused };
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

    /// Decode a bounded look-ahead and release at most one due frame. Call at display cadence.
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
        self.fill()?;
        if self.running {
            for chunk in self.audio_pending.drain(..) {
                match &mut self.sink {
                    #[cfg(target_os = "macos")]
                    AudioSink::Device(output) => output.push(chunk.pts, chunk.rate, chunk.layout, &chunk.samples)?,
                    AudioSink::Null => {}
                }
            }
            self.anchor.get_or_insert((now, self.position));
            self.state = State::Playing;
            self.update_clock(now);
        }
        let mut result = None;
        while self.frames.front().is_some_and(|f| f.pts() <= self.position + 0.001) {
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
            self.stats.displayed += 1;
            self.stats.av_offset = frame.pts() - self.position;
        }
        if self.eof && self.frames.is_empty() && self.position + 0.002 >= self.audio_end.max(self.video_end) {
            self.state = State::Ended;
            self.running = false;
        }
        Ok(result)
    }

    fn fill(&mut self) -> Result<()> {
        let horizon = self.position + 0.35;
        let selected: Vec<_> =
            std::iter::once(self.video_track).chain(self.audio_track).chain(self.subtitle_track).collect();
        for _ in 0..100_000 {
            if self.eof
                || (self.frames.back().is_some_and(|f| f.pts() >= horizon)
                    && (self.audio_track.is_none() || self.audio_end >= horizon))
            {
                return Ok(());
            }
            // Bound interleaving, including malicious files with no audio until hours later.
            if self.frames.len() >= 64 || self.audio_pending.iter().map(|c| c.samples.len()).sum::<usize>() > 2_000_000
            {
                return Err(Error::Media("interleaving exceeds playback buffer"));
            }
            let packet = self.demux.next(&selected)?;
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
                                self.audio_pending.push(chunk);
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
            self.stats.decoded += frames.len() as u64;
            // Decode preroll, but do not count it as dropped display frames.
            self.frames.extend(frames.into_iter().filter(|f| f.pts() + 0.001 >= self.trim_start));
        }
        Err(Error::Media("decode preroll exceeds limit"))
    }

    fn fallback(&mut self) -> Result<()> {
        self.software = true;
        self.stats.hardware_fallbacks += 1;
        self.restart(self.position)
    }

    fn update_clock(&mut self, now: Instant) {
        if !self.running {
            return;
        }
        let Some((at, start)) = self.anchor else { return };
        let wall = start + now.saturating_duration_since(at).as_secs_f64();
        let position = if self.audio_track.is_some() {
            match &self.sink {
                #[cfg(target_os = "macos")]
                // Output::position already subtracts CoreAudio's playback latency. Subtracting
                // Output::latency again would put video behind the sound by two device buffers.
                AudioSink::Device(output) => output.position().unwrap_or(self.position),
                AudioSink::Null => wall.min(self.audio_end),
            }
        } else {
            wall.min(self.video_end)
        };
        self.position = self.position.max(position);
    }

    fn clear_audio(&mut self) {
        #[cfg(target_os = "macos")]
        if let AudioSink::Device(output) = &mut self.sink {
            output.clear();
        }
    }
    fn fail(&mut self) {
        self.clear_audio();
        self.running = false;
        self.state = State::Failed;
    }
}

#[cfg(test)]
mod tests;
