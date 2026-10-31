//! The launch intro: on the page's near-black, the logo's three frames fly in from depth one after
//! another and land on springs, the mark slides off "flicks" as it fades in, and the whole lockup
//! eases back into its place in the top bar while the backdrop dissolves into Home, which is drawn
//! behind it from the first frame. The beats overlap, so the logo never comes to rest between
//! them: the mark is still gliding into the lockup when the lockup takes it to the bar, one curve
//! from the centre to the bar, and the whole stage pushes in slowly underneath. Motion, scale and
//! opacity only: no gradients, glows or sweeps. About 2.4 s, with the signature sound
//! (`assets/brand/intro.wav`) played by the app shell.
//!
//! The shell turns it on for a cold launch ([`Intro::new`]); returning to the app never does.
//! Any key, click or tap skips it. Under reduced motion it is a short fade, silent.
//!
//! Every beat is a spring scheduled from one commit, once the launch's own work has passed, so it
//! runs on the display link at the screen's rate and the link stops when it ends.

use std::cell::Cell;
use std::time::Duration;

use rune::{AnimExt, Cx, El, Image, ImageCache, Spring, SurfaceFit, Tx, el};

use crate::{Flicks, style};

/// What the intro asks of the shell's sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sound {
    /// Start the intro sound now.
    Play,
    /// The intro was skipped: fade the sound out.
    Stop,
}

pub struct Intro {
    /// Plays the intro sound; `None` keeps it silent.
    sound: Option<Box<dyn Fn(Sound)>>,
    /// The first poll, and the last one with how many frames in a row came on time since.
    first: Option<f64>,
    last: f64,
    steady: u32,
    /// When the beats started, in poll seconds.
    started: Option<f64>,
    /// Only the view sees reduced motion; it notes it here for the poll that starts the intro.
    reduced: Cell<bool>,
    /// The window's activation, once the system reports it: a launch in the background stays
    /// silent.
    focused: Option<bool>,
    frames: Vec<Image>,
    wordmark: Option<Image>,
}

const FRAMES: [&[u8]; 3] = [
    include_bytes!("../../../assets/brand/intro-back.png"),
    include_bytes!("../../../assets/brand/intro-middle.png"),
    include_bytes!("../../../assets/brand/intro-front.png"),
];
const WORDMARK: &[u8] = include_bytes!("../../../assets/brand/intro-wordmark.png");

/// Lockup points per SVG unit, and the lockup's geometry (`assets/brand/lockup.svg`): the
/// mark's box and the wordmark's box inside it.
const K: f32 = 1.15;
const LOCKUP: [f32; 2] = [383.48 * K, 119.0 * K];
const MARK_BOX: [f32; 4] = [10.0 * K, 10.0 * K, 94.3 * K, 98.9 * K];
const WORD_BOX: [f32; 4] = [126.75 * K, 12.0 * K, 256.73 * K, 96.25 * K];
/// How far right of its place the mark starts, so it lands centred before "flicks" joins it, and
/// how much of that the whole lockup glides back with it.
const MARK_SHIFT: f32 = LOCKUP[0] / 2.0 - (MARK_BOX[0] + MARK_BOX[2] / 2.0);
const GLIDE_SHIFT: f32 = 110.0;

// The beats, in seconds from the start, timed to intro.wav, which starts with them: its whoosh
// builds to a first hit at 0.46 s and a fuller second one at 0.57 s.
/// When each frame starts its flight, back to front. A frame is within 1% of its size 0.32 to
/// 0.36 s later: the middle one on the sound's first hit, the front one on the second, which
/// lands with a little more weight (a 2% overshoot, under 1% for the others).
const FLY_AT: [f32; 3] = [0.0, 0.1, 0.26];
const FLIGHT: Spring = Spring::new(0.55, 0.18);
const LANDING: Spring = Spring::new(0.55, 0.24);
/// The whole stage pushes in this slowly from the first frame, settling as the lockup lands.
const PUSH_FROM: f32 = 0.95;
const PUSH: Spring = Spring::new(2.0, 0.0);
/// As the front frame lands, the mark slides off "flicks", which fades in behind it, and the whole
/// lockup glides after it, slowly enough to still be moving when the exit takes it.
const WORD_AT: f32 = 0.5;
const SLIDE: Spring = Spring::new(0.7, 0.0);
const GLIDE: Spring = Spring::new(1.6, 0.0);
const WORD_FADE_AT: f32 = 0.56;
const WORD_FADE: Spring = Spring::new(0.5, 0.0);
/// The lockup eases back into the top bar's logo; the backdrop dissolves into Home on the same
/// curve just behind it.
const EXIT_AT: f32 = 1.15;
const EXIT: Spring = Spring::new(0.8, 0.0);
const BACKDROP_AT: f32 = 1.33;
/// The intro leaves the tree, its lockup on the top bar's to within half a point, which takes
/// over.
const END: f64 = 2.4;
/// Under reduced motion: the still logo holds, then fades out with the backdrop.
const REDUCED_EXIT_AT: f32 = 0.9;
const REDUCED_END: f64 = 1.25;

impl Intro {
    /// An intro for a cold launch; `sound` plays its sound (`None`: silent).
    pub fn new(sound: Option<Box<dyn Fn(Sound)>>) -> Intro {
        Intro {
            sound,
            first: None,
            last: f64::NEG_INFINITY,
            steady: 0,
            started: None,
            reduced: Cell::new(false),
            focused: None,
            frames: Vec::new(),
            wordmark: None,
        }
    }

    /// The fixed logo parts, pre-rendered at three device pixels a point.
    pub fn load(&mut self, images: &ImageCache) {
        self.frames = FRAMES.iter().map(|bytes| images.load(bytes.to_vec())).collect();
        self.wordmark = Some(images.load(WORDMARK.to_vec()));
    }

    fn end(&self) -> f64 {
        if self.reduced.get() { REDUCED_END } else { END }
    }
}

/// Frames this close together are on time (40 Hz and up).
const ON_TIME: f64 = 0.025;
/// Frames on time in a row before the beats start, and the longest wait for them.
const STEADY: u32 = 3;
const MAX_WAIT: f64 = 0.6;

/// Starts the beats once the display link is steady: a launch's first frames wait on its own work
/// (the window coming forward, the first catalog), which would drop the frames' flight. Takes the
/// intro away when they're done.
pub fn poll(s: &mut Flicks, now: f64, tx: &mut Tx) -> bool {
    let Some(intro) = &mut s.intro else { return false };
    let Some(started) = intro.started else {
        let first = *intro.first.get_or_insert(now);
        intro.steady = if now - intro.last < ON_TIME { intro.steady + 1 } else { 0 };
        intro.last = now;
        if intro.steady < STEADY && now - first < MAX_WAIT {
            tx.wake_after(Duration::ZERO);
            return false;
        }
        intro.started = Some(now);
        if !intro.reduced.get()
            && intro.focused != Some(false)
            && let Some(sound) = &intro.sound
        {
            sound(Sound::Play);
        }
        tx.wake_after(Duration::from_secs_f64(intro.end()));
        return true;
    };
    let left = intro.end() - (now - started);
    if left > 0.0 {
        tx.wake_after(Duration::from_secs_f64(left));
        return false;
    }
    s.intro = None;
    true
}

/// Any key, click or tap: the intro goes at once, and its sound fades.
pub fn skip(s: &mut Flicks) {
    if let Some(intro) = s.intro.take()
        && intro.started.is_some()
        && let Some(sound) = &intro.sound
    {
        sound(Sound::Stop);
    }
}

pub fn focus(s: &mut Flicks, focused: bool) {
    if let Some(intro) = &mut s.intro {
        intro.focused = Some(focused);
    }
}

/// While the lockup is on its way to the top bar, the bar's own logo waits hidden.
pub fn hides_logo(s: &Flicks, cx: &Cx<Flicks>) -> bool {
    s.intro.is_some() && !cx.is_reduced_motion()
}

/// The intro over the whole window: the backdrop, then the lockup centred on it.
pub fn view(intro: &Intro, cx: &mut Cx<Flicks>) -> El {
    let reduced = cx.is_reduced_motion();
    intro.reduced.set(reduced);
    let on = intro.started.is_some();
    let logo = if reduced { reduced_logo(intro, on) } else { logo(intro, on, cx) };
    el().key("intro")
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .on_press(skip)
        .child(
            el().key("intro-backdrop")
                .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .fill(style::bg())
                .opacity(if on { 0.0 } else { 1.0 })
                .opacity_motion(if reduced {
                    Spring::new(0.45, 0.0).delay(REDUCED_EXIT_AT)
                } else {
                    EXIT.delay(BACKDROP_AT)
                }),
        )
        .child(
            el().key("intro-stage")
                .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .center()
                .scale(if on || reduced { 1.0 } else { PUSH_FROM })
                .scale_motion(PUSH)
                .child(logo),
        )
}

fn picture(key: String, image: Option<&Image>, rect: [f32; 4]) -> El {
    let mut e = el().key(key.as_str()).absolute(rect[0], rect[1]).size(rect[2], rect[3]);
    if let Some(image) = image {
        e = e.pixels_fit(image.pixels(), SurfaceFit::Fit);
    }
    e
}

fn logo(intro: &Intro, on: bool, cx: &mut Cx<Flicks>) -> El {
    let whole = [0.0, 0.0, MARK_BOX[2], MARK_BOX[3]];
    let mut mark = el()
        .key("intro-mark")
        .absolute(MARK_BOX[0], MARK_BOX[1])
        .size(MARK_BOX[2], MARK_BOX[3])
        .offset(if on { 0.0 } else { MARK_SHIFT - GLIDE_SHIFT }, 0.0)
        .offset_motion(SLIDE.delay(WORD_AT));
    for (i, at) in FLY_AT.into_iter().enumerate() {
        let frame = picture(format!("intro-frame-{i}"), intro.frames.get(i), whole)
            .scale(if on { 1.0 } else { 0.22 })
            .scale_motion(if i == 2 { LANDING } else { FLIGHT }.delay(at))
            .opacity(if on { 1.0 } else { 0.0 })
            .opacity_motion(Spring::new(0.3, 0.0).delay(at));
        mark = mark.child(frame);
    }
    let word = picture("intro-word".into(), intro.wordmark.as_ref(), WORD_BOX)
        .opacity(if on { 1.0 } else { 0.0 })
        .opacity_motion(WORD_FADE.delay(WORD_FADE_AT));
    let glide = el()
        .key("intro-glide")
        .absolute(0.0, 0.0)
        .size(LOCKUP[0], LOCKUP[1])
        .offset(if on { 0.0 } else { GLIDE_SHIFT }, 0.0)
        .offset_motion(GLIDE.delay(WORD_AT))
        .child(word)
        .child(mark);
    // Where it ends: on the top bar's logo, else eased back and gone.
    let here = cx.layout("intro-logo");
    let bar = cx.layout("logo");
    let (offset, scale, opacity) = match (here, bar) {
        (Some(a), Some(b)) => {
            let center = |r: [f32; 4]| [r[0] + r[2] / 2.0, r[1] + r[3] / 2.0];
            let (a, b) = (center(a), center(b));
            ([b[0] - a[0], b[1] - a[1]], style::LOGO_H / LOCKUP[1], 1.0)
        }
        _ => ([0.0, 0.0], 0.92, 0.0),
    };
    let lockup = el().key("intro-logo").size(LOCKUP[0], LOCKUP[1]);
    let lockup = if on { lockup.offset(offset[0], offset[1]).scale(scale).opacity(opacity) } else { lockup };
    lockup
        .offset_motion(EXIT.delay(EXIT_AT))
        .scale_motion(EXIT.delay(EXIT_AT))
        .opacity_motion(EXIT.delay(EXIT_AT))
        .child(glide)
}

/// The lockup, still, fading in and out.
fn reduced_logo(intro: &Intro, on: bool) -> El {
    let whole = [0.0, 0.0, MARK_BOX[2], MARK_BOX[3]];
    let mut mark = el().key("intro-mark").absolute(MARK_BOX[0], MARK_BOX[1]).size(MARK_BOX[2], MARK_BOX[3]);
    for i in 0..3 {
        mark = mark.child(picture(format!("intro-frame-{i}"), intro.frames.get(i), whole));
    }
    el().key("intro-logo")
        .size(LOCKUP[0], LOCKUP[1])
        .opacity(if on { 0.0 } else { 1.0 })
        .opacity_motion(Spring::new(0.3, 0.0).delay(REDUCED_EXIT_AT))
        .child(picture("intro-word".into(), intro.wordmark.as_ref(), WORD_BOX))
        .child(mark)
}
