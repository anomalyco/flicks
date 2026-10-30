use crate::{Error, Result, Track, VideoFrame, audio, demux, video};
use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::JoinHandle;

pub(crate) struct Decoder {
    pub demux: demux::Demux,
    pub tracks: Vec<Track>,
    pub video_track: usize,
    pub audio_track: Option<usize>,
    pub subtitle_track: Option<usize>,
    pub video: video::Decoder,
    pub audio: Option<audio::Decoder>,
    pub frames: VecDeque<VideoFrame>,
    pub audio_pending: VecDeque<audio::Chunk>,
    pub subtitles: flicks_subtitle::Track,
    pub duration: f64,
    pub position: f64,
    pub audio_end: f64,
    pub video_end: f64,
    pub trim_start: f64,
    pub eof: bool,
    pub software: bool,
    pub decoded: u64,
    pub hardware_fallbacks: u64,
}

pub(crate) struct Batch {
    pub frames: VecDeque<VideoFrame>,
    pub audio: VecDeque<audio::Chunk>,
    pub subtitles: flicks_subtitle::Track,
    pub audio_end: f64,
    pub video_end: f64,
    pub eof: bool,
    pub hardware: bool,
    pub audio_finished: bool,
    pub decoded: u64,
    pub hardware_fallbacks: u64,
}

pub(crate) enum Command {
    Fill(f64),
    Seek { target: f64, audio: Option<usize>, subtitle: Option<usize> },
}

/// One request and one bounded batch in flight. The presentation thread never waits
/// for this worker except during explicitly blocking open/seek/track changes.
pub(crate) struct Worker {
    sender: Option<SyncSender<Command>>,
    receiver: Receiver<Result<Batch>>,
    thread: Option<JoinHandle<()>>,
    pub pending: bool,
}

impl Worker {
    pub fn new(mut decoder: Decoder) -> Result<Self> {
        let (sender, commands) = mpsc::sync_channel(1);
        let (results, receiver) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new().name("flicks-decode".into()).spawn(move || {
            while let Ok(command) = commands.recv() {
                let result = (|| {
                    match command {
                        Command::Fill(position) => decoder.position = position,
                        Command::Seek { target, audio, subtitle } => {
                            decoder.audio_track = audio;
                            decoder.subtitle_track = subtitle;
                            decoder.restart(target)?;
                        }
                    }
                    decoder.fill()?;
                    Ok(Batch {
                        frames: std::mem::take(&mut decoder.frames),
                        audio: std::mem::take(&mut decoder.audio_pending),
                        subtitles: decoder.subtitles.clone(),
                        audio_end: decoder.audio_end,
                        video_end: decoder.video_end,
                        eof: decoder.eof,
                        hardware: decoder.video.hardware(),
                        audio_finished: decoder.eof || decoder.audio_track.is_none_or(|id| decoder.demux.exhausted(id)),
                        decoded: decoder.decoded,
                        hardware_fallbacks: decoder.hardware_fallbacks,
                    })
                })();
                if results.send(result).is_err() {
                    break;
                }
            }
        })?;
        Ok(Self { sender: Some(sender), receiver, thread: Some(thread), pending: false })
    }

    pub fn request(&mut self, command: Command) -> Result<()> {
        self.sender
            .as_ref()
            .ok_or(Error::Media("decode worker stopped"))?
            .try_send(command)
            .map_err(|_| Error::Media("decode worker unavailable"))?;
        self.pending = true;
        Ok(())
    }

    pub fn poll(&mut self) -> Result<Option<Batch>> {
        if !self.pending {
            return Ok(None);
        }
        match self.receiver.try_recv() {
            Ok(result) => {
                self.pending = false;
                result.map(Some)
            }
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(Error::Media("decode worker stopped")),
        }
    }

    pub fn wait(&mut self) -> Result<Batch> {
        let result = self.receiver.recv().map_err(|_| Error::Media("decode worker stopped"))?;
        self.pending = false;
        result
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.sender.take();
        // At most one response exists; draining it lets the worker close normally.
        if self.pending {
            let _ = self.receiver.recv();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
