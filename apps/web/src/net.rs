//! The browser's network: the one server that served the page, reached with same-origin requests
//! signed exactly as `flicks_client` signs them (its message bytes, a WebCrypto signature). The
//! device key and the saved server live in IndexedDB (`browser.js`). Requests run as tasks on the
//! page's event loop and answer by updating the app's state, as the desktop's network thread does.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::time::Duration;

use flicks_client::merge::merge;
use flicks_client::{Entry, Server, auth, keys, pair};
use flicks_ui::{Catalog, Flicks, Request};
use rune::AppHandle;
use wasm_bindgen::prelude::*;

/// What this device calls itself to the server.
const DEVICE_NAME: &str = "Flicks on the web";

/// The IndexedDB entry holding the saved server.
const SERVER: &str = "server";

#[wasm_bindgen(module = "/src/browser.js")]
extern "C" {
    #[wasm_bindgen(catch)]
    async fn load(name: &str) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch)]
    async fn save(name: &str, value: &str) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch, js_name = publicKey)]
    async fn public_key() -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch)]
    async fn sign(message: &[u8]) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch)]
    async fn acquire() -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch)]
    async fn request(method: &str, path: &str, authorization: Option<String>, body: &[u8]) -> Result<JsValue, JsValue>;
}

pub struct Net {
    app: AppHandle<Flicks>,
    /// The paired server, once loaded or paired.
    server: Rc<RefCell<Option<Server>>>,
}

impl Net {
    pub fn new(app: AppHandle<Flicks>) -> Net {
        Net { app, server: Rc::default() }
    }

    pub fn send(&self, request: Request) {
        let (app, saved) = (self.app.clone(), self.server.clone());
        wasm_bindgen_futures::spawn_local(async move {
            match request {
                Request::Refresh => refresh(&app, &saved).await,
                Request::Pair(link) => {
                    let paired = pair_with(&link).await;
                    let ok = paired.as_ref().ok().cloned();
                    app.update(move |s, _| s.paired(paired.map(|server| server.name)));
                    if let Some(server) = ok {
                        *saved.borrow_mut() = Some(server);
                        refresh(&app, &saved).await;
                    }
                }
                Request::Artwork(key, name) => {
                    let server = saved.borrow().clone();
                    let bytes = match server {
                        Some(server) if server.key == key => {
                            // Held until the fetch ends, and taken before signing.
                            let _slot = Slot::take().await;
                            signed(&server, &format!("/artwork/{name}")).await.ok()
                        }
                        _ => None,
                    };
                    app.update(move |s, _| s.artwork(&key, &name, bytes));
                }
                // The Server screens aren't on the web yet: no server is listed for them.
                Request::Admin(..) => {}
                // Playback isn't on the web yet.
                Request::Play(id, _) => {
                    app.update(move |s, _| s.player_failed(id, "Playing isn't in the web app yet.".into()))
                }
                Request::Progress(..) => {}
            }
        });
    }
}

/// One of six artwork fetches at a time; dropping it gives the slot back.
struct Slot(Option<js_sys::Function>);

impl Slot {
    async fn take() -> Slot {
        Slot(acquire().await.ok().map(js_sys::Function::from))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(release) = &self.0 {
            let _ = release.call0(&JsValue::NULL);
        }
    }
}

/// Fetches the server's catalog and shows it, or opens Add Server when this browser hasn't paired.
async fn refresh(app: &AppHandle<Flicks>, saved: &Rc<RefCell<Option<Server>>>) {
    let known = saved.borrow().clone();
    let server = match known {
        Some(server) => server,
        None => match load_server().await {
            Ok(Some(server)) => {
                *saved.borrow_mut() = Some(server.clone());
                server
            }
            Ok(None) => return app.update(|s, tx| s.no_servers(tx)),
            Err(error) => return app.update(move |s, _| s.failed(error)),
        },
    };
    match catalog(server).await {
        Ok(catalog) => app.update(move |s, tx| s.loaded(catalog, None, true, tx)),
        Err(error) => app.update(move |s, _| s.failed(error)),
    }
}

async fn load_server() -> Result<Option<Server>, String> {
    let json = load(SERVER).await.map_err(js_error)?.as_string();
    Ok(json.and_then(|json| serde_json::from_str(&json).ok()))
}

/// The server's items, merged as the desktop merges them (one server, so one source a title).
async fn catalog(mut server: Server) -> Result<Catalog, String> {
    const PAGE: usize = 200;
    let started = js_sys::Date::now();
    let server_key = server_key(&server)?;
    server.name = probe(&server_key).await?;
    let latency = Duration::from_secs_f64(((js_sys::Date::now() - started) / 1000.0).max(0.0));
    let mut entries = Vec::new();
    // At most 10,000 pages (two million items).
    for page in 0..10_000 {
        let body = signed(&server, &format!("/items?limit={PAGE}&offset={}", page * PAGE)).await?;
        let items: Vec<Entry> = serde_json::from_slice(&body).map_err(|_| "the server's catalog is malformed")?;
        let done = items.len() < PAGE;
        entries.extend(items);
        if done {
            break;
        }
    }
    let catalogs = BTreeMap::from([(server.key.clone(), entries)]);
    let latencies = HashMap::from([(server.key.clone(), latency)]);
    Ok(Catalog::build(merge(&[server], &catalogs, &latencies)))
}

fn server_key(server: &Server) -> Result<Vec<u8>, String> {
    keys::decode(&server.key, keys::PUBLIC_KEY_LEN)
        .filter(|key| keys::parse_public(key).is_some())
        .ok_or_else(|| "the saved server's key is malformed".to_string())
}

/// Pairs with the server a `flicks://pair` link names, which must be the one that served this page
/// (its addresses are for other devices; the browser only talks to its own origin).
async fn pair_with(link: &str) -> Result<Server, String> {
    let link = pair::parse_link(link.trim()).ok_or("That isn't a Flicks pairing link.")?;
    let server_key = keys::public_bytes(&link.server_key);
    let name = probe(&server_key).await?;
    let device = device_key().await?;
    let transcript = pair::transcript(&server_key, &device, DEVICE_NAME);
    let proof = pair::proof(&server_key, &link.code, &transcript).map_err(|e| e.to_string())?;
    let signature = sign_bytes(&pair::request_message(&transcript, &proof)).await?;
    let body = pair::Request {
        key: keys::encode(&device),
        name: DEVICE_NAME.into(),
        proof: keys::encode(&proof),
        signature: keys::encode(&signature),
    };
    let body = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
    let reply = send("POST", "/pair/redeem", None, &body).await?;
    let reply: pair::Reply = serde_json::from_slice(&reply).map_err(|_| "the server's answer is malformed")?;
    if !pair::verify_reply(&link.server_key, &transcript, &reply) {
        return Err(flicks_client::Error::WrongServer.to_string());
    }
    let server =
        Server { key: keys::encode(&server_key), name, addrs: link.addrs, user: reply.user, admin: reply.admin };
    let json = serde_json::to_string(&server).map_err(|e| e.to_string())?;
    save(SERVER, &json).await.map_err(js_error)?;
    Ok(server)
}

/// `GET /info?probe=<nonce>`: the server's name, if this origin's answer is signed by `server_key`
/// (the page may come through a proxy; the server behind it must hold the key).
async fn probe(server_key: &[u8]) -> Result<String, String> {
    #[derive(serde::Deserialize)]
    struct Info {
        key: String,
        name: String,
        signature: String,
    }
    let nonce = keys::random::<16>().map_err(|e| e.to_string())?;
    let body = send("GET", &format!("/info?probe={}", keys::encode(&nonce)), None, &[]).await?;
    let info: Info = serde_json::from_slice(&body).map_err(|_| "the server's answer is malformed")?;
    let message = keys::Message::new("flicks-info-v1").field(&nonce).field(info.name.as_bytes()).into_bytes();
    let verified = keys::parse_public(server_key).is_some_and(|key| {
        keys::decode(&info.signature, 64).is_some_and(|signature| keys::verify(&key, &message, &signature))
    });
    if info.key != keys::encode(server_key) || !verified {
        return Err("This page's server isn't the one the link names (open the page on that server).".into());
    }
    Ok(if pair::valid_name(&info.name) { info.name } else { "Flicks".into() })
}

/// A signed `GET`, its body; signed over the bytes `flicks_client::auth::message` makes.
async fn signed(server: &Server, path: &str) -> Result<Vec<u8>, String> {
    let server_key = server_key(server)?;
    let device = device_key().await?;
    let timestamp = (js_sys::Date::now() / 1000.0) as u64;
    let nonce = keys::random::<{ auth::NONCE_LEN }>().map_err(|e| e.to_string())?;
    let message = auth::message(&server_key, "GET", path, b"", b"", timestamp, &nonce);
    let signature = sign_bytes(&message).await?;
    send("GET", path, Some(auth::header(&device, timestamp, &nonce, &signature)), &[]).await
}

/// A same-origin request's body; an error status becomes the server's message.
async fn send(method: &str, path: &str, authorization: Option<String>, body: &[u8]) -> Result<Vec<u8>, String> {
    let answer = js_sys::Array::from(&request(method, path, authorization, body).await.map_err(js_error)?);
    let status = answer.get(0).as_f64().unwrap_or(0.0) as u16;
    let body = js_sys::Uint8Array::new(&answer.get(1)).to_vec();
    if (200..300).contains(&status) {
        return Ok(body);
    }
    let message = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("error")?.as_str().map(str::to_string))
        .unwrap_or_else(|| match status {
            401 => "This browser isn't paired with the server (or was removed); add it again.".into(),
            429 => "The server is busy; try again in a moment.".into(),
            _ => format!("The server answered {status}."),
        });
    Err(message)
}

/// This device's public key, SEC1 compressed (33 bytes) as every Flicks message has it.
async fn device_key() -> Result<Vec<u8>, String> {
    let raw = js_sys::Uint8Array::new(&public_key().await.map_err(js_error)?).to_vec();
    // WebCrypto exports the uncompressed point, 0x04 ‖ x ‖ y.
    let (Some(0x04), Some(x), Some(y_last)) = (raw.first(), raw.get(1..33), raw.get(64)) else {
        return Err("the browser's key is malformed".into());
    };
    let mut key = vec![0x02 | (y_last & 1)];
    key.extend_from_slice(x);
    Ok(key)
}

async fn sign_bytes(message: &[u8]) -> Result<Vec<u8>, String> {
    Ok(js_sys::Uint8Array::new(&sign(message).await.map_err(js_error)?).to_vec())
}

/// A thrown string, or an exception's message.
fn js_error(error: JsValue) -> String {
    error
        .as_string()
        .or_else(|| js_sys::Reflect::get(&error, &JsValue::from_str("message")).ok()?.as_string())
        .unwrap_or_else(|| "the browser refused".into())
}
