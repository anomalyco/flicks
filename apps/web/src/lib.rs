//! Flicks in the browser: the same screens as the desktop app (`flicks-ui`) on Rune's web host,
//! served by the Flicks server at `/` (`flicks serve --web`), so every request is same-origin.
//! `build.sh` makes the folder to serve.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

#[cfg(target_arch = "wasm32")]
mod net;

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() -> Result<(), wasm_bindgen::JsValue> {
    let app = rune::App::new(flicks_ui::spec());
    let handle = app.handle();
    let network = handle.clone();
    handle.update(move |s, _| {
        let net = net::Net::new(network);
        net.send(flicks_ui::Request::Refresh);
        s.network = Some(Box::new(move |request| net.send(request)));
    });
    app.try_run().map_err(|error| wasm_bindgen::JsValue::from_str(&error))
}
