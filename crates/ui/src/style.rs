//! Colours and type: near-black, posters first, white text.

use rune::{Color, TextStyle, oklch, oklcha};

pub fn bg() -> Color {
    oklch(0.16, 0.003, 265.0)
}
pub fn raised() -> Color {
    oklch(0.22, 0.004, 265.0)
}
pub fn text() -> Color {
    oklch(0.97, 0.0, 0.0)
}
pub fn muted() -> Color {
    oklch(0.72, 0.006, 265.0)
}
pub fn faint() -> Color {
    oklcha(1.0, 0.0, 0.0, 0.08)
}
pub fn accent() -> Color {
    oklch(0.63, 0.22, 27.0)
}
pub fn error() -> Color {
    oklch(0.72, 0.15, 27.0)
}

pub const BRAND: TextStyle = TextStyle::new(20.0, 800.0).tracking(-0.4);
pub const NAV: TextStyle = TextStyle::new(13.0, 500.0);
pub const HERO: TextStyle = TextStyle::new(40.0, 800.0).lh(46.0).tracking(-0.8);
pub const HEADING: TextStyle = TextStyle::new(17.0, 650.0).tracking(-0.2);
pub const BODY: TextStyle = TextStyle::new(14.0, 400.0).lh(21.0);
pub const META: TextStyle = TextStyle::new(13.0, 500.0);
pub const TILE: TextStyle = TextStyle::new(13.0, 550.0).lh(17.0);
pub const SMALL: TextStyle = TextStyle::new(12.0, 450.0);
pub const BUTTON: TextStyle = TextStyle::new(15.0, 650.0);

/// Poster tiles, 2:3.
pub const POSTER_W: f32 = 150.0;
pub const POSTER_H: f32 = 225.0;
/// The window's margin left and right.
pub const GUTTER: f32 = 40.0;
