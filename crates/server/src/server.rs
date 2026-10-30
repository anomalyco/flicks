//! The HTTPS listener and router.
//!
//! Two routes answer without a signature, both behind one global rate limit: `GET /info?probe=`
//! and, only while a pairing window is open, `POST /pair/redeem`. Everything else is a signed
//! request: the server's own admin routes under `/admin/`, then [`crate::api`]. With `--web`,
//! an unsigned `GET` for one of the web app's files gets it ([`crate::web`]), behind the same limit.

use std::convert::Infallible;
use std::net::{Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Body, Bytes};
use hyper::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, RANGE};
use hyper::http::request::Parts;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use p256::ecdsa::SigningKey;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_rustls::TlsAcceptor;

use crate::db::{self, Db};
use crate::{Error, auth, folders, keys, pair, tls};

/// How long a client gets to finish the TLS handshake, send request headers, or send a body.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The most connections served at once; more wait to be accepted.
const MAX_CONNECTIONS: usize = 256;

/// The global limit on unauthenticated requests: bursts of 20, refilled at 10 a second.
const RATE_BURST: f64 = 20.0;
const RATE_PER_SECOND: f64 = 10.0;

/// A response; its body is boxed so the catalog can stream files.
pub type Reply = Response<BoxBody<Bytes, std::io::Error>>;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Everything a request handler needs.
pub struct State {
    pub key: SigningKey,
    /// The server key, SEC1 compressed.
    pub public: Vec<u8>,
    pub data_dir: PathBuf,
    /// What clients call this server (`/info`): the machine's name unless `flicks serve --name`.
    pub name: String,
    pub db: Db,
    pub api: crate::api::Api,
    /// The port the server listens on, for pairing links.
    pub port: u16,
    pub nonces: auth::Nonces,
    pub pairing: tokio::sync::Mutex<Option<pair::Window>>,
    /// The web app's folder (`flicks serve --web`), served at `/`.
    pub web: Option<PathBuf>,
    limiter: Limiter,
}

impl State {
    /// Loads (or creates) the data directory, server key and database.
    pub fn open(data_dir: &Path, port: u16) -> Result<State, Error> {
        create_private_dir(data_dir)?;
        let key = keys::load_or_create(&data_dir.join(crate::KEY_FILE))?;
        let db = Db::open(&data_dir.join(crate::DB_FILE))?;
        Ok(State {
            public: keys::public_bytes(key.verifying_key()),
            key,
            data_dir: data_dir.to_path_buf(),
            name: machine_name(),
            db,
            api: crate::api::Api::default(),
            port,
            nonces: auth::Nonces::default(),
            pairing: tokio::sync::Mutex::new(None),
            web: None,
            limiter: Limiter::new(),
        })
    }
}

/// This machine's name: the computer name on macOS ("Ryan's Mac"), the host name elsewhere.
fn machine_name() -> String {
    let command = if cfg!(target_os = "macos") {
        std::process::Command::new("scutil").args(["--get", "ComputerName"]).output()
    } else {
        std::process::Command::new("hostname").output()
    };
    command
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().to_string())
        .filter(|name| pair::valid_name(name))
        .unwrap_or_else(|| "Flicks".to_string())
}

fn create_private_dir(dir: &Path) -> Result<(), Error> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)?;
    // An existing directory may have been created with looser permissions.
    #[cfg(unix)]
    std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    #[cfg(windows)]
    flicks_platform::permissions::owner_only(dir)?;
    Ok(())
}

/// `flicks serve`: listens on port 7686 on every address until Ctrl-C, called `name` if given,
/// serving the web app from `web` if given.
pub async fn serve(data_dir: &Path, name: Option<String>, web: Option<PathBuf>) -> Result<(), Error> {
    if name.as_deref().is_some_and(|name| !pair::valid_name(name)) {
        return Err(Error::Usage("--name is 1 to 64 bytes, without control characters or surrounding spaces"));
    }
    // Windows defaults an IPv6 socket to IPv6-only. Explicit dual-stack is needed for both
    // the local admin CLI (127.0.0.1) and IPv4 LAN/tailnet clients to reach the one listener.
    #[cfg(windows)]
    let ipv6 = (|| -> std::io::Result<TcpListener> {
        let socket = socket2::Socket::new(socket2::Domain::IPV6, socket2::Type::STREAM, None)?;
        socket.set_only_v6(false)?;
        socket.set_nonblocking(true)?;
        socket.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, crate::PORT)).into())?;
        socket.listen(128)?;
        TcpListener::from_std(socket.into())
    })();
    #[cfg(not(windows))]
    let ipv6 = TcpListener::bind((Ipv6Addr::UNSPECIFIED, crate::PORT)).await;
    let listener = match ipv6 {
        Ok(listener) => listener,
        Err(_) => TcpListener::bind(("0.0.0.0", crate::PORT)).await?,
    };
    let mut state = State::open(data_dir, crate::PORT)?;
    if let Some(name) = name {
        state.name = name;
    }
    state.web = web;
    let state = Arc::new(state);
    crate::api::start(&state).await?;
    println!(
        "Flicks \"{}\" is serving on port {} (server key {}).",
        state.name,
        crate::PORT,
        keys::encode(&state.public)
    );
    tokio::select! {
        result = run(listener, state) => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

/// Binds `addr` and serves in the background; for tests. Returns the bound address.
pub async fn spawn(data_dir: &Path, addr: SocketAddr) -> Result<(SocketAddr, Arc<State>), Error> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let state = Arc::new(State::open(data_dir, local.port())?);
    crate::api::start(&state).await?;
    tokio::spawn(run(listener, state.clone()));
    Ok((local, state))
}

/// Accepts connections forever, at most [`MAX_CONNECTIONS`] at a time.
pub async fn run(listener: TcpListener, state: Arc<State>) -> Result<(), Error> {
    let acceptor = TlsAcceptor::from(Arc::new(tls::server_config(&state.key)?));
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        // The semaphore is never closed.
        let Ok(permit) = connections.clone().acquire_owned().await else {
            return Ok(());
        };
        let tcp = match listener.accept().await {
            Ok((tcp, _)) => tcp,
            // Out of file descriptors and the like: wait and keep serving.
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let Ok(Ok(stream)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await else {
                return;
            };
            let service = service_fn(move |request| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(handle(&state, request).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HANDSHAKE_TIMEOUT)
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

/// Answers one request.
pub async fn handle<B>(state: &State, request: Request<B>) -> Reply
where
    B: Body<Data = Bytes>,
    B::Error: Into<BoxError>,
{
    let (parts, body) = request.into_parts();
    let path = parts.uri.path();
    if parts.method == Method::GET && path == "/info" {
        if !state.limiter.allow() {
            return status(StatusCode::TOO_MANY_REQUESTS);
        }
        return info(state, parts.uri.query());
    }
    if parts.method == Method::GET
        && !parts.headers.contains_key(AUTHORIZATION)
        && let Some(file) = state.web.as_deref().and_then(|dir| crate::web::find(dir, path))
    {
        if !state.limiter.allow() {
            return status(StatusCode::TOO_MANY_REQUESTS);
        }
        return crate::web::serve(&file).await;
    }
    // While no window is open the pairing route is like any other: it wants a signature.
    if parts.method == Method::POST && path == "/pair/redeem" && pairing_open(state).await {
        if !state.limiter.allow() {
            return status(StatusCode::TOO_MANY_REQUESTS);
        }
        return match read_body(&parts, body).await {
            Ok(body) => redeem(state, &body).await,
            Err(code) => status(code),
        };
    }
    match authenticate(state, &parts, body).await {
        Ok((caller, body)) => route(state, &caller, &parts, &body).await,
        Err(code) => status(code),
    }
}

/// `GET /info?probe=<nonce>`: the server key, its name and
/// `Sign(server key, "flicks-info-v1\0" ‖ lp(nonce) ‖ lp(name))`.
pub fn info(state: &State, query: Option<&str>) -> Reply {
    let probe = query
        .unwrap_or("")
        .split('&')
        .find_map(|pair| pair.strip_prefix("probe="))
        .and_then(|probe| keys::decode(probe, 64))
        .filter(|probe| probe.len() >= 16);
    let Some(probe) = probe else {
        return status(StatusCode::BAD_REQUEST);
    };
    let signature = keys::sign(&state.key, &info_message(&probe, &state.name));
    json(&Info {
        key: keys::encode(&state.public),
        name: state.name.clone(),
        signature: keys::encode(&signature),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

/// What `/info` signs.
pub fn info_message(probe: &[u8], name: &str) -> Vec<u8> {
    keys::Message::new("flicks-info-v1").field(probe).field(name.as_bytes()).into_bytes()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Info {
    pub key: String,
    pub name: String,
    pub signature: String,
    pub version: String,
}

async fn pairing_open(state: &State) -> bool {
    state.pairing.lock().await.as_ref().is_some_and(pair::Window::is_open)
}

/// `POST /pair/redeem`: adds the device if the proof and signature check out.
pub async fn redeem(state: &State, body: &[u8]) -> Reply {
    let mut window = state.pairing.lock().await;
    let Some(window) = window.as_mut().filter(|w| w.is_open()) else {
        return status(StatusCode::UNAUTHORIZED);
    };
    let checked = match pair::check(&state.public, &window.code, body) {
        Ok(checked) => checked,
        Err(pair::Refusal::Malformed) => return status(StatusCode::BAD_REQUEST),
        Err(pair::Refusal::Forbidden) => return status(StatusCode::FORBIDDEN),
    };
    if let Some((key, reply)) = &window.redeemed {
        return if *key == checked.key {
            json_bytes(StatusCode::OK, reply.clone())
        } else {
            error(StatusCode::CONFLICT, "this pairing code was already used")
        };
    }
    let user = window.user.id;
    let (key, name) = (checked.key.clone(), checked.name.clone());
    if state.db.call(move |conn| db::add_device(conn, user, &key, &name)).await.is_err() {
        return status(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let (name, admin) = (window.user.name.clone(), window.user.admin);
    let signature = keys::sign(&state.key, &pair::ok_message(&checked.transcript, &name, admin));
    let reply = pair::Reply { user: name, admin, signature: keys::encode(&signature) };
    let reply = serde_json::to_vec(&reply).unwrap_or_default();
    window.redeemed = Some((checked.key, reply.clone()));
    json_bytes(StatusCode::OK, reply)
}

/// Who signed a request.
#[derive(Debug, Clone)]
pub struct Caller {
    /// The device, or `None` for the server key itself (the local CLI).
    pub device: Option<i64>,
    pub user: Option<i64>,
    pub admin: bool,
}

/// Checks a signed request, cheapest checks first: one `Authorization` and at most one `Range`
/// header, the timestamp is fresh, the nonce is new, the key is a P-256 point, the device exists,
/// then at most 64 KiB of body is read and the signature verified.
async fn authenticate<B>(state: &State, parts: &Parts, body: B) -> Result<(Caller, Bytes), StatusCode>
where
    B: Body<Data = Bytes>,
    B::Error: Into<BoxError>,
{
    let unauthorized = || StatusCode::UNAUTHORIZED;
    let headers = &parts.headers;
    if headers.get_all(AUTHORIZATION).iter().count() > 1 || headers.get_all(RANGE).iter().count() > 1 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let header = headers.get(AUTHORIZATION).and_then(|value| auth::parse(value.as_bytes())).ok_or_else(unauthorized)?;
    if !auth::fresh(header.timestamp, crate::unix_now()) {
        return Err(unauthorized());
    }
    state.nonces.check(&header.nonce, Instant::now()).map_err(nonce_status)?;
    let device = keys::parse_public(&header.key).ok_or_else(unauthorized)?;
    let caller = if header.key == state.public {
        Caller { device: None, user: None, admin: true }
    } else {
        let key = header.key.clone();
        let owner = state
            .db
            .call(move |conn| db::device_owner(conn, &key))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .ok_or_else(unauthorized)?;
        Caller { device: Some(owner.device), user: Some(owner.user), admin: owner.admin }
    };
    let body = read_body(parts, body).await?;
    let message = auth::message(
        &state.public,
        parts.method.as_str(),
        parts.uri.path_and_query().map_or("", |pq| pq.as_str()),
        headers.get(RANGE).map_or(&[][..], |v| v.as_bytes()),
        &body,
        header.timestamp,
        &header.nonce,
    );
    if !keys::verify(&device, &message, &header.signature) {
        return Err(unauthorized());
    }
    state.nonces.insert(header.nonce, Instant::now()).map_err(nonce_status)?;
    Ok((caller, body))
}

fn nonce_status(refusal: auth::NonceRefusal) -> StatusCode {
    match refusal {
        auth::NonceRefusal::Replayed => StatusCode::UNAUTHORIZED,
        auth::NonceRefusal::Full => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// Reads a body of at most [`auth::MAX_BODY`] bytes, sent within [`HANDSHAKE_TIMEOUT`].
async fn read_body<B>(parts: &Parts, body: B) -> Result<Bytes, StatusCode>
where
    B: Body<Data = Bytes>,
    B::Error: Into<BoxError>,
{
    let too_large = || StatusCode::PAYLOAD_TOO_LARGE;
    let declared = parts.headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|len| len > auth::MAX_BODY as u64) {
        return Err(too_large());
    }
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, Limited::new(body, auth::MAX_BODY).collect()).await {
        Ok(Ok(collected)) => Ok(collected.to_bytes()),
        Ok(Err(_)) => Err(too_large()),
        Err(_) => Err(StatusCode::REQUEST_TIMEOUT),
    }
}

async fn route(state: &State, caller: &Caller, parts: &Parts, body: &Bytes) -> Reply {
    let path = parts.uri.path();
    if let Some(admin_path) = path.strip_prefix("/admin/") {
        if !caller.admin {
            return status(StatusCode::FORBIDDEN);
        }
        return match admin(state, &parts.method, admin_path, body).await {
            Ok(reply) => reply,
            Err(_) => status(StatusCode::INTERNAL_SERVER_ERROR),
        };
    }
    match crate::api::route(state, caller, parts, body).await {
        Some(reply) => reply,
        None => status(StatusCode::NOT_FOUND),
    }
}

/// `flicks status`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    pub port: u16,
    pub users: i64,
    pub devices: i64,
    pub folders: i64,
    /// Seconds left in the open pairing window.
    pub pairing: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AddFolder {
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AddUser {
    pub name: String,
    pub admin: bool,
}

/// Opens a pairing window for a user, creating it if needed.
#[derive(Debug, Serialize, Deserialize)]
pub struct OpenPairing {
    pub user: String,
    pub admin: bool,
    /// More `host:port` addresses for the link (tunnel or tailnet names), before the detected ones.
    #[serde(default)]
    pub addrs: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Pairing {
    pub link: String,
    pub expires_in: u64,
}

async fn admin(state: &State, method: &Method, path: &str, body: &[u8]) -> Result<Reply, Error> {
    let db = &state.db;
    Ok(match (method, path) {
        (&Method::GET, "status") => {
            let [users, devices, folders] = db.call(|conn| db::counts(conn)).await?;
            let pairing = state
                .pairing
                .lock()
                .await
                .as_ref()
                .filter(|w| w.is_open() && w.redeemed.is_none())
                .map(|w| w.expires.saturating_duration_since(Instant::now()).as_secs());
            json(&Status {
                version: env!("CARGO_PKG_VERSION").to_string(),
                port: state.port,
                users,
                devices,
                folders,
                pairing,
            })
        }
        (&Method::GET, "folders") => json(&db.call(|conn| db::folders(conn)).await?),
        (&Method::POST, "folders") => {
            let Some(AddFolder { path }) = parse(body) else {
                return Ok(status(StatusCode::BAD_REQUEST));
            };
            let path = match folders::check(Path::new(&path), &state.data_dir) {
                Ok(path) => path,
                Err(why) => return Ok(error(StatusCode::FORBIDDEN, &why)),
            };
            let Some(path) = path.to_str().map(str::to_string) else {
                return Ok(error(StatusCode::BAD_REQUEST, "the path isn't valid UTF-8"));
            };
            match db.call(move |conn| db::add_folder(conn, &path)).await? {
                Some(folder) => {
                    crate::api::folders_changed(state);
                    json(&folder)
                }
                None => error(StatusCode::CONFLICT, "that folder is already added"),
            }
        }
        (&Method::GET, "users") => json(&db.call(|conn| db::users(conn)).await?),
        (&Method::POST, "users") => {
            let Some(AddUser { name, admin }) = parse(body) else {
                return Ok(status(StatusCode::BAD_REQUEST));
            };
            if !pair::valid_name(&name) {
                return Ok(error(StatusCode::BAD_REQUEST, "not a valid user name"));
            }
            match db.call(move |conn| db::add_user(conn, &name, admin)).await? {
                Some(user) => json(&user),
                None => error(StatusCode::CONFLICT, "that user already exists"),
            }
        }
        (&Method::POST, "pair") => {
            let Some(OpenPairing { user, admin, addrs: extra }) = parse(body) else {
                return Ok(status(StatusCode::BAD_REQUEST));
            };
            if !pair::valid_name(&user) {
                return Ok(error(StatusCode::BAD_REQUEST, "not a valid user name"));
            }
            if extra.len() > pair::MAX_EXTRA_ADDRS || !extra.iter().all(|addr| pair::valid_addr(addr)) {
                return Ok(error(StatusCode::BAD_REQUEST, "addresses must be host:port, at most 16"));
            }
            let user = db.call(move |conn| db::ensure_user(conn, &user, admin)).await?;
            let window = pair::Window::new(user)?;
            // Names given explicitly (a tunnel, a tailnet name) go first, so they're tried first.
            let mut addrs = extra;
            addrs.extend(addresses(state.port));
            let link = pair::link(&state.public, &window.code, &addrs);
            *state.pairing.lock().await = Some(window);
            json(&Pairing { link, expires_in: pair::WINDOW.as_secs() })
        }
        (&Method::GET, "devices") => json(&db.call(|conn| db::devices(conn)).await?),
        (&Method::DELETE, _) => {
            let (table, id) = path.split_once('/').unwrap_or((path, ""));
            let Ok(id) = id.parse::<i64>() else {
                return Ok(status(StatusCode::NOT_FOUND));
            };
            let removed = match table {
                "devices" => db.call(move |conn| db::remove_device(conn, id)).await?,
                "folders" => db.call(move |conn| db::remove_folder(conn, id)).await?,
                _ => return Ok(status(StatusCode::NOT_FOUND)),
            };
            if !removed {
                return Ok(error(StatusCode::NOT_FOUND, "no such id"));
            }
            if table == "folders" {
                crate::api::folders_changed(state);
            }
            status(StatusCode::NO_CONTENT)
        }
        _ => status(StatusCode::NOT_FOUND),
    })
}

/// The addresses other devices may reach this server on, as `host:port`: every interface address
/// except loopback and link-local.
pub fn addresses(port: u16) -> Vec<String> {
    let mut addrs: Vec<SocketAddr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .map(|interface| interface.ip())
        .filter(|ip| match ip {
            std::net::IpAddr::V4(ip) => !ip.is_loopback() && !ip.is_link_local(),
            std::net::IpAddr::V6(ip) => !ip.is_loopback() && !ip.is_unicast_link_local(),
        })
        .map(|ip| SocketAddr::new(ip, port))
        .collect();
    addrs.sort();
    addrs.dedup();
    addrs.iter().map(SocketAddr::to_string).collect()
}

fn parse<T: DeserializeOwned>(body: &[u8]) -> Option<T> {
    serde_json::from_slice(body).ok()
}

/// A whole body, boxed.
pub fn full(bytes: impl Into<Bytes>) -> BoxBody<Bytes, std::io::Error> {
    Full::new(bytes.into()).map_err(|never| match never {}).boxed()
}

pub fn status(code: StatusCode) -> Reply {
    let mut reply = Response::new(full(Bytes::new()));
    *reply.status_mut() = code;
    reply
}

pub fn json<T: Serialize>(value: &T) -> Reply {
    match serde_json::to_vec(value) {
        Ok(bytes) => json_bytes(StatusCode::OK, bytes),
        Err(_) => status(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

fn json_bytes(code: StatusCode, bytes: Vec<u8>) -> Reply {
    let mut reply = Response::new(full(bytes));
    *reply.status_mut() = code;
    reply.headers_mut().insert(CONTENT_TYPE, hyper::header::HeaderValue::from_static("application/json"));
    reply
}

/// An error with a message for the CLI or app to show.
pub fn error(code: StatusCode, message: &str) -> Reply {
    let body = serde_json::json!({ "error": message });
    json_bytes(code, serde_json::to_vec(&body).unwrap_or_default())
}

/// A token bucket shared by every unauthenticated request. It reads tokio's clock (the same as
/// the system's outside tests) so tests can pause it.
struct Limiter(Mutex<(f64, tokio::time::Instant)>);

impl Limiter {
    fn new() -> Limiter {
        Limiter(Mutex::new((RATE_BURST, tokio::time::Instant::now())))
    }

    fn allow(&self) -> bool {
        let mut bucket = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let now = tokio::time::Instant::now();
        let refill = now.duration_since(bucket.1).as_secs_f64() * RATE_PER_SECOND;
        *bucket = ((bucket.0 + refill).min(RATE_BURST), now);
        if bucket.0 < 1.0 {
            return false;
        }
        bucket.0 -= 1.0;
        true
    }
}
