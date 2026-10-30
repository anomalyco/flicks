//! Video in Rune's own pass: the playback thread queues frames in [`SLOT`] with the moment each
//! fell due on the audio clock, and the app's compositor imports (hardware) or uploads (software)
//! the one due by each display tick with `flicks-gpu` and draws it at the player's effect slot
//! (`El::effect(MARK)`), letterboxed, under the controls and subtitles. Ticks are read from
//! Rune's frame clock, which follows the display link, so 24 fps lands on an even pattern of
//! refreshes (5:5 at 120 Hz, 3:2 at 60 Hz) instead of on the playback thread's polling. The
//! compositor animates only while the player plays, so a paused or closed player draws nothing.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use flicks_player::VideoFrame;
use rune::{Compositor, Frame};

use flicks_ui::Flicks;
use flicks_ui::player::MARK;

/// How long after falling due a frame is shown: more than the playback thread takes to hand it
/// over (its 4 ms polling and scheduling), so every frame is there by the tick it belongs to.
const LAG: Duration = Duration::from_millis(8);
/// Frames waiting at most; the playback thread hands over one per video frame.
const QUEUED: usize = 8;

struct Queue {
    frames: VecDeque<(VideoFrame, Instant)>,
    /// The player closed: let the frame on screen go.
    clear: bool,
}

/// The hand-off between the playback thread and the compositor.
pub struct Slot {
    queue: Mutex<Queue>,
    playing: AtomicBool,
}

pub static SLOT: Slot =
    Slot { queue: Mutex::new(Queue { frames: VecDeque::new(), clear: false }), playing: AtomicBool::new(false) };

impl Slot {
    /// Queues `frame`, due at `due` (`None`: at once, as a frame shown while paused).
    pub fn show(&self, frame: VideoFrame, due: Option<Instant>) {
        let due = due.unwrap_or_else(|| Instant::now().checked_sub(LAG).unwrap_or_else(Instant::now));
        if let Ok(mut queue) = self.queue.lock() {
            if queue.frames.len() >= QUEUED {
                queue.frames.pop_front();
            }
            queue.frames.push_back((frame, due));
        }
    }

    pub fn clear(&self) {
        self.set_playing(false);
        if let Ok(mut queue) = self.queue.lock() {
            queue.frames.clear();
            queue.clear = true;
        }
    }

    pub fn set_playing(&self, playing: bool) {
        self.playing.store(playing, Ordering::Release);
    }

    /// The newest frame due by `tick`, dropping older ones; `Err` when the player closed.
    fn take(&self, tick: Instant) -> Result<Option<VideoFrame>, ()> {
        let Ok(mut queue) = self.queue.lock() else { return Ok(None) };
        if std::mem::take(&mut queue.clear) {
            return Err(());
        }
        let mut newest = None;
        while queue.frames.front().is_some_and(|(_, due)| *due + LAG <= tick) {
            newest = queue.frames.pop_front().map(|(frame, _)| frame);
        }
        Ok(newest)
    }
}

/// Display ticks as instants: Rune's clock (`Runtime::t`) steps by the display link's refresh
/// targets, so it is even where `Instant::now()` in `render` jitters with the frame's work.
#[derive(Default)]
struct Ticks {
    /// An instant and the clock reading at it.
    anchor: Option<(Instant, f64)>,
}

impl Ticks {
    /// Re-anchors after a rest (the clock doesn't count idle time) or when the two drift apart.
    fn at(&mut self, t: f64, now: Instant, animating: bool) -> Instant {
        let tick =
            self.anchor.and_then(|(at, t0)| Duration::try_from_secs_f64(t - t0).ok().and_then(|d| at.checked_add(d)));
        match tick {
            Some(tick) if animating && tick.max(now) - tick.min(now) < Duration::from_millis(4) => tick,
            _ => {
                self.anchor = Some((now, t));
                now
            }
        }
    }
}

#[derive(Default)]
pub struct Video {
    ticks: Ticks,
    /// Animating on the previous frame, so the clock ran through to this one.
    animating: bool,
    renderer: Option<flicks_gpu::Renderer>,
    /// The frame on screen and its visible size.
    frame: Option<(flicks_gpu::Frame, [u32; 2])>,
    /// The last error, printed once.
    error: Option<String>,
}

impl Video {
    fn take(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, tick: Instant) {
        let frame = match SLOT.take(tick) {
            Ok(None) => return,
            Err(()) => {
                self.frame = None;
                return;
            }
            Ok(Some(frame)) => frame,
        };
        let renderer = self.renderer.get_or_insert_with(|| flicks_gpu::Renderer::new(device));
        let size = frame.size();
        let gpu = match &frame {
            #[cfg(target_os = "macos")]
            VideoFrame::Hardware(f) => renderer.import(f.pixel_buffer(), frame.color(), frame.crop()),
            #[cfg(not(target_os = "macos"))]
            VideoFrame::Hardware(_) => return,
            VideoFrame::Software(f) => renderer.upload(queue, &f.as_frame()),
        };
        match gpu {
            Ok(gpu) => self.frame = Some((gpu, size)),
            Err(error) => self.report(error.to_string()),
        }
    }

    fn report(&mut self, error: String) {
        if self.error.as_ref() != Some(&error) {
            eprintln!("flicks: can't draw video: {error}");
            self.error = Some(error);
        }
    }
}

impl Compositor<Flicks> for Video {
    fn features(&self) -> wgpu::Features {
        flicks_gpu::P010_FEATURES
    }

    fn animating(&self) -> bool {
        SLOT.playing.load(Ordering::Acquire)
    }

    fn render(&mut self, f: &mut Frame<'_, Flicks>) {
        let tick = self.ticks.at(f.rt.t, Instant::now(), self.animating);
        self.animating = self.animating();
        self.take(f.device, f.queue, tick);
        let clear = f.rt.clear_color();
        f.ui.prepare(f.device, f.queue, &mut f.rt.text.atlas, &f.rt.instances, f.size);
        let marks = &f.rt.marks;
        let (renderer, frame) = (&mut self.renderer, &self.frame);
        let mut error = None;
        f.ui.draw_with(f.encoder, f.target, clear, &mut |pass, site| {
            let (Some(renderer), Some((frame, size))) = (renderer.as_mut(), frame) else { return };
            if marks.get(site.index as usize) != Some(&MARK) {
                return;
            }
            let [x, y, w, h] = site.instance.rect;
            let target = flicks_gpu::Target {
                format: site.format,
                size: site.size,
                rect: fit(*size, [x - site.origin[0], y - site.origin[1], w, h]),
                srgb_blend: site.srgb_blend,
            };
            match renderer.prepare(frame, target) {
                Ok(draw) => draw.record(pass),
                Err(e) => error = Some(e.to_string()),
            }
        });
        if let Some(error) = error {
            self.report(error);
        }
    }
}

/// The largest rectangle of the video's shape centred in `rect` (x, y, width, height).
fn fit(video: [u32; 2], rect: [f32; 4]) -> [f32; 4] {
    let [x, y, w, h] = rect;
    let (vw, vh) = (video[0].max(1) as f32, video[1].max(1) as f32);
    let scale = (w / vw).min(h / vh);
    let (fw, fh) = ((vw * scale).round().max(1.0), (vh * scale).round().max(1.0));
    [(x + (w - fw) / 2.0).round(), (y + (h - fh) / 2.0).round(), fw, fh]
}
