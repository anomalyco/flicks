//! Colours, type and motion: near-black, artwork first, warm white text, flat fills (never a
//! gradient), one coral accent, springs for everything that moves.

use rune::{Color, Spring, TextStyle, oklch, oklcha};

pub fn bg() -> Color {
    oklch(0.145, 0.004, 265.0)
}
/// Fields, and what stands in for missing artwork.
pub fn raised() -> Color {
    oklch(0.22, 0.004, 265.0)
}
/// The wordmark's off-white; also the primary button's fill.
pub fn text() -> Color {
    oklch(0.95, 0.012, 80.0)
}
pub fn muted() -> Color {
    oklch(0.72, 0.006, 265.0)
}
pub fn faint() -> Color {
    oklcha(1.0, 0.0, 0.0, 0.08)
}
/// The logo's front frame: progress, the seek bar and focus rings.
pub fn accent() -> Color {
    oklch(0.715, 0.18, 16.8)
}
/// Keyboard focus rings.
pub fn focus() -> Color {
    accent()
}
pub fn error() -> Color {
    oklch(0.72, 0.15, 27.0)
}
/// Secondary buttons (More Info, season tabs, the selected nav item).
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
/// A title's name under its artwork.
pub const NAME: TextStyle = TextStyle::new(14.0, 600.0);
pub const BODY: TextStyle = TextStyle::new(14.0, 400.0).lh(21.0);
pub const META: TextStyle = TextStyle::new(14.0, 550.0).features("'tnum' 1");
pub const SMALL: TextStyle = TextStyle::new(12.0, 500.0).features("'tnum' 1");
pub const BUTTON: TextStyle = TextStyle::new(15.0, 650.0);

/// Poster tiles, 2:3.
pub const POSTER_W: f32 = 168.0;
pub const POSTER_H: f32 = 252.0;
/// Continue Watching's stills, 16:9.
pub const STILL_W: f32 = 320.0;
pub const STILL_H: f32 = 180.0;
/// Artwork's corners.
pub const ART_RADIUS: f32 = 6.0;
/// Between tiles in a row.
pub const TILE_GAP: f32 = 16.0;
/// The window's margin left and right.
pub const GUTTER: f32 = 48.0;
/// The top bar, which floats over each screen.
pub const BAR_H: f32 = 64.0;
/// The backdrop band across the top of Home and a title's page.
pub const BAND_H: f32 = 400.0;

/// How much a hovered or focused tile grows, as on Apple TV.
pub const LIFT: f32 = 1.08;
/// The lift's spring: quick, with a little settle.
pub const LIFT_SPRING: Spring = Spring::new(0.32, 0.18);
/// Screens replacing each other.
pub const SCREEN_SPRING: Spring = Spring::new(0.42, 0.0);
