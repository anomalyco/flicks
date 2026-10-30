//! The client against a real server, on localhost and through a local TLS-terminating proxy that
//! has its own certificate under a hostname: pairing, the catalog and byte ranges, and refusing a
//! server whose key isn't the pinned one. And two servers merged into one library.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use flicks_client::download::{Downloads, State as DownloadState};
use flicks_client::merge::{self, Library};
use flicks_client::{Client, Error, Progress, Server, keys};
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
    setup_with(name, mp4()).await
}

async fn setup_with(name: &str, media: Vec<u8>) -> Setup {
    let (addr, state) = server::spawn(&dir(&format!("{name}-server")), "127.0.0.1:0".parse().unwrap()).await.unwrap();
    let root =
        flicks_client::Connection::pinned(state.key.clone(), state.key.verifying_key(), addr.to_string()).unwrap();
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("Example (2020).mp4"), &media).unwrap();
    let path = folder.path().to_string_lossy().into_owned();
    let _: Folder = root.call(Method::POST, "/admin/folders", Some(&AddFolder { path })).await.unwrap();
    let (proxy, proxied, authority) = proxy(addr, &state).await;
    Setup { addr, state, root, media, proxy, proxied, authority, _folder: folder }
}

fn mp4() -> Vec<u8> {
    mp4_sized(1280, 720)
}

fn mp4_sized(width: u16, height: u16) -> Vec<u8> {
    mp4_samples(width, height, (0..5).map(|second| vec![second; 100_000]))
}

fn mp4_samples(width: u16, height: u16, samples: impl Iterator<Item = Vec<u8>>) -> Vec<u8> {
    use flicks_container::isobmff::{
        AvcConfig, Brand, Mp4Writer, SampleEntry, TrackConfig, WriteSample, WriterOptions,
    };
    let avc = AvcConfig::new(vec![vec![0x67, 0x64, 0, 0x28, 1, 2]], vec![vec![0x68, 3]], 4);
    let mut w = Mp4Writer::new(std::io::Cursor::new(Vec::new()), WriterOptions::new(Brand::Mp4)).unwrap();
    let v = w.add_track(TrackConfig::new(SampleEntry::avc(avc, width, height), 1000)).unwrap();
    for data in samples {
        let sample = WriteSample { data: &data, duration: 1000, composition_offset: 0, is_sync: true };
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

/// Opens a pairing window whose link names `addr` first, then the server's interface addresses
/// (which don't answer: the server listens on 127.0.0.1 only).
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
    // Named as the server calls itself, not by its address.
    assert_eq!((&server.name, server.user.as_str(), server.admin), (&setup.state.name, "ana", false));
    assert_eq!(list_and_read(&mut local, &server, &setup.media).await, setup.addr.to_string());
    // The address that answered is saved first, and gets the head start next time.
    assert_eq!(server.addrs[0], setup.addr.to_string());
    assert!(server.addrs.len() > 1);
    // Artwork bytes come back as cached; a name that would leave `/artwork/` isn't sent.
    let artwork = setup.state.data_dir.join("artwork");
    std::fs::create_dir_all(&artwork).unwrap();
    std::fs::write(artwork.join("w342-abc123.jpg"), b"jpeg bytes").unwrap();
    let connection = local.connect(&server.key).await.unwrap();
    assert_eq!(&connection.artwork("w342-abc123.jpg").await.unwrap()[..], b"jpeg bytes");
    assert!(matches!(connection.artwork("w342-missing.jpg").await, Err(Error::Status(..))));
    assert!(matches!(connection.artwork("../flicks.db").await, Err(Error::BadResponse)));

    // Through the proxy, by name: its certificate is valid for `localhost` under its authority,
    // and the signed probe proves the server is behind it.
    let via = format!("localhost:{}", setup.proxy.port());
    let home = dir("pair-proxy");
    let mut proxied = Client::open(&home).unwrap();
    proxied.trust(setup.authority.clone()).unwrap();
    let server = proxied.pair(&link(&setup, "bo", via.clone()).await, "Phone").await.unwrap();
    assert_eq!((&server.name, server.user.as_str()), (&setup.state.name, "bo"));
    let before = setup.proxied.load(Ordering::SeqCst);
    assert_eq!(list_and_read(&mut proxied, &server, &setup.media).await, via);
    assert!(setup.proxied.load(Ordering::SeqCst) > before);

    // The key and server list survive a restart.
    let mut reopened = Client::open(&home).unwrap();
    reopened.trust(setup.authority.clone()).unwrap();
    assert_eq!((reopened.public_key(), &reopened.servers), (proxied.public_key(), &vec![server.clone()]));
    // A server renamed since pairing is saved under its new name on the next connect.
    reopened.servers[0].name = "Old name".into();
    reopened.connect(&server.key).await.unwrap();
    assert_eq!(Client::open(&home).unwrap().servers[0].name, setup.state.name);
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

/// A server that can be stopped, with `files` (name, width, height) in its folder, and this
/// client paired with it as `ana`. Returns the root connection, the serving task and the folder.
async fn stoppable(
    name: &str,
    files: &[(&str, u16, u16)],
    client: &mut Client,
) -> (flicks_client::Connection, tokio::task::JoinHandle<()>, Server, tempfile::TempDir) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(State::open(&dir(&format!("{name}-server")), addr.port()).unwrap());
    flicks_server::api::start(&state).await.unwrap();
    let serving = tokio::spawn({
        let state = state.clone();
        async move {
            let _ = server::run(listener, state).await;
        }
    });
    let root =
        flicks_client::Connection::pinned(state.key.clone(), state.key.verifying_key(), addr.to_string()).unwrap();
    let folder = tempfile::tempdir().unwrap();
    for (file, width, height) in files {
        std::fs::write(folder.path().join(file), mp4_sized(*width, *height)).unwrap();
    }
    let path = folder.path().to_string_lossy().into_owned();
    let _: Folder = root.call(Method::POST, "/admin/folders", Some(&AddFolder { path })).await.unwrap();
    let request = OpenPairing { user: "ana".into(), admin: false, addrs: vec![addr.to_string()] };
    let pairing: Pairing = root.call(Method::POST, "/admin/pair", Some(&request)).await.unwrap();
    let mut paired = client.pair(&pairing.link, "Laptop").await.unwrap();
    // Only the loopback address: the link's interface addresses take the 5 s connect timeout
    // each refresh once the server is stopped.
    paired.addrs.truncate(1);
    client.servers.last_mut().unwrap().addrs.truncate(1);
    // Wait for the first scan.
    let connection = client.connect(&paired.key).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while connection.items().await.unwrap().len() < files.len() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    (root, serving, paired, folder)
}

#[tokio::test]
async fn two_servers_merge_into_one_library() {
    let home = dir("merge-client");
    let mut client = Client::open(&home).unwrap();
    let (_, _a_serving, a, _a_folder) =
        stoppable("merge-a", &[("Shared (2020).mp4", 1280, 720), ("Only A (2021).mp4", 1280, 720)], &mut client).await;
    let (_, b_serving, b, _b_folder) =
        stoppable("merge-b", &[("Shared (2020).mp4", 1920, 1080), ("Only B (2022).mp4", 1280, 720)], &mut client).await;
    let mut library = Library::open(client);

    // The shared title is one, with a source on each server; the unique ones appear once each.
    library.refresh().await.unwrap();
    let names = |library: &Library| -> Vec<(String, usize, bool)> {
        library.titles().iter().map(|t| (t.item.title.clone(), t.sources.len(), t.available())).collect()
    };
    let all_up = vec![("Only A".into(), 1, true), ("Only B".into(), 1, true), ("Shared".into(), 2, true)];
    assert_eq!(names(&library), all_up);
    let shared = library.titles().iter().find(|t| t.item.title == "Shared").unwrap().clone();
    assert_eq!(shared.sources.iter().map(|s| s.server.as_str()).collect::<Vec<_>>(), [&a.key, &b.key]);
    // Both answer: the 1080p copy on B is best.
    assert_eq!(
        merge::best_source(&shared).map(|s| (s.server.as_str(), s.item.height)),
        Some((b.key.as_str(), Some(1080)))
    );

    // Progress saved on A (from another device, say) reaches B on the next refresh.
    let (a_file, b_file) = (shared.sources[0].item.id, shared.sources[1].item.id);
    let watched = Progress { position: 2_000, played: false, updated_at: 1_700_000_000_000 };
    let a_conn = library.connection(&a.key).unwrap();
    let _: Option<Progress> = a_conn.call(Method::PUT, &format!("/progress/{a_file}"), Some(&watched)).await.unwrap();
    library.refresh().await.unwrap();
    let on_b: Option<Progress> = library
        .connection(&b.key)
        .unwrap()
        .call(Method::GET, &format!("/progress/{b_file}"), None::<&()>)
        .await
        .unwrap();
    assert_eq!(on_b, Some(watched));
    // And newer progress set through the library goes to both.
    let later = Progress { position: 3_000, updated_at: watched.updated_at + 1, ..watched };
    library.set_progress(&shared.key, later).await.unwrap();
    for (server, file) in [(&a.key, a_file), (&b.key, b_file)] {
        let saved: Option<Progress> = library
            .connection(server)
            .unwrap()
            .call(Method::GET, &format!("/progress/{file}"), None::<&()>)
            .await
            .unwrap();
        assert_eq!(saved, Some(later));
    }

    // B stops: its titles stay listed from the cached catalog, unavailable, and A's 720p copy of
    // the shared title is the one to play.
    b_serving.abort();
    let _ = b_serving.await;
    library.refresh().await.unwrap();
    assert!(library.connection(&b.key).is_none());
    let one_down = vec![("Only A".into(), 1, true), ("Only B".into(), 1, false), ("Shared".into(), 2, true)];
    assert_eq!(names(&library), one_down);
    let shared = library.titles().iter().find(|t| t.item.title == "Shared").unwrap();
    assert!(!shared.sources[1].available);
    assert_eq!(
        merge::best_source(shared).map(|s| (s.server.as_str(), s.item.height)),
        Some((a.key.as_str(), Some(720)))
    );
    assert_eq!(shared.progress, Some(later));

    // Servers are reached at once: two that hang (accepting TCP but never answering TLS) cost one
    // connect timeout for the whole refresh, not one each, and the live server's titles are there.
    let hang = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hanging = hang.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            held.push(hang.accept().await.unwrap());
        }
    });
    library.client.servers.iter_mut().find(|s| s.key == b.key).unwrap().addrs = vec![hanging.clone()];
    let gone = keys::encode(&keys::public_bytes(keys::generate().unwrap().verifying_key()));
    let addrs = vec![hanging];
    library.client.servers.push(Server { key: gone, name: "Gone".into(), addrs, user: "ana".into(), admin: false });
    let started = std::time::Instant::now();
    library.refresh().await.unwrap();
    let took = started.elapsed();
    assert!(took >= Duration::from_secs(5) && took < Duration::from_secs(8), "{took:?}");
    assert_eq!(names(&library), one_down);
    assert!(library.connection(&a.key).is_some());

    // A fresh start shows the cached catalogs, every title unavailable until a refresh.
    let reopened = Library::open(Client::open(&home).unwrap());
    let all_down = vec![("Only A".into(), 1, false), ("Only B".into(), 1, false), ("Shared".into(), 2, false)];
    assert_eq!(names(&reopened), all_down);
    assert!(reopened.titles().iter().all(|t| merge::best_source(t).is_none()));
}

/// Waits (up to 20 s) for `check` to give something.
async fn until<T>(mut check: impl FnMut() -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(found) = check() {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap()
}

fn part_files(home: &Path) -> Vec<PathBuf> {
    let parts = std::fs::read_dir(home.join(flicks_client::download::DIR)).unwrap().flatten();
    parts.map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "part")).collect()
}

#[tokio::test]
async fn downloads_resume_verify_and_keep_progress_offline() {
    // 40 MB of varied bytes: twenty chunks, so an interruption lands mid-way.
    let samples = (0..40u32).map(|i| (0..1_000_000u32).map(|j| (j.wrapping_mul(7) ^ (i * 13)) as u8).collect());
    let media = mp4_samples(1280, 720, samples);
    let setup = setup_with("download", media.clone()).await;
    let home = dir("download-client");
    let mut client = Client::open(&home).unwrap();
    let server = client.pair(&link(&setup, "ana", setup.addr.to_string()).await, "Laptop").await.unwrap();
    let mut library = Library::open(client);
    until_titles(&mut library).await;
    let title = library.titles()[0].clone();
    let source = merge::best_source(&title).unwrap().clone();
    let connection = library.connection(&server.key).unwrap().clone();
    let total = media.len() as u64;
    let state = |downloads: &Downloads| downloads.get(&server.key, source.item.id).unwrap();

    // Dropping the manager mid-way is a crash: the partial file stays, and a new manager sees
    // the download paused at the bytes on disk.
    let downloads = Downloads::open(&home).unwrap();
    downloads.start(&connection, &source).unwrap();
    until(|| (state(&downloads).done >= 4 << 20).then_some(())).await;
    assert_eq!(state(&downloads).state, DownloadState::Downloading);
    drop(downloads);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let part = part_files(&home).pop().unwrap();
    let on_disk = std::fs::metadata(&part).unwrap().len();
    assert!(on_disk >= 4 << 20 && on_disk < total, "{on_disk} of {total}");
    let downloads = Downloads::open(&home).unwrap();
    let paused = state(&downloads);
    assert_eq!((paused.state, paused.done), (DownloadState::Paused, on_disk));
    assert_eq!(downloads.local_path(&title), None);

    // Resuming gives a byte-identical copy, with no partial file left.
    downloads.start(&connection, &source).unwrap();
    until(|| (state(&downloads).state == DownloadState::Complete).then_some(())).await;
    let local = downloads.local_path(&title).unwrap();
    assert_eq!(std::fs::read(&local).unwrap(), media);
    assert!(part_files(&home).is_empty());
    assert_eq!(downloads.local_file(&server.key, source.item.id), Some(local.clone()));

    // Removing deletes the copy and its record.
    downloads.remove(&server.key, source.item.id).await.unwrap();
    assert!(!local.exists() && downloads.list().is_empty() && downloads.local_path(&title).is_none());

    // Pause keeps the partial copy; a corrupted one is rejected by its hash when it ends, and
    // deleted.
    downloads.start(&connection, &source).unwrap();
    until(|| (state(&downloads).done >= 2 << 20).then_some(())).await;
    downloads.pause(&server.key, source.item.id).await;
    let paused = state(&downloads);
    assert_eq!(paused.state, DownloadState::Paused);
    assert!(paused.done < total);
    let part = part_files(&home).pop().unwrap();
    let mut bytes = std::fs::read(&part).unwrap();
    bytes[1000] ^= 0xff;
    std::fs::write(&part, bytes).unwrap();
    downloads.start(&connection, &source).unwrap();
    let failed = until(|| matches!(state(&downloads).state, DownloadState::Failed(_)).then(|| state(&downloads))).await;
    let DownloadState::Failed(why) = failed.state else { panic!() };
    assert!(why.contains("hash"), "{why}");
    assert!(part_files(&home).is_empty() && downloads.local_path(&title).is_none());
    // Trying again starts over and succeeds.
    downloads.start(&connection, &source).unwrap();
    until(|| (state(&downloads).state == DownloadState::Complete).then_some(())).await;
    assert_eq!(std::fs::read(downloads.local_path(&title).unwrap()).unwrap(), media);

    // Progress made while no server answers (a library that hasn't refreshed) is kept, newest
    // first, and reaches the server on the next refresh.
    let mut offline = Library::open(Client::open(&home).unwrap());
    let watched = Progress { position: 90_000, played: false, updated_at: 1_700_000_000_000 };
    downloads.set_progress(&server.key, source.item.id, watched).unwrap();
    let older = Progress { position: 1, updated_at: watched.updated_at - 1, ..watched };
    downloads.set_progress(&server.key, source.item.id, older).unwrap();
    assert_eq!(downloads.progress(&offline.titles()[0]), Some(watched));
    assert_eq!(downloads.sync(&offline).await.unwrap(), 0);
    let saved = |library: &Library| {
        let connection = library.connection(&server.key).unwrap().clone();
        async move {
            let path = format!("/progress/{}", source.item.id);
            connection.call::<Option<Progress>>(Method::GET, &path, None::<&()>).await.unwrap()
        }
    };
    offline.refresh().await.unwrap();
    assert_eq!(saved(&offline).await, None);
    assert_eq!(downloads.sync(&offline).await.unwrap(), 1);
    assert_eq!(saved(&offline).await, Some(watched));
    assert_eq!(downloads.sync(&offline).await.unwrap(), 0);
}

/// Refreshes the library until its first scan has put a title in it.
async fn until_titles(library: &mut Library) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while library.refresh().await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}
