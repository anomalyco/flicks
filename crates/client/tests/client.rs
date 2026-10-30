//! The client against a real server, on localhost and through a local TLS-terminating proxy that
//! has its own certificate under a hostname: pairing, the catalog and byte ranges, and refusing a
//! server whose key isn't the pinned one.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use flicks_client::{Client, Error, Server, keys};
use flicks_server::db::{Device, Folder};
use flicks_server::server::{self, AddFolder, OpenPairing, Pairing, State};
use hyper::Method;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

fn dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("client-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

struct Setup {
    addr: SocketAddr,
    state: Arc<State>,
    root: flicks_client::Connection,
    media: Vec<u8>,
    proxy: SocketAddr,
    proxied: Arc<AtomicUsize>,
    authority: CertificateDer<'static>,
    _folder: tempfile::TempDir,
}

/// A server with one generated MP4 in its folder, and a proxy in front of it.
async fn setup(name: &str) -> Setup {
    let (addr, state) = server::spawn(&dir(&format!("{name}-server")), "127.0.0.1:0".parse().unwrap()).await.unwrap();
    let root =
        flicks_client::Connection::pinned(state.key.clone(), state.key.verifying_key(), addr.to_string()).unwrap();
    let folder = tempfile::tempdir().unwrap();
    let media = mp4();
    std::fs::write(folder.path().join("Example (2020).mp4"), &media).unwrap();
    let path = folder.path().to_string_lossy().into_owned();
    let _: Folder = root.call(Method::POST, "/admin/folders", Some(&AddFolder { path })).await.unwrap();
    let (proxy, proxied, authority) = proxy(addr, &state).await;
    Setup { addr, state, root, media, proxy, proxied, authority, _folder: folder }
}

fn mp4() -> Vec<u8> {
    use flicks_container::isobmff::{
        AvcConfig, Brand, Mp4Writer, SampleEntry, TrackConfig, WriteSample, WriterOptions,
    };
    let avc = AvcConfig::new(vec![vec![0x67, 0x64, 0, 0x28, 1, 2]], vec![vec![0x68, 3]], 4);
    let mut w = Mp4Writer::new(std::io::Cursor::new(Vec::new()), WriterOptions::new(Brand::Mp4)).unwrap();
    let v = w.add_track(TrackConfig::new(SampleEntry::avc(avc, 1280, 720), 1000)).unwrap();
    for second in 0..5 {
        let sample = WriteSample { data: &[second; 100_000], duration: 1000, composition_offset: 0, is_sync: true };
        w.write_sample(v, sample).unwrap();
    }
    w.finish().unwrap().into_inner()
}

/// A TLS-terminating proxy at `localhost`, like a tunnel: its own certificate for that name from
/// its own authority (returned, for clients to trust), and TLS to the server behind it.
async fn proxy(server: SocketAddr, state: &State) -> (SocketAddr, Arc<AtomicUsize>, CertificateDer<'static>) {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::new(ca_params, ca_key);
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap().signed_by(&key, &issuer).unwrap();
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], PrivateKeyDer::Pkcs8(key.serialize_der().into()))
        .unwrap();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let web = flicks_client::net::web_verifier(&[]).unwrap();
    let upstream =
        TlsConnector::from(Arc::new(flicks_client::net::client_config(state.key.verifying_key(), web).unwrap()));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let counter = count.clone();
    tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            let (acceptor, upstream, counter) = (acceptor.clone(), upstream.clone(), counter.clone());
            tokio::spawn(async move {
                let Ok(mut client) = acceptor.accept(tcp).await else { return };
                counter.fetch_add(1, Ordering::SeqCst);
                let tcp = TcpStream::connect(server).await.unwrap();
                let mut server = upstream.connect(ServerName::try_from("flicks").unwrap(), tcp).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
            });
        }
    });
    (addr, count, ca.der().clone())
}

/// Opens a pairing window whose link names `addr` after the server's interface addresses (which
/// don't answer: the server listens on 127.0.0.1 only, so pairing has to win the race elsewhere).
async fn link(setup: &Setup, user: &str, addr: String) -> String {
    let request = OpenPairing { user: user.into(), admin: false, addrs: vec![addr] };
    let pairing: Pairing = setup.root.call(Method::POST, "/admin/pair", Some(&request)).await.unwrap();
    pairing.link
}

async fn devices(setup: &Setup) -> usize {
    let devices: Vec<Device> = setup.root.call(Method::GET, "/admin/devices", None::<&()>).await.unwrap();
    devices.len()
}

/// Lists the catalog (waiting for the first scan) and reads byte ranges, checking them against the
/// file.
async fn list_and_read(client: &mut Client, server: &Server, media: &[u8]) -> String {
    let connection = client.connect(&server.key).await.unwrap();
    let items = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let items = connection.items().await.unwrap();
            if !items.is_empty() {
                return items;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(items.len(), 1);
    let item = &items[0].item;
    assert_eq!((item.title.as_str(), item.year, item.size), ("Example", Some(2020), media.len() as u64));
    assert_eq!(&connection.read(item.id, 100, 1099).await.unwrap()[..], &media[100..1100]);
    let len = media.len() as u64;
    assert_eq!(&connection.read(item.id, len - 10, len + 100).await.unwrap()[..], &media[media.len() - 10..]);
    assert!(connection.read(item.id, 5, 4).await.is_err());
    connection.addr
}

#[tokio::test]
async fn pairs_lists_and_reads_on_localhost_and_through_a_proxy() {
    let setup = setup("pair").await;

    // On localhost: the TLS certificate carries the server key.
    // An existing data directory with looser permissions is made private.
    let home = dir("pair-local");
    std::fs::create_dir_all(&home).unwrap();
    let mut local = Client::open(&home).unwrap();
    let server = local.pair(&link(&setup, "ana", setup.addr.to_string()).await, "Laptop").await.unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&home), 0o700);
        assert_eq!(mode(&home.join(flicks_client::KEY_FILE)), 0o600);
        assert_eq!(mode(&home.join(flicks_client::SERVERS_FILE)), 0o600);
    }
    assert_eq!((server.name.as_str(), server.user.as_str(), server.admin), ("127.0.0.1", "ana", false));
    assert_eq!(list_and_read(&mut local, &server, &setup.media).await, setup.addr.to_string());
    // The address that answered is saved first, and gets the head start next time.
    assert_eq!(server.addrs[0], setup.addr.to_string());
    assert!(server.addrs.len() > 1);

    // Through the proxy, by name: its certificate is valid for `localhost` under its authority,
    // and the signed probe proves the server is behind it.
    let via = format!("localhost:{}", setup.proxy.port());
    let home = dir("pair-proxy");
    let mut proxied = Client::open(&home).unwrap();
    proxied.trust(setup.authority.clone()).unwrap();
    let server = proxied.pair(&link(&setup, "bo", via.clone()).await, "Phone").await.unwrap();
    assert_eq!((server.name.as_str(), server.user.as_str()), ("localhost", "bo"));
    let before = setup.proxied.load(Ordering::SeqCst);
    assert_eq!(list_and_read(&mut proxied, &server, &setup.media).await, via);
    assert!(setup.proxied.load(Ordering::SeqCst) > before);

    // The key and server list survive a restart.
    let mut reopened = Client::open(&home).unwrap();
    reopened.trust(setup.authority.clone()).unwrap();
    assert_eq!((reopened.public_key(), &reopened.servers), (proxied.public_key(), &vec![server.clone()]));
    let device_key = keys::encode(&proxied.public_key());
    let devices: Vec<Device> = setup.root.call(Method::GET, "/admin/devices", None::<&()>).await.unwrap();
    assert!(devices.iter().any(|d| d.key == device_key && d.name == "Phone" && d.user == "bo"));

    // A client that doesn't trust the proxy's authority refuses it.
    let mut untrusting = Client::open(&dir("pair-untrusting")).unwrap();
    let err = untrusting.pair(&link(&setup, "cy", via).await, "Tablet").await.unwrap_err();
    assert!(matches!(err, Error::Unreachable { .. }), "{err}");
    assert_eq!(devices.len(), 2);
    assert_eq!(self::devices(&setup).await, 2);
}

#[tokio::test]
async fn a_wrong_pinned_key_is_refused() {
    let setup = setup("wrong").await;
    let mut client = Client::open(&dir("wrong-client")).unwrap();
    client.trust(setup.authority.clone()).unwrap();
    let wrong = keys::encode(&keys::public_bytes(keys::generate().unwrap().verifying_key()));
    let real = keys::encode(&setup.state.public);
    let via = format!("localhost:{}", setup.proxy.port());

    // A link carrying another key, with the real code and addresses: directly the TLS handshake
    // fails; through the proxy the probe isn't signed by the pinned key. Nothing is redeemed.
    for addr in [setup.addr.to_string(), via.clone()] {
        let link = link(&setup, "dee", addr.clone()).await.replace(&real, &wrong);
        let err = client.pair(&link, "Laptop").await.unwrap_err();
        let Error::Unreachable { last, .. } = &err else { panic!("{addr}: {err}") };
        if addr == via {
            assert!(matches!(**last, Error::WrongServer), "{addr}: {err}");
        } else {
            let tls = matches!(&**last, Error::Io(e) if e.kind() == std::io::ErrorKind::InvalidData);
            assert!(tls, "{addr}: {err}");
        }
    }
    assert_eq!(devices(&setup).await, 0);
    assert!(client.servers.is_empty());

    // A saved server with the wrong key is refused at every address too.
    let addrs = vec![setup.addr.to_string(), via];
    client.servers.push(Server { key: wrong.clone(), name: "x".into(), addrs, user: "dee".into(), admin: false });
    assert!(matches!(client.connect(&wrong).await, Err(Error::Unreachable { tried: 2, .. })));
    assert!(matches!(client.connect(&real).await, Err(Error::NotPaired)));
}
