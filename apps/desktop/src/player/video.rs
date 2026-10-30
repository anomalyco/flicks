//! Video in Rune's own pass: the playback thread leaves its newest frame in [`SLOT`], and the
//! app's compositor imports it (hardware) or uploads it (software) with `flicks-gpu` and draws it
//! at the player's effect slot (`El::effect(MARK)`), letterboxed, under the controls and
//! subtitles. The compositor animates only while the player plays, so a paused or closed player
//! draws no frames.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use flicks_player::VideoFrame;
use rune::{Compositor, Frame};

use crate::Flicks;

/// The player's effect slot (`El::effect`).
pub const MARK: u32 = 0x464c_4b56;

enum Next {
    Frame(VideoFrame),
    /// The player closed: let the last frame go.
    Clear,
}

/// The hand-off between the playback thread and the compositor.
pub struct Slot {
    next: Mutex<Option<Next>>,
    playing: AtomicBool,
}

pub static SLOT: Slot = Slot { next: Mutex::new(None), playing: AtomicBool::new(false) };

impl Slot {
    /// Shows `frame` on the next drawn frame; a frame not drawn yet is replaced.
    pub fn show(&self, frame: VideoFrame) {
        self.put(Next::Frame(frame));
    }

    pub fn clear(&self) {
        self.set_playing(false);
        self.put(Next::Clear);
    }

    pub fn set_playing(&self, playing: bool) {
        self.playing.store(playing, Ordering::Release);
    }

    fn put(&self, next: Next) {
        if let Ok(mut slot) = self.next.lock() {
            *slot = Some(next);
        }
    }

    fn take(&self) -> Option<Next> {
        self.next.lock().ok()?.take()
    }
}

#[derive(Default)]
pub struct Video {
    renderer: Option<flicks_gpu::Renderer>,
    /// The frame on screen and its visible size.
    frame: Option<(flicks_gpu::Frame, [u32; 2])>,
    /// The last error, printed once.
    error: Option<String>,
}

impl Video {
    fn take(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let frame = match SLOT.take() {
            None => return,
            Some(Next::Clear) => {
                self.frame = None;
                return;
            }
            Some(Next::Frame(frame)) => frame,
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
        self.take(f.device, f.queue);
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
