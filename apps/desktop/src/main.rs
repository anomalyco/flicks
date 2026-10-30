//! Flicks for macOS: the screens of `flicks-ui` in a window, with the network on its own thread.
//! `cargo run -p flicks-desktop` opens the window; `-- --snap home.png --scene home` renders a
//! scene headless (`--list-scenes` lists them).

#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod worker;

use rune::App;

fn main() {
    rune::prewarm_gpu();
    let app = App::new(flicks_ui::spec());
    let handle = app.handle();
    // The network starts after the first frame, so launch draws at once.
    app.on_ready(move |_| {
        let started = handle.clone();
        handle.update(move |s, _| {
            let worker = worker::start(started);
            s.network = Some(Box::new(move |request| worker.send(request)));
        });
    })
    .run();
}
