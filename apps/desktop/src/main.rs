//! Flicks for macOS: the screens of `flicks-ui` in a window, with the network on its own thread.
//! `cargo run -p flicks-desktop` opens the window; `-- --snap home.png --scene home` renders a
//! scene headless (`--list-scenes` lists them).

#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod admin;
mod player;
mod worker;

use rune::App;

/// The Dock and app switcher icon, `assets/brand/app-icon.svg` rendered at 1024 px. The app has
/// no bundle yet, so Rune sets it once the app has launched (set before `run`, AppKit's launch
/// puts the default icon back over it).
const ICON: &[u8] = include_bytes!("../../../assets/brand/app-icon.png");

fn main() {
    rune::prewarm_gpu();
    let app = App::new(flicks_ui::spec()).compositor(player::video::Video::default());
    let handle = app.handle();
    // The network starts after the first frame, so launch draws at once.
    app.on_ready(move |_| {
        if let Err(error) = rune::set_app_icon(ICON) {
            eprintln!("flicks: {error}");
        }
        let started = handle.clone();
        handle.update(move |s, _| {
            let worker = worker::start(started);
            s.network = Some(Box::new(move |request| worker.send(request)));
        });
    })
    .run();
}
