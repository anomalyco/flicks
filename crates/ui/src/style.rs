//! Colours, type and motion: near-black, artwork first, warm white text, the logo's violet to
//! amber for what's primary, springs for everything that moves.

use rune::{Color, Spring, TextStyle, oklch, oklcha};

pub fn bg() -> Color {
    oklch(0.145, 0.004, 265.0)
}
pub fn raised() -> Color {
    oklch(0.22, 0.004, 265.0)
}
/// The wordmark's off-white.
pub fn text() -> Color {
    oklch(0.95, 0.012, 80.0)
}
pub fn muted() -> Color {
    oklch(0.72, 0.006, 265.0)
}
pub fn faint() -> Color {
    oklcha(1.0, 0.0, 0.0, 0.08)
}
// The logo's frames, back to front: violet, orchid, coral and amber.
pub fn violet() -> Color {
    oklch(0.504, 0.229, 282.0)
}
pub fn orchid() -> Color {
    oklch(0.589, 0.213, 310.5)
}
pub fn coral() -> Color {
    oklch(0.715, 0.18, 16.8)
}
pub fn amber() -> Color {
    oklch(0.826, 0.148, 73.8)
}
/// Primary buttons, left to right.
pub fn primary() -> [(f32, Color); 3] {
    [(0.0, violet()), (0.6, orchid()), (1.0, coral())]
}
/// How far in a title is: the logo's front frame.
pub fn progress() -> [(f32, Color); 2] {
    [(0.0, coral()), (1.0, amber())]
}
/// Keyboard focus rings.
pub fn focus() -> Color {
    amber()
}
pub fn error() -> Color {
    oklch(0.72, 0.15, 27.0)
}
/// Glass buttons over artwork (More Info, row arrows).
pub fn glass() -> Color {
    oklcha(1.0, 0.0, 0.0, 0.16)
}

/// The lockup (mark and wordmark) in the top bar.
pub const LOGO_H: f32 = 26.0;
pub const LOGO_W: f32 = LOGO_H * 383.48 / 119.0;
pub const NAV: TextStyle = TextStyle::new(14.0, 550.0);
/// A featured title's name on Home and a title's page.
pub const DISPLAY: TextStyle = TextStyle::new(56.0, 800.0).lh(60.0).tracking(-1.6);
pub const HERO: TextStyle = TextStyle::new(40.0, 800.0).lh(46.0).tracking(-0.8);
pub const HEADING: TextStyle = TextStyle::new(19.0, 700.0).tracking(-0.3);
pub const BODY: TextStyle = TextStyle::new(14.0, 400.0).lh(21.0);
pub const META: TextStyle = TextStyle::new(14.0, 550.0).features("'tnum' 1");
pub const SMALL: TextStyle = TextStyle::new(12.0, 500.0).features("'tnum' 1");
pub const BUTTON: TextStyle = TextStyle::new(15.0, 650.0);

/// Poster tiles, 2:3.
pub const POSTER_W: f32 = 168.0;
pub const POSTER_H: f32 = 252.0;
/// Continue Watching's cards, 16:9.
pub const CARD_W: f32 = 320.0;
pub const CARD_H: f32 = 180.0;
pub const TILE_RADIUS: f32 = 10.0;
/// Between tiles in a row.
pub const TILE_GAP: f32 = 16.0;
/// The window's margin left and right.
pub const GUTTER: f32 = 48.0;
/// The top bar, which floats over each screen.
pub const BAR_H: f32 = 64.0;
/// Home's featured title and the top of a title's page.
pub const HERO_H: f32 = 600.0;

/// How much a hovered or focused tile grows, as on Apple TV.
pub const LIFT: f32 = 1.08;
/// The lift's spring: quick, with a little settle.
pub const LIFT_SPRING: Spring = Spring::new(0.32, 0.18);
/// Screens replacing each other.
pub const SCREEN_SPRING: Spring = Spring::new(0.42, 0.0);
