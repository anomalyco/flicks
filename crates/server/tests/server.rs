//! The server on localhost and behind a local TLS-terminating proxy: pairing, signed requests,
//! replays, alterations, removed devices, and fuzzed unauthenticated routes.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use flicks_client::{Connection, net};
use flicks_server::db::{Device, Folder, User};
use flicks_server::server::{self, AddFolder, AddUser, Info, OpenPairing, Pairing, State, Status};
use flicks_server::{auth, keys, pair, tls};
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
    root: Connection,
}

async fn start(name: &str) -> Server {
    let (addr, state) = server::spawn(&data_dir(name), "127.0.0.1:0".parse().unwrap()).await.unwrap();
    let root = Connection::pinned(state.key.clone(), state.key.verifying_key(), addr.to_string()).unwrap();
    Server { addr, state, root }
}

fn post_json(path: &str, body: Vec<u8>) -> Request<Full<Bytes>> {
    Request::post(path).header(HOST, "flicks").body(Full::new(Bytes::from(body))).unwrap()
}

fn get(path: &str) -> Request<Full<Bytes>> {
    Request::get(path).header(HOST, "flicks").body(Full::new(Bytes::new())).unwrap()
}

/// Opens a pairing window as root.
async fn open_window(root: &Connection, user: &str, admin: bool) -> pair::Link {
    let request = OpenPairing { user: user.into(), admin, addrs: Vec::new() };
    let pairing: Pairing = root.call(Method::POST, "/admin/pair", Some(&request)).await.unwrap();
    pair::parse_link(&pairing.link).unwrap()
}

/// Pairs a new device key through `addr` (the server or a proxy) and returns a client for it.
async fn pair_device(server: &Server, via: SocketAddr, tls: Arc<ClientConfig>, user: &str, admin: bool) -> Connection {
    let link = open_window(&server.root, user, admin).await;
    let key = keys::generate().unwrap();
    let server_key = keys::public_bytes(&link.server_key);
    let request = pair::Request::new(&key, &server_key, &link.code, "Test Mac").unwrap();
    let response =
        https(via, tls.clone(), post_json("/pair/redeem", serde_json::to_vec(&request).unwrap())).await.unwrap();
    assert_eq!(response.status, StatusCode::OK);
    let reply: pair::Reply = serde_json::from_slice(&response.body).unwrap();
    let transcript = pair::transcript(&server_key, &keys::public_bytes(key.verifying_key()), "Test Mac");
    assert!(pair::verify_reply(&link.server_key, &transcript, &reply));
    assert_eq!((reply.user.as_str(), reply.admin), (user, admin));
    // The user and admin flag are signed too.
    let tampered = pair::Reply { admin: !admin, ..reply.clone() };
    assert!(!pair::verify_reply(&link.server_key, &transcript, &tampered));
    let tampered = pair::Reply { user: format!("{user}x"), ..reply };
    assert!(!pair::verify_reply(&link.server_key, &transcript, &tampered));
    Connection { key, server_key, addr: via.to_string(), tls }
}

fn pinned(state: &State) -> Arc<ClientConfig> {
    Arc::new(net::client_config(state.key.verifying_key(), net::web_verifier(&[]).unwrap()).unwrap())
}

/// One request over a new TLS connection to `addr`.
async fn https(
    addr: SocketAddr,
    tls: Arc<ClientConfig>,
    request: Request<Full<Bytes>>,
) -> Result<net::Response, flicks_client::Error> {
    net::send(&addr.to_string(), tls, request).await
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
    flicks_client::auth::sign(key, server_key, method, path, range, body, timestamp).unwrap()
}

#[tokio::test]
async fn pairing_works_on_localhost() {
    let server = start("pairing").await;
    let tls = pinned(&server.state);

    // The probe proves the server holds its key, and signs its name.
    let probe = keys::encode(&[9u8; 16]);
    let response = https(server.addr, tls.clone(), get(&format!("/info?probe={probe}"))).await.unwrap();
    let info: Info = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(info.key, keys::encode(&server.state.public));
    assert_eq!(info.name, server.state.name);
    assert!(pair::valid_name(&info.name), "{:?}", info.name);
    let signature = keys::decode(&info.signature, 64).unwrap();
    let key = server.state.key.verifying_key();
    assert!(keys::verify(key, &server::info_message(&[9u8; 16], &info.name), &signature));
    assert!(!keys::verify(key, &server::info_message(&[9u8; 16], "Someone else"), &signature));

    // No window open: the pairing route answers like any unsigned request.
    let response = https(server.addr, tls.clone(), post_json("/pair/redeem", b"{}".to_vec())).await.unwrap();
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);

    let admin = pair_device(&server, server.addr, tls.clone(), "ana", true).await;
    let status: Status = admin.call(Method::GET, "/admin/status", None::<&()>).await.unwrap();
    assert_eq!((status.users, status.devices, status.pairing), (1, 1, None));

    // A viewer is authenticated but not an admin; unknown routes are 404 once signed.
    let viewer = pair_device(&server, server.addr, tls.clone(), "bo", false).await;
    let err = viewer.call::<Status>(Method::GET, "/admin/status", None::<&()>).await.unwrap_err();
    assert!(matches!(err, flicks_client::Error::Status(403, _)), "{err}");
    let err = viewer.call::<()>(Method::GET, "/nothing", None::<&()>).await.unwrap_err();
    assert!(matches!(err, flicks_client::Error::Status(404, _)), "{err}");

    // Redeeming twice with the same key is a no-op; any other key gets "used".
    let link = open_window(&server.root, "cy", false).await;
    let server_key = keys::public_bytes(&link.server_key);
    let first = keys::generate().unwrap();
    let body = serde_json::to_vec(&pair::Request::new(&first, &server_key, &link.code, "TV").unwrap()).unwrap();
    let a = https(server.addr, tls.clone(), post_json("/pair/redeem", body.clone())).await.unwrap();
    let b = https(server.addr, tls.clone(), post_json("/pair/redeem", body)).await.unwrap();
    assert_eq!((a.status, b.status), (StatusCode::OK, StatusCode::OK));
    assert_eq!(a.body, b.body);
    let second = keys::generate().unwrap();
    let body = serde_json::to_vec(&pair::Request::new(&second, &server_key, &link.code, "TV").unwrap()).unwrap();
    let c = https(server.addr, tls.clone(), post_json("/pair/redeem", body)).await.unwrap();
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
    assert!(matches!(err, flicks_client::Error::Status(409, _)), "{err}");

    // Tunnel or tailnet names go before the detected addresses in the link; malformed ones are
    // refused.
    let request = OpenPairing { user: "cy".into(), admin: false, addrs: vec!["nas.tail1234.ts.net:7686".into()] };
    let pairing: Pairing = server.root.call(Method::POST, "/admin/pair", Some(&request)).await.unwrap();
    let link = pair::parse_link(&pairing.link).unwrap();
    assert_eq!(link.addrs.first().map(String::as_str), Some("nas.tail1234.ts.net:7686"));
    assert!(link.addrs.len() > 1, "and the detected addresses after it: {:?}", link.addrs);
    let request = OpenPairing { user: "cy".into(), admin: false, addrs: vec!["nas&code=x:7686".into()] };
    let err = server.root.call::<Pairing>(Method::POST, "/admin/pair", Some(&request)).await.unwrap_err();
    assert!(matches!(err, flicks_client::Error::Status(400, _)), "{err}");

    // Folders: added once, refused in protected places, removed by id.
    let media = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let add = |path: &Path| AddFolder { path: path.to_string_lossy().into_owned() };
    let folder: Folder = admin.call(Method::POST, "/admin/folders", Some(&add(&media))).await.unwrap();
    for (path, code) in [(media.as_path(), 409), (Path::new("/etc"), 403)] {
        let err = admin.call::<Folder>(Method::POST, "/admin/folders", Some(&add(path))).await.unwrap_err();
        assert!(matches!(err, flicks_client::Error::Status(c, _) if c == code), "{err}");
    }
    let err =
        admin.call::<Folder>(Method::POST, "/admin/folders", Some(&add(&server.state.data_dir))).await.unwrap_err();
    assert!(matches!(err, flicks_client::Error::Status(403, _)), "{err}");
    // A temp folder is fine (on macOS it lives under /var); the home folder and its parents aren't.
    let temp = tempfile::tempdir().unwrap();
    let _: Folder = admin.call(Method::POST, "/admin/folders", Some(&add(temp.path()))).await.unwrap();
    if let Some(home) = flicks_server::home_dir() {
        for path in home.ancestors() {
            let err = admin.call::<Folder>(Method::POST, "/admin/folders", Some(&add(path))).await.unwrap_err();
            assert!(matches!(err, flicks_client::Error::Status(403, _)), "{}: {err}", path.display());
        }
    }
    admin.call::<()>(Method::DELETE, &format!("/admin/folders/{}", folder.id), None::<&()>).await.unwrap();
    let folders: Vec<Folder> = admin.call(Method::GET, "/admin/folders", None::<&()>).await.unwrap();
    assert_eq!(folders.len(), 1);
}

#[tokio::test]
async fn pairing_and_requests_through_a_tls_terminating_proxy() {
    let server = start("proxy").await;
    let (via, recording) = proxy(server.addr).await;
    let tls = any_cert();

    // Behind the proxy the certificate proves nothing; the probe does.
    let link = open_window(&server.root, "dee", true).await;
    let probe = keys::random::<16>().unwrap();
    let response = https(via, tls.clone(), get(&format!("/info?probe={}", keys::encode(&probe)))).await.unwrap();
    let info: Info = serde_json::from_slice(&response.body).unwrap();
    let signature = keys::decode(&info.signature, 64).unwrap();
    assert!(keys::verify(&link.server_key, &server::info_message(&probe, &info.name), &signature));

    // Pair through the proxy, which records the request.
    let server_key = keys::public_bytes(&link.server_key);
    let device = keys::generate().unwrap();
    let request = pair::Request::new(&device, &server_key, &link.code, "Phone").unwrap();
    let response =
        https(via, tls.clone(), post_json("/pair/redeem", serde_json::to_vec(&request).unwrap())).await.unwrap();
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
            let response = https(addr, tls.clone(), post_json("/pair/redeem", serde_json::to_vec(&forged).unwrap()))
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
    let client = Connection { key: device, server_key: server_key.clone(), addr: via.to_string(), tls: tls.clone() };
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
    assert!(matches!(err, flicks_client::Error::Status(401, _)), "{err}");
}

#[tokio::test]
async fn replayed_altered_stale_and_removed_requests_fail() {
    let server = start("requests").await;
    let tls = pinned(&server.state);
    let device = pair_device(&server, server.addr, tls.clone(), "eve", true).await;
    let key = &device.key;
    let sk = &device.server_key;
    let send = |request| https(server.addr, tls.clone(), request);

    let auth = sign(key, sk, "GET", "/admin/status", b"", b"", flicks_server::unix_now());
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
        let auth = sign(key, sk, "GET", "/admin/status", signed_range, b"", flicks_server::unix_now());
        let response = send(signed_request(method, path, &auth, range, body)).await.unwrap();
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "altered {what}");
    }
    // Signed for another server.
    let auth = sign(key, &[2; 33], "GET", "/admin/status", b"", b"", flicks_server::unix_now());
    let response = send(signed_request(Method::GET, "/admin/status", &auth, None, b"")).await.unwrap();
    assert_eq!(response.status, StatusCode::UNAUTHORIZED, "another server");
    // A range is fine when it is signed.
    let auth = sign(key, sk, "GET", "/admin/status", b"bytes=0-1", b"", flicks_server::unix_now());
    let response = send(signed_request(Method::GET, "/admin/status", &auth, Some("bytes=0-1"), b"")).await.unwrap();
    assert_eq!(response.status, StatusCode::OK);

    // Timestamps: within 60 s either way. Each is taken fresh, two seconds from the edge, so a
    // slow runner crossing a second or two between signing and checking can't flip the answer.
    for (offset, code) in [(-62, 401), (62, 401), (-58, 200), (58, 200)] {
        let at = flicks_server::unix_now().saturating_add_signed(offset);
        let auth = sign(key, sk, "GET", "/admin/status", b"", b"", at);
        let response = send(signed_request(Method::GET, "/admin/status", &auth, None, b"")).await.unwrap();
        assert_eq!(response.status.as_u16(), code, "offset {offset}");
    }

    // An unknown key is refused; so is a body over 64 KiB, before it is read.
    let stranger = keys::generate().unwrap();
    let auth = sign(&stranger, sk, "GET", "/admin/status", b"", b"", flicks_server::unix_now());
    let response = send(signed_request(Method::GET, "/admin/status", &auth, None, b"")).await.unwrap();
    assert_eq!(response.status, StatusCode::UNAUTHORIZED, "unknown key");
    let big = vec![b'x'; auth::MAX_BODY + 1];
    let auth = sign(key, sk, "POST", "/admin/users", b"", &big, flicks_server::unix_now());
    let response = send(signed_request(Method::POST, "/admin/users", &auth, None, &big)).await.unwrap();
    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);

    // Removing the device takes effect on its next request; root is unaffected.
    let devices: Vec<Device> = device.call(Method::GET, "/admin/devices", None::<&()>).await.unwrap();
    server.root.call::<()>(Method::DELETE, &format!("/admin/devices/{}", devices[0].id), None::<&()>).await.unwrap();
    let err = device.call::<Status>(Method::GET, "/admin/status", None::<&()>).await.unwrap_err();
    assert!(matches!(err, flicks_client::Error::Status(401, _)), "{err}");
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
        let response = https(server.addr, pinned(state), get("/info?probe=AAAAAAAAAAAAAAAAAAAAAA")).await.unwrap();
        match response.status {
            StatusCode::OK => answered += 1,
            StatusCode::TOO_MANY_REQUESTS => limited += 1,
            other => panic!("{other}"),
        }
    }
    // The global limit lets a burst of 20 (plus refill) through and refuses the rest.
    assert!((20..40).contains(&answered) && limited > 0, "{answered} answered, {limited} limited");
}

/// A body that never arrives.
struct Stalled;

impl hyper::body::Body for Stalled {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Self::Error>>> {
        std::task::Poll::Pending
    }
}

#[tokio::test(start_paused = true)]
async fn duplicate_headers_and_stalled_bodies_are_refused() {
    let state = State::open(&data_dir("stalled"), 0).unwrap();
    let auth = |method, range: &[u8]| {
        sign(&state.key, &state.public, method, "/admin/status", range, b"", flicks_server::unix_now())
    };

    // Each request would pass with its first header alone.
    let mut request = signed_request(Method::GET, "/admin/status", &auth("GET", b""), None, b"");
    request.headers_mut().append(AUTHORIZATION, auth("GET", b"").parse().unwrap());
    assert_eq!(server::handle(&state, request).await.status(), StatusCode::BAD_REQUEST, "two Authorization");
    let mut request = signed_request(Method::GET, "/admin/status", &auth("GET", b"bytes=0-1"), Some("bytes=0-1"), b"");
    request.headers_mut().append(RANGE, "bytes=2-3".parse().unwrap());
    assert_eq!(server::handle(&state, request).await.status(), StatusCode::BAD_REQUEST, "two Range");

    // The paused clock jumps ahead to the body timeout.
    let request =
        Request::post("/admin/status").header(HOST, "flicks").header(AUTHORIZATION, auth("POST", b"")).body(Stalled);
    assert_eq!(server::handle(&state, request.unwrap()).await.status(), StatusCode::REQUEST_TIMEOUT);
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

fn catalog_mp4() -> Vec<u8> {
    use flicks_container::isobmff::{
        AvcConfig, Brand, Mp4Writer, SampleEntry, TrackConfig, WriteSample, WriterOptions,
    };
    let avc = AvcConfig::new(vec![vec![0x67, 0x64, 0, 0x28, 1, 2]], vec![vec![0x68, 3]], 4);
    let mut w = Mp4Writer::new(std::io::Cursor::new(Vec::new()), WriterOptions::new(Brand::Mp4)).unwrap();
    let v = w.add_track(TrackConfig::new(SampleEntry::avc(avc, 1280, 720), 1000)).unwrap();
    let a = w.add_track(TrackConfig::new(SampleEntry::aac(vec![0x11, 0x90], 2, 48000), 48000)).unwrap();
    for second in 0..5 {
        w.write_sample(
            v,
            WriteSample { data: &[second; 100_000], duration: 1000, composition_offset: 0, is_sync: true },
        )
        .unwrap();
        w.write_sample(a, WriteSample { data: &[2; 10], duration: 48000, composition_offset: 0, is_sync: true })
            .unwrap();
    }
    w.finish().unwrap().into_inner()
}

async fn catalog_ready(client: &Connection, count: usize) -> Vec<serde_json::Value> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let items: Vec<serde_json::Value> = client.call(Method::GET, "/items", None::<&()>).await.unwrap();
            if items.len() == count {
                return items;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
}

async fn api_request(server: &Server, client: &Connection, path: &str, range: Option<&str>) -> server::Reply {
    let auth = sign(
        &client.key,
        &client.server_key,
        "GET",
        path,
        range.unwrap_or("").as_bytes(),
        b"",
        flicks_server::unix_now(),
    );
    server::handle(&server.state, signed_request(Method::GET, path, &auth, range, b"")).await
}

#[tokio::test]
async fn catalog_play_ranges_and_private_progress() {
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    let server = start("catalog").await;
    let viewer = pair_device(&server, server.addr, pinned(&server.state), "viewer", false).await;
    let other = pair_device(&server, server.addr, pinned(&server.state), "other", false).await;
    let media = tempfile::tempdir().unwrap();
    let bytes = catalog_mp4();
    std::fs::write(media.path().join("Example (2020).mp4"), &bytes).unwrap();
    let folder: Folder = server
        .root
        .call(Method::POST, "/admin/folders", Some(&AddFolder { path: media.path().to_string_lossy().into_owned() }))
        .await
        .unwrap();
    let items = catalog_ready(&viewer, 1).await;
    let id = items[0]["item"]["id"].as_i64().unwrap();
    assert_eq!(items[0]["item"]["title"], "Example");
    assert_eq!(items[0]["progress"], Value::Null);
    assert!(items[0]["item"].get("path").is_none());
    let libs: Value = viewer.call(Method::GET, "/libraries", None::<&()>).await.unwrap();
    assert_eq!(libs[0]["id"], folder.id);
    let found: Value = viewer.call(Method::GET, "/search?q=Ex%61mple&limit=1", None::<&()>).await.unwrap();
    assert_eq!(found.as_array().unwrap().len(), 1);
    let found: Value =
        viewer.call(Method::GET, &format!("/items?library={}&offset=1", folder.id), None::<&()>).await.unwrap();
    assert_eq!(found, json!([]));
    for path in ["/search?q=%GG", "/items?limit=no", "/items?library=-1", "/progress?user=1"] {
        assert_eq!(api_request(&server, &viewer, path, None).await.status(), StatusCode::BAD_REQUEST);
    }
    let detail: Value = viewer.call(Method::GET, &format!("/items/{id}"), None::<&()>).await.unwrap();
    assert_eq!(detail["streams"][0]["codec"], "aac");
    assert_eq!(detail["item"]["video_codec"], "h264");
    // Clients see local cache names and fetch signed artwork without knowing about TMDB.
    let art = server.state.data_dir.join("artwork");
    std::fs::create_dir_all(&art).unwrap();
    let jpeg = [0xff, 0xd8, 0xff, 0xe0, 1, 2, 3];
    std::fs::write(art.join("w342-p1.jpg"), jpeg).unwrap();
    server
        .state
        .db
        .call(move |conn| {
            conn.execute("UPDATE library_files SET poster='/p1.jpg', backdrop='/b1.jpg' WHERE id=?1", [id])?;
            Ok(())
        })
        .await
        .unwrap();
    let detail: Value = viewer.call(Method::GET, &format!("/items/{id}"), None::<&()>).await.unwrap();
    assert_eq!(detail["item"]["poster"], "w342-p1.jpg");
    assert_eq!(detail["item"]["backdrop"], "w780-b1.jpg");
    let list: Value = viewer.call(Method::GET, "/items", None::<&()>).await.unwrap();
    assert_eq!(list[0]["item"], detail["item"]);
    let art_path = "/artwork/w342-p1.jpg";
    let reply = api_request(&server, &viewer, art_path, None).await;
    assert_eq!(reply.status(), StatusCode::OK);
    assert_eq!(reply.headers()["content-type"], "image/jpeg");
    assert_eq!(reply.into_body().collect().await.unwrap().to_bytes().as_ref(), jpeg);
    assert_eq!(server::handle(&server.state, get(art_path)).await.status(), StatusCode::UNAUTHORIZED);
    for path in [
        "/artwork/../server.key",
        "/artwork/%2e%2e%2fserver.key",
        "/artwork/w342-..jpg",
        "/artwork/w342-x/y.jpg",
        "/artwork/w342-.jpg",
        "/artwork/w100-p1.jpg",
    ] {
        assert_eq!(api_request(&server, &viewer, path, None).await.status(), StatusCode::BAD_REQUEST);
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(server.state.data_dir.join("server.key"), art.join("w342-secret.jpg")).unwrap();
        assert_eq!(
            api_request(&server, &viewer, "/artwork/w342-secret.jpg", None).await.status(),
            StatusCode::FORBIDDEN
        );
    }
    let p = format!("/progress/{id}");
    let saved = json!({"position": 2500, "played": false, "updated_at": 1000});
    let result: Value = viewer.call(Method::PUT, &p, Some(&saved)).await.unwrap();
    assert_eq!(result, saved);
    for updated_at in [999, 1000] {
        let stale = json!({"position": 0, "played": true, "updated_at": updated_at});
        let result: Value = viewer.call(Method::PUT, &p, Some(&stale)).await.unwrap();
        assert_eq!(result, saved, "equal timestamps preserve stored progress");
    }
    let result: Value = viewer.call(Method::GET, &p, None::<&()>).await.unwrap();
    assert_eq!(result, saved);
    for bad in [
        json!({"position": -1,"played":false,"updated_at":2000}),
        json!({"position": 0,"played":false,"updated_at":-1}),
        json!({"position": 0,"played":false,"updated_at":2000,"user":1}),
    ] {
        assert!(matches!(
            viewer.call::<Value>(Method::PUT, &p, Some(&bad)).await,
            Err(flicks_client::Error::Status(400, _))
        ));
    }
    let result: Value = other.call(Method::GET, &p, None::<&()>).await.unwrap();
    assert_eq!(result, Value::Null);
    let result: Value = other.call(Method::GET, "/progress", None::<&()>).await.unwrap();
    assert_eq!(result, json!([]));
    let result: Value = viewer.call(Method::GET, "/progress", None::<&()>).await.unwrap();
    assert_eq!(result, json!([{"item":id,"progress":saved}]));
    let home: Value = viewer.call(Method::GET, "/home", None::<&()>).await.unwrap();
    assert_eq!(home["continue_watching"][0]["progress"], saved);
    assert_eq!(home["recently_added"][0]["item"]["id"], id);
    let home: Value = other.call(Method::GET, "/home", None::<&()>).await.unwrap();
    assert_eq!(home["continue_watching"], json!([]));

    // One playback metadata request includes facts, tracks, index, progress and real header ranges.
    let play: Value = viewer.call(Method::GET, &format!("/play/{id}"), None::<&()>).await.unwrap();
    assert_eq!(play["item"]["duration_ms"], 5000);
    assert_eq!(play["streams"][0]["kind"], "audio");
    assert_eq!(play["keyframes"].as_array().unwrap().len(), 5);
    assert_eq!(play["resume_ms"], 2500);
    assert_eq!(play["ranges"]["media"]["start"], play["keyframes"][2]["offset"]);
    assert_eq!(play["progress"], saved);
    // This writer puts moov at EOF: returned metadata ranges must cover it without fetching mdat.
    let moov = bytes.windows(4).rposition(|b| b == b"moov").unwrap() as u64 - 4;
    let headers = play["ranges"]["header"].as_array().unwrap();
    assert!(
        headers
            .iter()
            .any(|r| r["start"].as_u64().unwrap() <= moov && r["end"].as_u64().unwrap() >= bytes.len() as u64 - 1)
    );
    let header_bytes: u64 = headers.iter().map(|r| r["end"].as_u64().unwrap() - r["start"].as_u64().unwrap() + 1).sum();
    assert!(header_bytes < 10_000);

    let path = format!("/files/{id}");
    let len = bytes.len();
    for (range, start, end) in [
        (None, 0, len - 1),
        (Some("bytes=0-0"), 0, 0),
        (Some("bytes=10-19"), 10, 19),
        (Some("bytes=10-"), 10, len - 1),
        (Some("bytes=-10"), len - 10, len - 1),
        (Some("bytes=-9999999"), 0, len - 1),
        (Some("bytes=10-9999999"), 10, len - 1),
    ] {
        let reply = api_request(&server, &viewer, &path, range).await;
        assert_eq!(reply.status(), if range.is_some() { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK });
        assert_eq!(reply.headers()["accept-ranges"], "bytes");
        assert_eq!(reply.headers()["content-length"], (end - start + 1).to_string());
        if range.is_some() {
            assert_eq!(reply.headers()["content-range"], format!("bytes {start}-{end}/{len}"));
        }
        let mut body = reply.into_body();
        let mut received = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.unwrap().into_data().unwrap();
            assert!(frame.len() <= 64 * 1024);
            received.extend_from_slice(&frame);
        }
        assert_eq!(received, bytes[start..=end]);
    }
    for range in [
        "bytes=9999999-",
        "bytes=20-10",
        "bytes=-0",
        "bytes=0-1,3-4",
        "bytes=",
        "items=0-1",
        "bytes=18446744073709551616-",
        "bytes=+1-2",
    ] {
        let reply = api_request(&server, &viewer, &path, Some(range)).await;
        assert_eq!(reply.status(), StatusCode::RANGE_NOT_SATISFIABLE, "{range}");
        assert_eq!(reply.headers()["content-range"], format!("bytes */{len}"));
    }
    assert_eq!(server::handle(&server.state, get(&path)).await.status(), StatusCode::UNAUTHORIZED);
    let authorization =
        sign(&viewer.key, &viewer.server_key, "GET", &path, b"bytes=0-1", b"", flicks_server::unix_now());
    assert_eq!(
        server::handle(&server.state, signed_request(Method::GET, &path, &authorization, Some("bytes=1-2"), b""))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    // Marking played removes only this user's Continue Watching entry.
    let _: Value =
        viewer.call(Method::PUT, &p, Some(&json!({"position":5000,"played":true,"updated_at":1001}))).await.unwrap();
    let home: Value = viewer.call(Method::GET, "/home", None::<&()>).await.unwrap();
    assert_eq!(home["continue_watching"], json!([]));

    // A watcher rescans after a new file appears, without another folder edit.
    std::fs::write(media.path().join("Second.mp4"), &bytes).unwrap();
    catalog_ready(&viewer, 2).await;
    std::fs::remove_file(media.path().join("Second.mp4")).unwrap();
    catalog_ready(&viewer, 1).await;
    server.root.call::<()>(Method::DELETE, &format!("/admin/folders/{}", folder.id), None::<&()>).await.unwrap();
    assert_eq!(api_request(&server, &viewer, &path, None).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(api_request(&server, &viewer, &format!("/play/{id}"), None).await.status(), StatusCode::NOT_FOUND);
    catalog_ready(&viewer, 0).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let rows = server
                .state
                .db
                .call(|conn| {
                    Ok(conn.query_row(
                        "SELECT (SELECT COUNT(*) FROM library_files) +
                    (SELECT COUNT(*) FROM library_streams) + (SELECT COUNT(*) FROM library_dirs) +
                    (SELECT COUNT(*) FROM progress)",
                        [],
                        |r| r.get::<_, i64>(0),
                    )?)
                })
                .await
                .unwrap();
            if rows == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let version = server
        .state
        .db
        .call(|conn| Ok(conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))?))
        .await
        .unwrap();
    assert_eq!(version, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn catalog_paths_are_checked_at_each_open() {
    use std::os::unix::fs::symlink;
    let server = start("catalog-paths").await;
    let media = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let bytes = catalog_mp4();
    let root = std::fs::canonicalize(media.path()).unwrap();
    let path = root.join("Movie.mp4");
    std::fs::write(&path, &bytes).unwrap();
    // Populate a real scan, but don't subscribe this test's folder to a watcher: the stale
    // catalog entry must remain present while we replace its path after scanning.
    let r = root.clone();
    let item = server
        .state
        .db
        .call(move |conn| {
            flicks_server::db::add_folder(conn, r.to_str().unwrap())?;
            flicks_library::Scanner::new(None, r.join("artwork")).scan(conn, &r).unwrap();
            Ok(flicks_library::item(conn, 1).unwrap().unwrap())
        })
        .await
        .unwrap();
    assert_eq!(
        api_request(&server, &server.root, &format!("/files/{}", item.id), Some("bytes=0-0")).await.status(),
        StatusCode::PARTIAL_CONTENT
    );
    // A truncated-to-empty file is a valid empty 200 without Range; no byte range can satisfy it.
    std::fs::write(&path, []).unwrap();
    let empty = api_request(&server, &server.root, &format!("/files/{}", item.id), None).await;
    assert_eq!(empty.status(), StatusCode::OK);
    assert_eq!(empty.headers()["content-length"], "0");
    let empty = api_request(&server, &server.root, &format!("/files/{}", item.id), Some("bytes=0-")).await;
    assert_eq!(empty.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(empty.headers()["content-range"], "bytes */0");
    std::fs::write(outside.path().join("secret"), b"private").unwrap();
    std::fs::remove_file(&path).unwrap();
    symlink(outside.path().join("secret"), &path).unwrap();
    for prefix in ["files", "play"] {
        assert_eq!(
            api_request(&server, &server.root, &format!("/{prefix}/{}", item.id), None).await.status(),
            StatusCode::FORBIDDEN
        );
    }
    // Even an in-root symlink is refused.
    std::fs::remove_file(&path).unwrap();
    std::fs::write(root.join("inside"), &bytes).unwrap();
    symlink(root.join("inside"), &path).unwrap();
    assert_eq!(
        api_request(&server, &server.root, &format!("/files/{}", item.id), None).await.status(),
        StatusCode::FORBIDDEN
    );
    // A directory component repointed after scanning is refused too.
    symlink(outside.path(), root.join("linked")).unwrap();
    let linked = root.join("linked/secret");
    server
        .state
        .db
        .call(move |conn| {
            conn.execute("UPDATE library_files SET path=?1 WHERE id=?2", rusqlite::params![linked.to_str(), item.id])?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        api_request(&server, &server.root, &format!("/files/{}", item.id), None).await.status(),
        StatusCode::FORBIDDEN
    );
    // Lexical traversal in a corrupt/stale catalog cannot escape either.
    let escaped = root.join("..").join(outside.path().file_name().unwrap()).join("secret");
    server
        .state
        .db
        .call(move |conn| {
            conn.execute("UPDATE library_files SET path=?1 WHERE id=?2", rusqlite::params![escaped.to_str(), item.id])?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        api_request(&server, &server.root, &format!("/files/{}", item.id), None).await.status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn startup_scans_existing_folders_and_matroska_play_headers() {
    use flicks_container::matroska::{MkvWriter, MuxOptions, TrackKind, TrackSpec};
    use serde_json::Value;
    let dir = tempfile::tempdir().unwrap();
    let media = tempfile::tempdir().unwrap();
    let mut track = TrackSpec::new(TrackKind::Video, "V_MPEG4/ISO/AVC");
    track.video_size = Some((1280, 720));
    let mut writer = MkvWriter::new(std::io::Cursor::new(Vec::new()), vec![track], MuxOptions::default()).unwrap();
    for i in 0..5 {
        writer.write_frame(0, i * 1_000_000_000, true, &[1; 100], Some(1_000_000_000)).unwrap();
    }
    let mut bytes = writer.finish().unwrap().into_inner();
    // Add a second SeekHead and Chapters after the media, plus stale entries in the first
    // SeekHead. The container opens this file, so /play must accept it too.
    use flicks_container::matroska::{self, ebml, element_ids::*};
    fn element(id: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        ebml::write_id(&mut out, id);
        ebml::write_size(&mut out, data.len() as u64, 1);
        out.extend_from_slice(data);
        out
    }
    fn seek(id: u32, position: u64) -> Vec<u8> {
        let mut id_bytes = Vec::new();
        ebml::write_id(&mut id_bytes, id);
        let mut data = element(SEEK_ID, &id_bytes);
        let position = position.to_be_bytes();
        let start = position.iter().position(|b| *b != 0).unwrap_or(7);
        data.extend(element(SEEK_POSITION, &position[start..]));
        element(SEEK, &data)
    }
    let doc = matroska::open(&bytes).unwrap();
    let base = doc.segment_data_start;
    let chapters = bytes.len() as u64;
    bytes.extend(element(CHAPTERS, &[]));
    let chained = bytes.len() as u64;
    bytes.extend(element(SEEK_HEAD, &seek(CHAPTERS, chapters - base)));
    let mut entries = Vec::new();
    for entry in &doc.seek_head {
        entries.extend(seek(entry.id, entry.position));
    }
    entries.extend(seek(INFO, doc.first_cluster.unwrap() - base));
    entries.extend(seek(CUES, u64::MAX));
    entries.extend(seek(SEEK_HEAD, chained - base));
    let mut head = element(SEEK_HEAD, &entries);
    assert!(head.len() <= 126);
    head.extend(element(VOID, &vec![0; 128 - head.len() - 2]));
    bytes[base as usize..base as usize + 128].copy_from_slice(&head);
    // The muxer reserves eight bytes for the Segment size; use an unknown-size Segment so
    // appended metadata is part of it without changing any file offsets.
    let mut unknown = Vec::new();
    ebml::write_unknown_size(&mut unknown, 8);
    bytes[base as usize - 8..base as usize].copy_from_slice(&unknown);
    std::fs::write(media.path().join("Movie.mkv"), &bytes).unwrap();
    let before = State::open(dir.path(), 0).unwrap();
    let root = std::fs::canonicalize(media.path()).unwrap();
    before
        .db
        .call(move |conn| {
            flicks_server::db::add_folder(conn, root.to_str().unwrap())?;
            Ok(())
        })
        .await
        .unwrap();
    drop(before);
    let (addr, state) = server::spawn(dir.path(), "127.0.0.1:0".parse().unwrap()).await.unwrap();
    let client = Connection::pinned(state.key.clone(), state.key.verifying_key(), addr.to_string()).unwrap();
    // Schema ready before spawn returns; opening startup with a folder triggers a scan.
    let items = catalog_ready(&client, 1).await;
    let id = items[0]["item"]["id"].as_i64().unwrap();
    let play: Value = client.call(Method::GET, &format!("/play/{id}"), None::<&()>).await.unwrap();
    assert_eq!(play["item"]["container"], "mkv");
    assert_eq!(play["keyframes"].as_array().unwrap().len(), 5);
    let header = play["ranges"]["header"].as_array().unwrap();
    assert_eq!(header[0]["start"], 0);
    assert!(header.iter().all(|r| r["end"].as_u64().unwrap() < bytes.len() as u64));
    for offset in [chapters, chained] {
        assert!(
            header.iter().any(|r| r["start"].as_u64().unwrap() <= offset && r["end"].as_u64().unwrap() >= offset + 4)
        );
    }
}

#[tokio::test]
async fn cataloged_mp4_padding_and_oversized_boxes_play() {
    let server = start("mp4-padding").await;
    let media = tempfile::tempdir().unwrap();
    let base = catalog_mp4();
    let mut padded = base.clone();
    padded.extend([1, 2, 3, 4, 5]);
    std::fs::write(media.path().join("Padded.mp4"), padded).unwrap();
    let mut clamped = base.clone();
    clamped.extend(100u32.to_be_bytes());
    clamped.extend(b"free");
    std::fs::write(media.path().join("Clamped.mp4"), clamped).unwrap();
    let mut many = base.clone();
    for _ in 0..100_001 {
        many.extend(b"\0\0\0\x08free");
    }
    std::fs::write(media.path().join("Many.mp4"), many).unwrap();
    let mut no_ftyp = base;
    no_ftyp[4..8].copy_from_slice(b"free");
    std::fs::write(media.path().join("No brand.mp4"), no_ftyp).unwrap();
    server
        .root
        .call::<Folder>(
            Method::POST,
            "/admin/folders",
            Some(&AddFolder { path: media.path().to_string_lossy().into_owned() }),
        )
        .await
        .unwrap();
    let items = catalog_ready(&server.root, 4).await;
    for entry in items {
        let id = entry["item"]["id"].as_i64().unwrap();
        let play: serde_json::Value = server.root.call(Method::GET, &format!("/play/{id}"), None::<&()>).await.unwrap();
        assert_eq!(play["item"]["id"], id);
        let size = play["item"]["size"].as_u64().unwrap();
        for r in play["ranges"]["header"].as_array().unwrap() {
            assert!(r["start"].as_u64().unwrap() <= r["end"].as_u64().unwrap());
            assert!(r["end"].as_u64().unwrap() < size);
        }
    }
}

#[tokio::test]
async fn cli_get_stops_at_the_end_of_the_file() {
    use flicks_client::MAX_READ;
    use flicks_server::cli_client::get;
    let server = start("cli-get").await;
    let media = tempfile::tempdir().unwrap();
    // Exactly one read long, padded with a `free` box: a range past the end used to ask for the
    // next chunk and fail with 416 after writing every byte.
    let mut bytes = catalog_mp4();
    let pad = MAX_READ as usize - bytes.len();
    bytes.extend((pad as u32).to_be_bytes());
    bytes.extend(b"free");
    bytes.resize(MAX_READ as usize, 0);
    std::fs::write(media.path().join("Exact.mp4"), &bytes).unwrap();
    let path = media.path().to_string_lossy().into_owned();
    server.root.call::<Folder>(Method::POST, "/admin/folders", Some(&AddFolder { path })).await.unwrap();
    let id = catalog_ready(&server.root, 1).await[0]["item"]["id"].as_i64().unwrap();
    let size = MAX_READ;
    for (start, end) in [(0, size - 1), (0, size), (0, 3 * size), (10, size + 5), (size - 1, u64::MAX)] {
        let mut out = Vec::new();
        get(&server.root, id, start, end, &mut out).await.unwrap();
        assert_eq!(out, bytes[start as usize..], "{start}-{end}");
    }
    let mut out = Vec::new();
    get(&server.root, id, 100, 199, &mut out).await.unwrap();
    assert_eq!(out, bytes[100..200]);
    for start in [size, size + 1, u64::MAX] {
        let err = get(&server.root, id, start, u64::MAX, &mut Vec::new()).await.unwrap_err();
        assert!(matches!(err, flicks_server::Error::Usage(_)), "{start}: {err}");
    }
}

/// Opt-in measurement against the actual release `flicks serve`, with an isolated HOME.
/// Set FLICKS_API_MEDIA to an existing read-only media folder to measure real files instead.
#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "binds port 7686 and measures localhost HTTPS; run alone in release mode"]
async fn api_performance() {
    use http_body_util::BodyExt;
    use hyper_util::rt::TokioIo;
    use serde_json::Value;
    use std::time::{Duration, Instant};

    struct Process(std::process::Child);
    impl Drop for Process {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    assert!(std::net::TcpStream::connect("127.0.0.1:7686").is_err(), "port 7686 already in use");
    let home = tempfile::tempdir().unwrap();
    let synthetic = tempfile::tempdir().unwrap();
    let media = match std::env::var_os("FLICKS_API_MEDIA") {
        Some(path) => PathBuf::from(path),
        None => {
            std::fs::write(synthetic.path().join("Synthetic.mp4"), catalog_mp4()).unwrap();
            synthetic.path().to_path_buf()
        }
    };
    let executable = env!("CARGO_BIN_EXE_flicks");
    let _process = Process(
        std::process::Command::new(executable)
            .arg("serve")
            .env("HOME", home.path())
            .env_remove("FLICKS_TMDB_KEY")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let data = home.path().join("Library/Application Support/Flicks");
    let root = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(root) = flicks_server::cli::local(&data)
                && root.call::<Status>(Method::GET, "/admin/status", None::<&()>).await.is_ok()
            {
                break root;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let scan_start = Instant::now();
    let added = std::process::Command::new(executable)
        .args(["folder", "add"])
        .arg(&media)
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    let items = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let items: Vec<Value> = root.call(Method::GET, "/items", None::<&()>).await.unwrap();
            // The supplied real fixture folder has two files; synthetic has one.
            let expected = if std::env::var_os("FLICKS_API_MEDIA").is_some() { 2 } else { 1 };
            if items.len() == expected {
                break items;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    eprintln!("API_PERF scan_to_visible_ms={:.3} files={}", scan_start.elapsed().as_secs_f64() * 1000.0, items.len());
    let link = open_window(&root, "perf-viewer", false).await;
    let key = keys::generate().unwrap();
    let request = pair::Request::new(&key, &root.server_key, &link.code, "Perf").unwrap();
    let response =
        net::send(&root.addr, root.tls.clone(), post_json("/pair/redeem", serde_json::to_vec(&request).unwrap()))
            .await
            .unwrap();
    assert_eq!(response.status, StatusCode::OK);
    let viewer =
        Connection { key, server_key: root.server_key.clone(), addr: root.addr.clone(), tls: root.tls.clone() };
    for entry in items {
        let id = entry["item"]["id"].as_i64().unwrap();
        let play_path = format!("/play/{id}");
        let play: Value = viewer.call(Method::GET, &play_path, None::<&()>).await.unwrap();
        assert_eq!(play["item"]["container"], "mp4");
        assert!(!play["keyframes"].as_array().unwrap().is_empty());
        let mut first_byte = Vec::new();
        let mut play_time = Vec::new();
        for n in 0..101 {
            let path = format!("/files/{id}");
            let range = "bytes=0-65535";
            let authorization =
                sign(&viewer.key, &viewer.server_key, "GET", &path, range.as_bytes(), b"", flicks_server::unix_now());
            let request = signed_request(Method::GET, &path, &authorization, Some(range), b"");
            // Includes a NEW TCP connection and pinned TLS handshake for every range request.
            // Signing happens before the timer; server signature verification is included.
            let start = Instant::now();
            let tcp = TcpStream::connect(viewer.addr.as_str()).await.unwrap();
            let stream = TlsConnector::from(viewer.tls.clone())
                .connect(ServerName::try_from("flicks").unwrap(), tcp)
                .await
                .unwrap();
            let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.unwrap();
            let task = tokio::spawn(connection);
            let reply = sender.send_request(request).await.unwrap();
            assert_eq!(reply.status(), StatusCode::PARTIAL_CONTENT);
            let mut body = reply.into_body();
            let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
            assert!(!first.is_empty());
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            body.collect().await.unwrap();
            drop(sender);
            task.await.unwrap().unwrap();
            let start = Instant::now();
            let got: Value = viewer.call(Method::GET, &play_path, None::<&()>).await.unwrap();
            assert_eq!(got["item"]["id"], id);
            if n > 0 {
                first_byte.push(elapsed);
                play_time.push(start.elapsed().as_secs_f64() * 1000.0);
            }
        }
        first_byte.sort_by(f64::total_cmp);
        play_time.sort_by(f64::total_cmp);
        eprintln!(
            "API_PERF size={} keys={} n=100 range_ms p50={:.3} p95={:.3} max={:.3}; play_ms p50={:.3} p95={:.3} max={:.3}",
            play["item"]["size"],
            play["keyframes"].as_array().unwrap().len(),
            first_byte[49],
            first_byte[94],
            first_byte[99],
            play_time[49],
            play_time[94],
            play_time[99]
        );
        assert!(first_byte[94] < 5.0, "range first-byte p95 exceeds 5 ms: {}", first_byte[94]);
    }
}
