//! The client core (docs/architecture.md, "App"): this device's key, the servers it has paired
//! with, pairing from a link, address racing with the signed `/info` probe, signed requests, the
//! catalog and byte ranges.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod auth;
pub mod keys;
pub mod net;
pub mod pair;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HOST, RANGE};
use hyper::{Method, StatusCode};
use p256::ecdsa::{SigningKey, VerifyingKey};
use rustls::ClientConfig;
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::CertificateDer;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

/// This device's key file in the data directory.
pub const KEY_FILE: &str = "client.key";

/// The saved servers in the data directory.
pub const SERVERS_FILE: &str = "servers.json";

/// The most bytes one [`Connection::read`] asks for.
pub const MAX_READ: u64 = 8 * 1024 * 1024;

/// The most addresses raced for one server.
const MAX_ADDRS: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("the OS random source failed")]
    Random,
    #[error("{0} is not a Flicks key file")]
    KeyFile(PathBuf),
    #[error("{0} is not a Flicks server list")]
    ServerList(PathBuf),
    #[error("couldn't encode a key")]
    KeyEncoding,
    #[error("TLS: {0}")]
    Tls(#[from] rustls::Error),
    #[error("HTTP: {0}")]
    Http(#[from] hyper::Error),
    #[error("not a Flicks pairing link")]
    BadLink,
    #[error("not a host:port address")]
    BadAddress,
    #[error("the server answered with a malformed response")]
    BadResponse,
    #[error("that address isn't the server the link names (its key or signature doesn't match)")]
    WrongServer,
    #[error("couldn't reach the server at any of its {tried} addresses (last: {last})")]
    Unreachable { tried: usize, last: Box<Error> },
    #[error("{1} ({0})")]
    Status(u16, String),
}

/// A server this device has paired with.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Server {
    /// The server key, SEC1 compressed, base64url.
    pub key: String,
    /// What to call it: the host it paired through.
    pub name: String,
    /// `host:port` each, raced on every connect.
    pub addrs: Vec<String>,
    /// The user this device belongs to there.
    pub user: String,
    pub admin: bool,
}

impl Server {
    fn verifying_key(&self) -> Result<VerifyingKey, Error> {
        keys::decode(&self.key, keys::PUBLIC_KEY_LEN).and_then(|key| keys::parse_public(&key)).ok_or(Error::KeyEncoding)
    }
}

/// One catalog entry from `GET /items`.
#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    pub item: Item,
    pub progress: Option<Progress>,
}

/// An item: one file (docs/architecture.md, "Catalog API").
#[derive(Debug, Clone, Deserialize)]
pub struct Item {
    pub id: i64,
    pub title: String,
    pub year: Option<u16>,
    pub season: Option<u16>,
    pub episode: Option<u16>,
    pub episode_end: Option<u16>,
    pub edition: Option<String>,
    pub tmdb_id: Option<u64>,
    pub tmdb_media: Option<String>,
    pub poster: Option<String>,
    pub backdrop: Option<String>,
    /// Unix seconds.
    pub added_at: i64,
    pub size: u64,
    pub oshash: String,
    pub container: String,
    pub duration_ms: Option<u64>,
    pub video_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// `hdr10` or `hlg`.
    pub hdr: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    pub position: i64,
    pub played: bool,
    pub updated_at: i64,
}

#[derive(Deserialize)]
struct Info {
    key: String,
    signature: String,
}

/// This device: its key, data directory and saved servers.
pub struct Client {
    dir: PathBuf,
    key: SigningKey,
    pub servers: Vec<Server>,
    web: Arc<WebPkiServerVerifier>,
}

impl Client {
    /// Opens the data directory, creating it (mode 0700) and `client.key` (mode 0600) if needed.
    pub fn open(dir: &Path) -> Result<Client, Error> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir)?;
        let key = keys::load_or_create(&dir.join(KEY_FILE))?;
        let path = dir.join(SERVERS_FILE);
        let servers = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| Error::ServerList(path))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Client { dir: dir.to_path_buf(), key, servers, web: net::web_verifier(&[])? })
    }

    /// Also trusts `authority` for proxies' certificates, besides the public web roots (a private
    /// reverse proxy's own CA).
    pub fn trust(&mut self, authority: CertificateDer<'static>) -> Result<(), Error> {
        self.web = net::web_verifier(&[authority])?;
        Ok(())
    }

    /// This device's public key, SEC1 compressed.
    pub fn public_key(&self) -> Vec<u8> {
        keys::public_bytes(self.key.verifying_key())
    }

    /// Pairs with the server a link names: races its addresses, sends the code proof as
    /// `device_name` to the first that proves it's that server, checks the signed reply, and saves
    /// the server (replacing an earlier entry for the same key).
    pub async fn pair(&mut self, link: &str, device_name: &str) -> Result<Server, Error> {
        let link = pair::parse_link(link.trim()).ok_or(Error::BadLink)?;
        let connection = self.race(&link.server_key, &link.addrs).await?;
        let server_key = keys::public_bytes(&link.server_key);
        let request = pair::Request::new(&self.key, &server_key, &link.code, device_name)?;
        let body = serde_json::to_vec(&request).map_err(|_| Error::BadResponse)?;
        let reply: pair::Reply = connection.exchange(Method::POST, "/pair/redeem", None, body, false).await?;
        let transcript = pair::transcript(&server_key, &self.public_key(), device_name);
        if !pair::verify_reply(&link.server_key, &transcript, &reply) {
            return Err(Error::WrongServer);
        }
        let server = Server {
            key: keys::encode(&server_key),
            name: net::host(&connection.addr).to_string(),
            addrs: link.addrs,
            user: reply.user,
            admin: reply.admin,
        };
        self.servers.retain(|s| s.key != server.key);
        self.servers.push(server.clone());
        self.save()?;
        Ok(server)
    }

    fn save(&self) -> Result<(), Error> {
        let bytes = serde_json::to_vec_pretty(&self.servers).map_err(|_| Error::BadResponse)?;
        let temp = self.dir.join(format!("{SERVERS_FILE}.tmp"));
        std::fs::write(&temp, bytes)?;
        std::fs::rename(&temp, self.dir.join(SERVERS_FILE))?;
        Ok(())
    }

    /// Races every saved address of `server` and keeps the first whose `/info` probe verifies.
    pub async fn connect(&self, server: &Server) -> Result<Connection, Error> {
        self.race(&server.verifying_key()?, &server.addrs).await
    }

    async fn race(&self, server_key: &VerifyingKey, addrs: &[String]) -> Result<Connection, Error> {
        let tls = Arc::new(net::client_config(server_key, self.web.clone())?);
        let mut probes = JoinSet::new();
        for addr in addrs.iter().take(MAX_ADDRS) {
            probes.spawn(probe(addr.clone(), tls.clone(), *server_key));
        }
        let tried = probes.len();
        let mut last = Error::BadAddress;
        while let Some(joined) = probes.join_next().await {
            match joined {
                Ok(Ok(addr)) => {
                    return Ok(Connection {
                        addr,
                        tls,
                        key: self.key.clone(),
                        server_key: keys::public_bytes(server_key),
                    });
                }
                Ok(Err(error)) if telling(&error) >= telling(&last) => last = error,
                _ => {}
            }
        }
        Err(Error::Unreachable { tried, last: Box::new(last) })
    }
}

/// How much an error says about a server: an answer beats a refused handshake, which beats no
/// connection at all, so a failed race reports the answer.
fn telling(error: &Error) -> u8 {
    match error {
        Error::WrongServer | Error::Status(..) | Error::BadResponse => 2,
        Error::Io(e) if e.kind() == std::io::ErrorKind::InvalidData => 1,
        _ => 0,
    }
}

/// `GET /info?probe=<nonce>` at `addr`: returns `addr` if the answer is signed by `server_key`.
async fn probe(addr: String, tls: Arc<ClientConfig>, server_key: VerifyingKey) -> Result<String, Error> {
    let nonce = keys::random::<16>()?;
    let request = hyper::Request::get(format!("/info?probe={}", keys::encode(&nonce)))
        .header(HOST, net::host_header(&addr))
        .body(Full::new(Bytes::new()))
        .map_err(|_| Error::BadAddress)?;
    let response = net::send(&addr, tls, request).await?;
    let info: Info = json(response)?;
    let message = keys::Message::new("flicks-info-v1").field(&nonce).into_bytes();
    let signed = keys::decode(&info.signature, 64).is_some_and(|sig| keys::verify(&server_key, &message, &sig));
    if info.key != keys::encode(&keys::public_bytes(&server_key)) || !signed {
        return Err(Error::WrongServer);
    }
    Ok(addr)
}

/// Decodes a JSON answer; an error status becomes [`Error::Status`] with the server's message.
fn json<T: DeserializeOwned>(response: net::Response) -> Result<T, Error> {
    check(&response)?;
    let body: &[u8] = if response.body.is_empty() { b"null" } else { &response.body };
    serde_json::from_slice(body).map_err(|_| Error::BadResponse)
}

fn check(response: &net::Response) -> Result<(), Error> {
    if response.status.is_success() {
        return Ok(());
    }
    let message = serde_json::from_slice::<serde_json::Value>(&response.body)
        .ok()
        .and_then(|v| v.get("error")?.as_str().map(str::to_string))
        .unwrap_or_else(|| response.status.canonical_reason().unwrap_or("error").to_string());
    Err(Error::Status(response.status.as_u16(), message))
}

/// A server reached at one address that proved it holds the server key.
pub struct Connection {
    /// The winning `host:port`.
    pub addr: String,
    tls: Arc<ClientConfig>,
    key: SigningKey,
    server_key: Vec<u8>,
}

impl Connection {
    async fn send(
        &self,
        method: Method,
        path_and_query: &str,
        range: Option<&str>,
        body: Vec<u8>,
        signed: bool,
    ) -> Result<net::Response, Error> {
        let mut request = hyper::Request::builder()
            .method(method.clone())
            .uri(path_and_query)
            .header(HOST, net::host_header(&self.addr));
        if signed {
            let range = range.unwrap_or("");
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let authorization =
                auth::authorization(&self.key, &self.server_key, method.as_str(), path_and_query, range, &body, now)?;
            request = request.header(AUTHORIZATION, authorization);
        }
        if let Some(range) = range {
            request = request.header(RANGE, range);
        }
        if !body.is_empty() {
            request = request.header(CONTENT_TYPE, "application/json");
        }
        let request = request.body(Full::new(Bytes::from(body))).map_err(|_| Error::BadAddress)?;
        net::send(&self.addr, self.tls.clone(), request).await
    }

    async fn exchange<T: DeserializeOwned>(
        &self,
        method: Method,
        path_and_query: &str,
        range: Option<&str>,
        body: Vec<u8>,
        signed: bool,
    ) -> Result<T, Error> {
        json(self.send(method, path_and_query, range, body, signed).await?)
    }

    /// A signed `GET`, decoded from JSON.
    pub async fn get<T: DeserializeOwned>(&self, path_and_query: &str) -> Result<T, Error> {
        self.exchange(Method::GET, path_and_query, None, Vec::new(), true).await
    }

    /// Every item, a page of 200 at a time.
    pub async fn items(&self) -> Result<Vec<Entry>, Error> {
        const PAGE: usize = 200;
        let mut items = Vec::new();
        // At most 10,000 pages (two million items).
        for page in 0..10_000 {
            let entries: Vec<Entry> = self.get(&format!("/items?limit={PAGE}&offset={}", page * PAGE)).await?;
            let done = entries.len() < PAGE;
            items.extend(entries);
            if done {
                break;
            }
        }
        Ok(items)
    }

    /// Bytes `start..=end` of a file, at most [`MAX_READ`]; fewer when the file ends sooner.
    pub async fn read(&self, file: i64, start: u64, end: u64) -> Result<Bytes, Error> {
        let len = end.checked_sub(start).and_then(|d| d.checked_add(1)).filter(|len| *len <= MAX_READ);
        let Some(len) = len else {
            return Err(Error::Status(416, format!("ranges are 1 to {MAX_READ} bytes")));
        };
        let range = format!("bytes={start}-{end}");
        let response = self.send(Method::GET, &format!("/files/{file}"), Some(&range), Vec::new(), true).await?;
        check(&response)?;
        if response.status != StatusCode::PARTIAL_CONTENT || response.body.len() as u64 > len {
            return Err(Error::BadResponse);
        }
        Ok(response.body)
    }
}
