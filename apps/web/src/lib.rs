//! Flicks in the browser: the same screens as the desktop app (`flicks-ui`) on Rune's web host,
//! served by the Flicks server at `/` (`flicks serve --web`), so every request is same-origin.
//! `build.sh` makes the folder to serve.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

#[cfg(target_arch = "wasm32")]
mod net;

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsCast;

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() -> Result<(), wasm_bindgen::JsValue> {
    // Every page load is a cold launch: it opens on the intro, whose sound `start.js` plays.
    let app = rune::App::new(flicks_ui::spec()).init(|| {
        let mut s = flicks_ui::Flicks::new();
        s.intro = Some(flicks_ui::Intro::new(Some(Box::new(intro_sound))));
        s
    });
    let handle = app.handle();
    let network = handle.clone();
    handle.update(move |s, _| {
        let net = net::Net::new(network);
        net.send(flicks_ui::Request::Refresh);
        s.network = Some(Box::new(move |request| net.send(request)));
    });
    app.try_run().map_err(|error| wasm_bindgen::JsValue::from_str(&error))
}

/// Hands the intro's cue to `start.js` (`globalThis.flicksIntroSound`), if it's there.
#[cfg(target_arch = "wasm32")]
fn intro_sound(cue: flicks_ui::Sound) {
    let cue = match cue {
        flicks_ui::Sound::Play => "play",
        flicks_ui::Sound::Stop => "stop",
    };
    let hook = js_sys::Reflect::get(&js_sys::global(), &"flicksIntroSound".into());
    if let Ok(hook) = hook.and_then(|f| f.dyn_into::<js_sys::Function>()) {
        let _ = hook.call1(&wasm_bindgen::JsValue::NULL, &cue.into());
    }
}
