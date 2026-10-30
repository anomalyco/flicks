//! The server on localhost and behind a local TLS-terminating proxy: pairing, signed requests,
//! replays, alterations, removed devices, and fuzzed unauthenticated routes.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use flicks_server::cli::Client;
use flicks_server::db::{Device, Folder, User};
use flicks_server::server::{self, AddFolder, AddUser, Info, OpenPairing, Pairing, State, Status};
use flicks_server::{auth, http, keys, pair, tls};
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::header::{AUTHORIZATION, HOST, RANGE};
use hyper::{Method, Request, StatusCode};
use p256::ecdsa::SigningKey;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

fn data_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("server-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

struct Server {
    addr: SocketAddr,
    state: Arc<State>,
    root: Client,
}

async fn start(name: &str) -> Server {
    let (addr, state) = server::spawn(&data_dir(name), "127.0.0.1:0".parse().unwrap()).await.unwrap();
    let root = Client::new(state.key.clone(), state.key.verifying_key(), addr).unwrap();
    Server { addr, state, root }
}

fn post_json(path: &str, body: Vec<u8>) -> Request<Full<Bytes>> {
    Request::post(path).header(HOST, "flicks").body(Full::new(Bytes::from(body))).unwrap()
}

fn get(path: &str) -> Request<Full<Bytes>> {
    Request::get(path).header(HOST, "flicks").body(Full::new(Bytes::new())).unwrap()
}

/// Opens a pairing window as root.
async fn open_window(root: &Client, user: &str, admin: bool) -> pair::Link {
    let pairing: Pairing =
        root.call(Method::POST, "/admin/pair", Some(&OpenPairing { user: user.into(), admin })).await.unwrap();
    pair::parse_link(&pairing.link).unwrap()
}

/// Pairs a new device key through `addr` (the server or a proxy) and returns a client for it.
async fn pair_device(server: &Server, via: SocketAddr, tls: Arc<ClientConfig>, user: &str, admin: bool) -> Client {
    let link = open_window(&server.root, user, admin).await;
    let key = keys::generate().unwrap();
    let server_key = keys::public_bytes(&link.server_key);
    let request = pair::Request::new(&key, &server_key, &link.code, "Test Mac").unwrap();
    let response =
        http::send(via, tls.clone(), post_json("/pair/redeem", serde_json::to_vec(&request).unwrap())).await.unwrap();
    assert_eq!(response.status, StatusCode::OK);
    let reply: pair::Reply = serde_json::from_slice(&response.body).unwrap();
    let transcript = pair::transcript(&server_key, &keys::public_bytes(key.verifying_key()), "Test Mac");
    assert!(pair::verify_reply(&link.server_key, &transcript, &reply));
    assert_eq!((reply.user.as_str(), reply.admin), (user, admin));
    Client { key, server_key, addr: via, tls }
}

fn pinned(state: &State) -> Arc<ClientConfig> {
    Arc::new(tls::pinned_client_config(state.key.verifying_key()).unwrap())
}

/// Accepts any certificate, as a client reaching a tunnel's public certificate does; such a
/// client checks the server with the signed `/info` probe instead.
#[derive(Debug)]
struct AnyCert;

impl ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        tls::provider().signature_verification_algorithms.supported_schemes()
    }
}

fn any_cert() -> Arc<ClientConfig> {
    let mut config = ClientConfig::builder_with_provider(tls::provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCert))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

/// What clients sent through the proxy, one entry per connection.
type Recording = Arc<Mutex<Vec<Vec<u8>>>>;

/// A TLS-terminating proxy, like a tunnel: clients reach it under its own certificate, it reaches
/// the server over TLS without checking the server's certificate, and it can read (here: record)
/// everything clients send.
async fn proxy(server: SocketAddr) -> (SocketAddr, Recording) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(tls::server_config(&keys::generate().unwrap()).unwrap()));
    let recording = Recording::default();
    let log = recording.clone();
    tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            let (acceptor, log) = (acceptor.clone(), log.clone());
            tokio::spawn(async move {
                let Ok(client) = acceptor.accept(tcp).await else {
                    return;
                };
                let upstream = TlsConnector::from(any_cert())
                    .connect(ServerName::try_from("flicks").unwrap(), TcpStream::connect(server).await.unwrap())
                    .await
                    .unwrap();
                let entry = {
                    let mut log = log.lock().unwrap();
                    log.push(Vec::new());
                    log.len() - 1
                };
                let (mut client_read, mut client_write) = tokio::io::split(client);
                let (mut upstream_read, mut upstream_write) = tokio::io::split(upstream);
                let up = async {
                    let mut buf = [0u8; 16384];
                    loop {
                        let n = client_read.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        log.lock().unwrap()[entry].extend_from_slice(&buf[..n]);
                        if upstream_write.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                    let _ = upstream_write.shutdown().await;
                };
                let down = async {
                    let _ = tokio::io::copy(&mut upstream_read, &mut client_write).await;
                    let _ = client_write.shutdown().await;
                };
                tokio::join!(up, down);
            });
        }
    });
    (addr, recording)
}

/// Sends raw request bytes over TLS and returns the response's status code.
async fn raw(addr: SocketAddr, bytes: &[u8]) -> u16 {
    let tcp = TcpStream::connect(addr).await.unwrap();
    let mut stream =
        TlsConnector::from(any_cert()).connect(ServerName::try_from("flicks").unwrap(), tcp).await.unwrap();
    stream.write_all(bytes).await.unwrap();
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    while !response.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf).await.unwrap();
        assert!(n > 0, "connection closed before a response");
        response.extend_from_slice(&buf[..n]);
    }
    std::str::from_utf8(&response[9..12]).unwrap().parse().unwrap()
}

/// Replaces `from` with `to` (same length) in a captured request.
fn alter(captured: &[u8], from: &str, to: &str) -> Vec<u8> {
    assert_eq!(from.len(), to.len());
    let at = captured.windows(from.len()).position(|w| w == from.as_bytes()).unwrap();
    let mut altered = captured.to_vec();
    altered[at..at + to.len()].copy_from_slice(to.as_bytes());
    altered
}

/// A request with an explicit `Authorization` value, for exercising the checks one by one.
fn signed_request(
    method: Method,
    path: &str,
    authorization: &str,
    range: Option<&str>,
    body: &[u8],
) -> Request<Full<Bytes>> {
    let mut builder =
        Request::builder().method(method).uri(path).header(HOST, "flicks").header(AUTHORIZATION, authorization);
    if let Some(range) = range {
        builder = builder.header(RANGE, range);
    }
    builder.body(Full::new(Bytes::from(body.to_vec()))).unwrap()
}

fn sign(
    key: &SigningKey,
    server_key: &[u8],
    method: &str,
    path: &str,
    range: &[u8],
    body: &[u8],
    timestamp: u64,
) -> String {
    auth::sign(key, server_key, method, path, range, body, timestamp).unwrap()
}

#[tokio::test]
async fn pairing_works_on_localhost() {
    let server = start("pairing").await;
    let tls = pinned(&server.state);

    // The probe proves the server holds its key.
    let probe = keys::encode(&[9u8; 16]);
    let response = http::send(server.addr, tls.clone(), get(&format!("/info?probe={probe}"))).await.unwrap();
    let info: Info = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(info.key, keys::encode(&server.state.public));
    let signature = keys::decode(&info.signature, 64).unwrap();
    assert!(keys::verify(server.state.key.verifying_key(), &server::info_message(&[9u8; 16]), &signature));

    // No window open: the pairing route answers like any unsigned request.
    let response = http::send(server.addr, tls.clone(), post_json("/pair/redeem", b"{}".to_vec())).await.unwrap();
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);

    let admin = pair_device(&server, server.addr, tls.clone(), "ana", true).await;
    let status: Status = admin.call(Method::GET, "/admin/status", None::<&()>).await.unwrap();
    assert_eq!((status.users, status.devices, status.pairing), (1, 1, None));

    // A viewer is authenticated but not an admin; unknown routes are 404 once signed.
    let viewer = pair_device(&server, server.addr, tls.clone(), "bo", false).await;
    let err = viewer.call::<Status>(Method::GET, "/admin/status", None::<&()>).await.unwrap_err();
    assert!(matches!(err, flicks_server::Error::Status(403, _)), "{err}");
    let err = viewer.call::<()>(Method::GET, "/nothing", None::<&()>).await.unwrap_err();
    assert!(matches!(err, flicks_server::Error::Status(404, _)), "{err}");

    // Redeeming twice with the same key is a no-op; any other key gets "used".
    let link = open_window(&server.root, "cy", false).await;
    let server_key = keys::public_bytes(&link.server_key);
    let first = keys::generate().unwrap();
    let body = serde_json::to_vec(&pair::Request::new(&first, &server_key, &link.code, "TV").unwrap()).unwrap();
    let a = http::send(server.addr, tls.clone(), post_json("/pair/redeem", body.clone())).await.unwrap();
    let b = http::send(server.addr, tls.clone(), post_json("/pair/redeem", body)).await.unwrap();
    assert_eq!((a.status, b.status), (StatusCode::OK, StatusCode::OK));
    assert_eq!(a.body, b.body);
    let second = keys::generate().unwrap();
    let body = serde_json::to_vec(&pair::Request::new(&second, &server_key, &link.code, "TV").unwrap()).unwrap();
    let c = http::send(server.addr, tls.clone(), post_json("/pair/redeem", body)).await.unwrap();
    assert_eq!(c.status, StatusCode::CONFLICT);

    let devices: Vec<Device> = admin.call(Method::GET, "/admin/devices", None::<&()>).await.unwrap();
    assert_eq!(devices.len(), 3);
    let users: Vec<User> = server.root.call(Method::GET, "/admin/users", None::<&()>).await.unwrap();
    let names: Vec<_> = users.iter().map(|u| (u.name.as_str(), u.admin)).collect();
    assert_eq!(names, [("ana", true), ("bo", false), ("cy", false)]);
    let err = server
        .root
        .call::<User>(Method::POST, "/admin/users", Some(&AddUser { name: "ana".into(), admin: false }))
        .await
        .unwrap_err();
    assert!(matches!(err, flicks_server::Error::Status(409, _)), "{err}");

    // Folders: added once, refused in protected places, removed by id.
    let media = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let add = |path: &Path| AddFolder { path: path.to_string_lossy().into_owned() };
    let folder: Folder = admin.call(Method::POST, "/admin/folders", Some(&add(&media))).await.unwrap();
    for (path, code) in [(media.as_path(), 409), (Path::new("/etc"), 403)] {
        let err = admin.call::<Folder>(Method::POST, "/admin/folders", Some(&add(path))).await.unwrap_err();
        assert!(matches!(err, flicks_server::Error::Status(c, _) if c == code), "{err}");
    }
    let err =
        admin.call::<Folder>(Method::POST, "/admin/folders", Some(&add(&server.state.data_dir))).await.unwrap_err();
    assert!(matches!(err, flicks_server::Error::Status(403, _)), "{err}");
    admin.call::<()>(Method::DELETE, &format!("/admin/folders/{}", folder.id), None::<&()>).await.unwrap();
    let folders: Vec<Folder> = admin.call(Method::GET, "/admin/folders", None::<&()>).await.unwrap();
    assert!(folders.is_empty());
}

#[tokio::test]
async fn pairing_and_requests_through_a_tls_terminating_proxy() {
    let server = start("proxy").await;
    let (via, recording) = proxy(server.addr).await;
    let tls = any_cert();

    // Behind the proxy the certificate proves nothing; the probe does.
    let link = open_window(&server.root, "dee", true).await;
    let probe = keys::random::<16>().unwrap();
    let response = http::send(via, tls.clone(), get(&format!("/info?probe={}", keys::encode(&probe)))).await.unwrap();
    let info: Info = serde_json::from_slice(&response.body).unwrap();
    assert!(keys::verify(&link.server_key, &server::info_message(&probe), &keys::decode(&info.signature, 64).unwrap()));

    // Pair through the proxy, which records the request.
    let server_key = keys::public_bytes(&link.server_key);
    let device = keys::generate().unwrap();
    let request = pair::Request::new(&device, &server_key, &link.code, "Phone").unwrap();
    let response =
        http::send(via, tls.clone(), post_json("/pair/redeem", serde_json::to_vec(&request).unwrap())).await.unwrap();
    assert_eq!(response.status, StatusCode::OK);
    let captured = recording.lock().unwrap().last().unwrap().clone();
    let body_at = captured.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let relayed: pair::Request = serde_json::from_slice(&captured[body_at..]).unwrap();

    // The proxy can't redeem the relayed request for its own key: not by re-signing the proof,
    // not under another name, and not by replaying it before the device does.
    let thief = keys::generate().unwrap();
    let thief_key = keys::public_bytes(thief.verifying_key());
    let proof = keys::decode(&relayed.proof, pair::PROOF_LEN).unwrap();
    for name in ["Phone", "Thief"] {
        let transcript = pair::transcript(&server_key, &thief_key, name);
        let forged = pair::Request {
            key: keys::encode(&thief_key),
            name: name.into(),
            proof: relayed.proof.clone(),
            signature: keys::encode(&keys::sign(&thief, &pair::request_message(&transcript, &proof))),
        };
        for addr in [via, server.addr] {
            let response =
                http::send(addr, tls.clone(), post_json("/pair/redeem", serde_json::to_vec(&forged).unwrap()))
                    .await
                    .unwrap();
            assert_eq!(response.status, StatusCode::FORBIDDEN, "{name}");
        }
    }
    assert_eq!(raw(server.addr, &captured).await, 200, "replayed for the same key: a no-op");
    let late = open_window(&server.root, "dee", true).await;
    let replayed = raw(server.addr, &captured).await;
    assert_eq!(replayed, 403, "a relayed request is bound to its own window's code");
    drop(late);
    let devices: Vec<Device> = server.root.call(Method::GET, "/admin/devices", None::<&()>).await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].key, keys::encode(&keys::public_bytes(device.verifying_key())));

    // Signed requests through the proxy work, and what it records can't be replayed or altered.
    let client = Client { key: device, server_key: server_key.clone(), addr: via, tls: tls.clone() };
    let status: Status = client.call(Method::GET, "/admin/status", None::<&()>).await.unwrap();
    assert_eq!(status.devices, 1);
    let captured = recording.lock().unwrap().last().unwrap().clone();
    assert_eq!(raw(via, &captured).await, 401, "replayed through the proxy");
    assert_eq!(raw(server.addr, &captured).await, 401, "replayed directly");

    let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let _: Folder = client
        .call(Method::POST, "/admin/folders", Some(&AddFolder { path: folder.to_string_lossy().into_owned() }))
        .await
        .unwrap();
    let captured = recording.lock().unwrap().last().unwrap().clone();
    // A fresh nonce for each alteration, so only the alteration can be what fails.
    for (from, to) in [("POST", "COPY"), ("/admin/folders", "/admin/foldery"), ("tests", "testz")] {
        let altered = alter(&captured, from, to);
        let header_end = altered.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let text = String::from_utf8(altered[..header_end].to_vec()).unwrap();
        let old_nonce = text
            .lines()
            .find_map(|l| l.strip_prefix("authorization: Flicks "))
            .unwrap()
            .split(' ')
            .nth(2)
            .unwrap()
            .to_string();
        let fresh = alter(&altered, &old_nonce, &keys::encode(&keys::random::<16>().unwrap()));
        assert_eq!(raw(via, &fresh).await, 401, "{from} -> {to}");
    }

    // Removed: its next request through the proxy is refused.
    server.root.call::<()>(Method::DELETE, &format!("/admin/devices/{}", devices[0].id), None::<&()>).await.unwrap();
    let err = client.call::<Status>(Method::GET, "/admin/status", None::<&()>).await.unwrap_err();
    assert!(matches!(err, flicks_server::Error::Status(401, _)), "{err}");
}

#[tokio::test]
async fn replayed_altered_stale_and_removed_requests_fail() {
    let server = start("requests").await;
    let tls = pinned(&server.state);
    let device = pair_device(&server, server.addr, tls.clone(), "eve", true).await;
    let key = &device.key;
    let sk = &device.server_key;
    let send = |request| http::send(server.addr, tls.clone(), request);
    let now = flicks_server::unix_now();

    let auth = sign(key, sk, "GET", "/admin/status", b"", b"", now);
    let ok = send(signed_request(Method::GET, "/admin/status", &auth, None, b"")).await.unwrap();
    assert_eq!(ok.status, StatusCode::OK);
    let replay = send(signed_request(Method::GET, "/admin/status", &auth, None, b"")).await.unwrap();
    assert_eq!(replay.status, StatusCode::UNAUTHORIZED, "replayed");

    // Each part of the request is covered by the signature.
    let cases: [(&str, Method, &str, Option<&str>, &[u8]); 6] = [
        ("method", Method::DELETE, "/admin/status", None, b""),
        ("path", Method::GET, "/admin/users", None, b""),
        ("query", Method::GET, "/admin/status?x=1", None, b""),
        ("range", Method::GET, "/admin/status", Some("bytes=0-1"), b""),
        ("body", Method::GET, "/admin/status", None, b"{}"),
        ("range removed", Method::GET, "/admin/status", None, b""),
    ];
    for (what, method, path, range, body) in cases {
        let signed_range: &[u8] = if what == "range removed" { b"bytes=0-1" } else { b"" };
        let auth = sign(key, sk, "GET", "/admin/status", signed_range, b"", now);
        let response = send(signed_request(method, path, &auth, range, body)).await.unwrap();
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "altered {what}");
    }
    // Signed for another server.
    let auth = sign(key, &[2; 33], "GET", "/admin/status", b"", b"", now);
    let response = send(signed_request(Method::GET, "/admin/status", &auth, None, b"")).await.unwrap();
    assert_eq!(response.status, StatusCode::UNAUTHORIZED, "another server");
    // A range is fine when it is signed.
    let auth = sign(key, sk, "GET", "/admin/status", b"bytes=0-1", b"", now);
    let response = send(signed_request(Method::GET, "/admin/status", &auth, Some("bytes=0-1"), b"")).await.unwrap();
    assert_eq!(response.status, StatusCode::OK);

    // Timestamps: within 60 s either way.
    for (offset, code) in [(-61, 401), (61, 401), (-59, 200), (59, 200)] {
        let at = now.saturating_add_signed(offset);
        let auth = sign(key, sk, "GET", "/admin/status", b"", b"", at);
        let response = send(signed_request(Method::GET, "/admin/status", &auth, None, b"")).await.unwrap();
        assert_eq!(response.status.as_u16(), code, "offset {offset}");
    }

    // An unknown key is refused; so is a body over 64 KiB, before it is read.
    let stranger = keys::generate().unwrap();
    let auth = sign(&stranger, sk, "GET", "/admin/status", b"", b"", now);
    let response = send(signed_request(Method::GET, "/admin/status", &auth, None, b"")).await.unwrap();
    assert_eq!(response.status, StatusCode::UNAUTHORIZED, "unknown key");
    let big = vec![b'x'; auth::MAX_BODY + 1];
    let auth = sign(key, sk, "POST", "/admin/users", b"", &big, now);
    let response = send(signed_request(Method::POST, "/admin/users", &auth, None, &big)).await.unwrap();
    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);

    // Removing the device takes effect on its next request; root is unaffected.
    let devices: Vec<Device> = device.call(Method::GET, "/admin/devices", None::<&()>).await.unwrap();
    server.root.call::<()>(Method::DELETE, &format!("/admin/devices/{}", devices[0].id), None::<&()>).await.unwrap();
    let err = device.call::<Status>(Method::GET, "/admin/status", None::<&()>).await.unwrap_err();
    assert!(matches!(err, flicks_server::Error::Status(401, _)), "{err}");
    let _: Status = server.root.call(Method::GET, "/admin/status", None::<&()>).await.unwrap();
}

/// A small deterministic generator for fuzz inputs.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

/// Truncations, every single-bit flip, and random junk of `valid`.
fn mutations(valid: &[u8], rng: &mut XorShift) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = (0..valid.len()).map(|len| valid[..len].to_vec()).collect();
    for i in 0..valid.len() {
        for bit in 0..8 {
            let mut flipped = valid.to_vec();
            flipped[i] ^= 1 << bit;
            out.push(flipped);
        }
    }
    for len in [1, 2, 7, 33, 64, 200, 1000, 70_000] {
        out.push(rng.bytes(len));
    }
    out
}

#[tokio::test]
async fn unauthenticated_routes_fuzzed() {
    let server = start("fuzz").await;
    let state = &server.state;
    let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);

    // Pairing redemption: nothing but the exact request redeems the window.
    let link = open_window(&server.root, "fuzz", false).await;
    let device = keys::generate().unwrap();
    let request = pair::Request::new(&device, &state.public, &link.code, "Fuzz").unwrap();
    let valid = serde_json::to_vec(&request).unwrap();
    let mut cases = mutations(&valid, &mut rng);
    let oversized = |field: &str| {
        let mut r = request.clone();
        let long = "A".repeat(100_000);
        match field {
            "key" => r.key = long,
            "name" => r.name = long,
            "proof" => r.proof = long,
            _ => r.signature = long,
        }
        serde_json::to_vec(&r).unwrap()
    };
    for field in ["key", "name", "proof", "signature"] {
        cases.push(oversized(field));
    }
    cases.push(b"null".to_vec());
    cases.push(b"[]".to_vec());
    cases.push(br#"{"key":1,"name":[],"proof":{},"signature":null}"#.to_vec());
    let mut refused = [0usize; 2];
    for case in &cases {
        let status = server::redeem(state, case).await.status();
        match status {
            StatusCode::BAD_REQUEST => refused[0] += 1,
            StatusCode::FORBIDDEN => refused[1] += 1,
            other => panic!("{other} for {:?}", String::from_utf8_lossy(case)),
        }
    }
    assert_eq!(refused.iter().sum::<usize>(), cases.len());
    assert!(refused[1] > 100, "proof or signature refusals: {refused:?}");
    assert_eq!(server::redeem(state, &valid).await.status(), StatusCode::OK);

    // The probe: never panics, and every answer it gives is a valid signature.
    let valid_query = format!("probe={}", keys::encode(&[5u8; 24]));
    let mut answered = 0;
    for case in mutations(valid_query.as_bytes(), &mut rng) {
        let query = String::from_utf8_lossy(&case);
        let reply = server::info(state, Some(&query));
        if reply.status() == StatusCode::OK {
            answered += 1;
        } else {
            assert_eq!(reply.status(), StatusCode::BAD_REQUEST);
        }
    }
    assert!(answered > 0);
    assert_eq!(server::info(state, None).status(), StatusCode::BAD_REQUEST);

    // The Authorization header: every mutation of a valid one is refused, then the original
    // passes once.
    let now = flicks_server::unix_now();
    let auth = sign(&server.root.key, &state.public, "GET", "/admin/status", b"", b"", now);
    for case in mutations(auth.as_bytes(), &mut rng) {
        let mut request = get("/admin/status");
        if let Ok(value) = hyper::header::HeaderValue::from_bytes(&case) {
            request.headers_mut().insert(AUTHORIZATION, value);
        }
        let status = server::handle(state, request).await.status();
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{:?}", String::from_utf8_lossy(&case));
    }
    let request = signed_request(Method::GET, "/admin/status", &auth, None, b"");
    assert_eq!(server::handle(state, request).await.status(), StatusCode::OK);

    // On the wire: junk before and inside TLS doesn't take the server down.
    for len in [0, 1, 5, 64, 517, 4096] {
        let mut tcp = TcpStream::connect(server.addr).await.unwrap();
        let _ = tcp.write_all(&rng.bytes(len)).await;
        drop(tcp);
        let tcp = TcpStream::connect(server.addr).await.unwrap();
        let mut stream =
            TlsConnector::from(any_cert()).connect(ServerName::try_from("flicks").unwrap(), tcp).await.unwrap();
        if len > 0 {
            // The server answers junk HTTP with an error and closes, rather than waiting.
            stream.write_all(&rng.bytes(len)).await.unwrap();
            let mut answer = Vec::new();
            let _ = stream.read_to_end(&mut answer).await;
            assert!(answer.is_empty() || answer.starts_with(b"HTTP/1.1 4"), "{len}");
        }
    }
    let mut answered = 0;
    let mut limited = 0;
    for _ in 0..40 {
        let response = http::send(server.addr, pinned(state), get("/info?probe=AAAAAAAAAAAAAAAAAAAAAA")).await.unwrap();
        match response.status {
            StatusCode::OK => answered += 1,
            StatusCode::TOO_MANY_REQUESTS => limited += 1,
            other => panic!("{other}"),
        }
    }
    // The global limit lets a burst of 20 (plus refill) through and refuses the rest.
    assert!((20..40).contains(&answered) && limited > 0, "{answered} answered, {limited} limited");
}

#[test]
fn the_server_key_file_is_private_and_stable() {
    let dir = data_dir("keyfile");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("server.key");
    let key = keys::load_or_create(&path).unwrap();
    assert_eq!(keys::load_or_create(&path).unwrap().to_bytes(), key.to_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    std::fs::write(&path, b"short").unwrap();
    assert!(keys::load(&path).is_err());
}
