//! The server on localhost and behind a local TLS-terminating proxy: pairing, signed requests,
//! replays, alterations, removed devices, and fuzzed unauthenticated routes.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Exercise the shipped worker, not a fixture decoder. Each child is reaped even on a hang.
fn still_worker(path: &Path, output: &Path, times: &[u32]) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let exe = std::env::var_os("FLICKS_STILL_EXE").unwrap_or_else(|| env!("CARGO_BIN_EXE_flicks").into());
    #[cfg(unix)]
    let mut command = {
        let mut c = Command::new("nice");
        c.args(["-n", "19"]).arg(exe);
        c
    };
    #[cfg(not(unix))]
    let mut command = Command::new(exe);
    let mut child = command
        .arg("--still-worker")
        .env("RAYON_NUM_THREADS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let job = serde_json::json!({"root":path.parent().unwrap(), "path":path, "size":path.metadata().unwrap().len(),
        "times":times, "output":output});
    child.stdin.take().unwrap().write_all(&serde_json::to_vec(&job).unwrap()).unwrap();
    let begin = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status.success();
        }
        if begin.elapsed() > Duration::from_secs(5) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("still worker exceeded deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn stills_software_codecs_and_hostile_files() {
    use std::process::Command;
    let ffmpeg = flicks_testkit::require_ffmpeg!();
    let dir = tempfile::tempdir().unwrap();
    let jpg = dir.path().join("still.jpg");
    for (codec, ext) in [("libx264", "mp4"), ("libx265", "mp4"), ("libvpx-vp9", "webm"), ("libsvtav1", "mp4")] {
        let path = dir.path().join(format!("{codec}.{ext}"));
        let result = Command::new(&ffmpeg)
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=160x96:rate=12",
                "-t",
                "2",
                "-c:v",
                codec,
                "-g",
                "12",
                "-pix_fmt",
                "yuv420p",
                "-threads",
                "1",
                "-y",
            ])
            .arg(&path)
            .output()
            .unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        assert!(still_worker(&path, &jpg, &[400, 1200]), "{codec}");
        let image = image::open(&jpg).unwrap().to_rgb8();
        assert_eq!(image.width() * 9, image.height() * 16);
        assert!(image.width() <= 1280);
        let bytes = std::fs::read(&path).unwrap();
        let bad = dir.path().join("truncated.mp4");
        for length in [0, 4, 32, bytes.len() / 2] {
            std::fs::write(&bad, &bytes[..length]).unwrap();
            let _ = still_worker(&bad, &jpg, &[400]); // partial MKVs may still contain a valid frame
        }
    }
    let black = dir.path().join("black.mp4");
    // Top/bottom, sides, and both must reproduce the unpadded content (within compression
    // error), not resize the black bars into the thumbnail.
    let mut reference: Option<Vec<u8>> = None;
    for pad in ["null", "pad=160:128:0:16", "pad=192:96:16:0", "pad=192:128:16:16"] {
        let path = dir.path().join("bars.mp4");
        assert!(
            Command::new(&ffmpeg)
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=160x96:rate=1",
                    "-vf",
                    pad,
                    "-frames:v",
                    "1",
                    "-c:v",
                    "libx264",
                    "-crf",
                    "18",
                    "-pix_fmt",
                    "yuv420p",
                    "-y"
                ])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        assert!(still_worker(&path, &jpg, &[0]), "{pad}");
        let image = image::open(&jpg).unwrap().to_rgb8();
        assert_eq!(image.dimensions(), (160, 90), "{pad}");
        let bytes = image.into_raw();
        if let Some(reference) = &reference {
            let error: u64 = bytes.iter().zip(reference).map(|(&a, &b)| u64::from(a.abs_diff(b))).sum();
            assert!(error < bytes.len() as u64 * 5, "{pad}: mean RGB error {}", error as f64 / bytes.len() as f64);
        } else {
            reference = Some(bytes);
        }
    }
    assert!(
        Command::new(&ffmpeg)
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=black:size=160x96:rate=12",
                "-t",
                "2",
                "-c:v",
                "libx264",
                "-g",
                "12",
                "-y"
            ])
            .arg(&black)
            .status()
            .unwrap()
            .success()
    );
    std::fs::remove_file(&jpg).unwrap();
    assert!(!still_worker(&black, &jpg, &[0, 400, 800, 1200]));
    assert!(!jpg.exists());
    let audio = dir.path().join("audio.mp4");
    assert!(
        Command::new(&ffmpeg)
            .args(["-v", "error", "-f", "lavfi", "-i", "sine=frequency=440", "-t", "1", "-c:a", "aac", "-y"])
            .arg(&audio)
            .status()
            .unwrap()
            .success()
    );
    assert!(!still_worker(&audio, &jpg, &[200]));
    assert!(!jpg.exists());
    // The first indexed candidate is black; a later keyframe must be tried.
    let later = dir.path().join("later.mp4");
    assert!(
        Command::new(&ffmpeg)
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=black:size=160x96:rate=12:duration=1",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=160x96:rate=12:duration=1",
                "-filter_complex",
                "[0:v][1:v]concat=n=2:v=1:a=0[v]",
                "-map",
                "[v]",
                "-c:v",
                "libx264",
                "-g",
                "12",
                "-y"
            ])
            .arg(&later)
            .status()
            .unwrap()
            .success()
    );
    assert!(still_worker(&later, &jpg, &[0, 1200]));
}

/// Opt-in measurement against read-only real media; all catalog/artwork writes use a temp dir.
#[tokio::test]
#[ignore = "set FLICKS_STILLS_MEDIA to a read-only media directory"]
async fn stills_real_media() {
    use http_body_util::BodyExt;
    use std::time::Instant;
    let root = std::env::var("FLICKS_STILLS_MEDIA").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let state = State::open(dir.path(), 0).unwrap();
    flicks_server::api::migrate(&state).await.unwrap();
    let artwork = dir.path().join("artwork");
    std::fs::create_dir_all(&artwork).unwrap();
    let tmdb = flicks_metadata::Tmdb::from_env();
    let keyed = tmdb.is_some();
    let began = Instant::now();
    let ids = state
        .db
        .call(move |conn| {
            flicks_server::db::add_folder(conn, &root)?;
            let report = flicks_library::Scanner::new(tmdb, artwork).scan(conn, Path::new(&root)).unwrap();
            assert_eq!(report.failed, 0);
            let mut stmt = conn.prepare("SELECT id FROM library_files ORDER BY episode")?;
            Ok(stmt.query_map([], |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap();
    println!("scan: {} ms, {} files, TMDB={keyed}", began.elapsed().as_millis(), ids.len());
    assert!(!ids.is_empty());
    for id in ids {
        let (item, keys) = state
            .db
            .call(move |conn| {
                Ok((flicks_library::item(conn, id).unwrap().unwrap(), flicks_library::keyframes(conn, id).unwrap()))
            })
            .await
            .unwrap();
        if item.still.is_some() {
            assert!(keyed);
            assert!(item.still.as_deref().unwrap().starts_with('/'));
            assert!(item.backdrop.as_deref().unwrap().starts_with('/'));
            assert_ne!(item.still, item.backdrop);
            println!("episode {:?}: TMDB episode still wins; series backdrop separate (no frame decode)", item.episode);
            continue;
        }
        let duration = item.duration_ms.unwrap();
        let times: Vec<_> = [20, 25, 15, 30]
            .into_iter()
            .filter_map(|percent| {
                keys.iter().rev().find(|k| u64::from(k.time_ms) <= duration * percent / 100).map(|k| k.time_ms)
            })
            .collect();
        let name = format!("still-v2-{}.jpg", item.oshash);
        let output = dir.path().join("artwork").join(&name);
        let mut elapsed = Vec::new();
        for _ in 0..5 {
            let started = Instant::now();
            assert!(still_worker(Path::new(&item.path), &output, &times));
            elapsed.push(started.elapsed().as_millis());
        }
        let image = image::open(&output).unwrap();
        println!(
            "episode {:?}: {:?} ms, {}x{}, {} bytes",
            item.episode,
            elapsed,
            image.width(),
            image.height(),
            output.metadata().unwrap().len()
        );
        state
            .db
            .call(move |conn| {
                conn.execute("UPDATE library_files SET still=?1 WHERE id=?2", rusqlite::params![name, id])?;
                Ok(())
            })
            .await
            .unwrap();
        let path = format!("/artwork/still-v2-{}.jpg", item.oshash);
        let auth = sign(&state.key, &state.public, "GET", &path, b"", b"", flicks_server::unix_now());
        let reply = server::handle(&state, signed_request(Method::GET, &path, &auth, None, b"")).await;
        assert_eq!(reply.status(), StatusCode::OK);
        assert_eq!(reply.headers()["content-type"], "image/jpeg");
        assert_eq!(reply.into_body().collect().await.unwrap().to_bytes().as_ref(), std::fs::read(&output).unwrap());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hls_forged_tokens_never_wait_for_database_and_are_rate_limited() {
    let server = start("hls-forgery").await;
    let (release, blocked) = std::sync::mpsc::channel();
    let (ready, entered) = tokio::sync::oneshot::channel();
    let db = server.state.db.clone();
    let job = tokio::spawn(async move {
        db.call(move |_| {
            ready.send(()).unwrap();
            blocked.recv().unwrap();
            Ok(())
        })
        .await
        .unwrap();
    });
    entered.await.unwrap();
    let path = format!(
        "/hls/1/master.m3u8?token=1.{}.{}.{}",
        flicks_server::unix_now() + 300,
        keys::encode(&[7; 16]),
        keys::encode(&[0; 32])
    );
    let outcome = tokio::time::timeout(std::time::Duration::from_millis(200), async {
        for i in 0..25 {
            let reply = server::handle(&server.state, get(&path)).await;
            assert_eq!(reply.status(), if i < 20 { StatusCode::UNAUTHORIZED } else { StatusCode::TOO_MANY_REQUESTS });
            assert_eq!(reply.headers()["referrer-policy"], "no-referrer");
        }
    })
    .await;
    release.send(()).unwrap();
    job.await.unwrap();
    outcome.expect("forged tokens must not queue behind the database");
}

/// Real HTTPS HLS oracle, including native-player grants (the oracle cannot sign
/// individual requests). FLICKS_HLS_MEDIA optionally supplies read-only real media.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hls_oracle() {
    if std::env::var_os("FLICKS_HLS_MEDIA").is_some() {
        hls_case("h264", "aac", false).await;
    } else {
        for (video, audio) in [("h264", "aac"), ("hevc", "aac"), ("h264", "ac3"), ("hevc", "eac3")] {
            hls_case(video, audio, false).await;
        }
        hls_case("hevc", "aac", true).await;
        hls_case("h264", "aac", true).await;
    }
}

async fn hls_case(video_codec: &str, audio_codec: &str, open_gop: bool) {
    use serde_json::Value;
    use std::process::Command;
    use std::time::Instant;
    async fn fetch(server: &Server, viewer: &Connection, path: &str) -> net::Response {
        let auth = sign(&viewer.key, &viewer.server_key, "GET", path, b"", b"", flicks_server::unix_now());
        https(server.addr, pinned(&server.state), signed_request(Method::GET, path, &auth, None, b"")).await.unwrap()
    }
    let (ffmpeg, ffprobe) = flicks_testkit::require_oracles!();
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("B frames.mkv");
    let mut generator = Command::new(&ffmpeg);
    generator.args(["-v", "error", "-y"]);
    if let Some(real) = std::env::var_os("FLICKS_HLS_MEDIA") {
        generator.arg("-i").arg(real).args(["-t", "16", "-map", "0:v:0", "-map", "0:a:0", "-c", "copy"]);
    } else {
        generator.args([
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=24",
            "-f",
            "lavfi",
            "-i",
            if open_gop && video_codec == "h264" {
                "sine=frequency=440:sample_rate=44100"
            } else {
                "sine=frequency=440:sample_rate=48000"
            },
            "-t",
            "12",
            "-c:v",
            if video_codec == "h264" { "libx264" } else { "libx265" },
            "-g",
            "48",
            "-bf",
            "3",
            if video_codec == "h264" { "-x264-params" } else { "-x265-params" },
            if open_gop { "scenecut=0:open-gop=1" } else { "scenecut=0:open-gop=0" },
            "-c:a",
            audio_codec,
        ]);
    }
    let output = generator.arg(&input).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let reference = tempfile::tempdir().unwrap();
    let source_input = reference.path().join("source.mkv");
    std::fs::copy(&input, &source_input).unwrap();
    // The audio sync flag is a property of the codec, not SimpleBlock's bit.
    // Clear every generated audio block's bit in-place without changing offsets.
    if std::env::var_os("FLICKS_HLS_MEDIA").is_none() {
        use flicks_container::matroska::{ebml, element_ids::*};
        let mut bytes = std::fs::read(&input).unwrap();
        let mut flags = Vec::new();
        let segment = ebml::Children::new(&bytes).find(|e| e.id == SEGMENT).unwrap();
        let base = segment.offset + segment.header_len;
        for cluster in ebml::Children::new(segment.data).filter(|e| e.id == CLUSTER) {
            for block in ebml::Children::new(cluster.data).filter(|e| e.id == SIMPLE_BLOCK) {
                let (track, len, _) = ebml::read_vint(block.data).unwrap();
                if track == 2 {
                    flags.push(base + cluster.offset + cluster.header_len + block.offset + block.header_len + len + 2);
                }
            }
        }
        assert!(flags.len() > 100);
        for offset in flags {
            bytes[offset] &= !0x80;
        }
        std::fs::write(&input, bytes).unwrap();
        let mut demux = flicks_container::matroska::Demuxer::new(std::fs::File::open(&input).unwrap()).unwrap();
        let mut audio = 0;
        while let Some(p) = demux.next_packet().unwrap() {
            if p.track == 1 && !p.keyframe {
                audio += 1;
            }
        }
        assert!(audio > 100);
    }
    let server = start(&format!("hls-{video_codec}-{audio_codec}-{open_gop}")).await;
    let viewer = pair_device(&server, server.addr, pinned(&server.state), "hls", false).await;
    let _: Folder = server
        .root
        .call(Method::POST, "/admin/folders", Some(&AddFolder { path: dir.path().to_string_lossy().into_owned() }))
        .await
        .unwrap();
    let items = catalog_ready(&viewer, 1).await;
    let id = items[0]["item"]["id"].as_i64().unwrap();
    let root = format!("/hls/{id}");
    let grant: Value = viewer.call(Method::POST, &format!("{root}/token"), None::<&()>).await.unwrap();
    let grant = grant["url"].as_str().unwrap();
    let query = grant.split_once('?').unwrap().1;
    // ffmpeg 8/9 does not propagate tls_verify to HLS child playlists. A test-only
    // loopback HTTP adapter calls the exact same authenticated handler; production
    // remains HTTPS-only. Latency and ordinary clients below use actual pinned TLS.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let oracle_addr = listener.local_addr().unwrap();
    let state = server.state.clone();
    let adapter = tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            let state = state.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    let state = state.clone();
                    async move { Ok::<_, std::convert::Infallible>(server::handle(&state, request).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(tcp), service)
                    .await;
            });
        }
    });
    let url = format!("http://{oracle_addr}{grant}");
    if let Some(script) = std::env::var_os("FLICKS_HLS_AVPLAYER") {
        let output = Command::new("swift").arg(script).arg(&url).output().unwrap();
        eprintln!(
            "HLS_AVPLAYER {video_codec}/{audio_codec} open={open_gop} exit={} {} {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success());
    }
    let response = fetch(&server, &viewer, &format!("{root}/master.m3u8")).await;
    assert_eq!(response.status, StatusCode::OK, "{}", String::from_utf8_lossy(&response.body));
    let master = String::from_utf8(response.body.to_vec()).unwrap();
    assert!(master.contains("TYPE=AUDIO"));
    assert!(!master.contains("#EXT-X-INDEPENDENT-SEGMENTS"));
    assert!(master.contains("CHANNELS=\""));
    assert!(master.contains("LANGUAGE=\""));
    let video = master.lines().find(|l| !l.starts_with('#')).unwrap().strip_suffix(".m3u8").unwrap();
    let segment_path = format!("{root}/{video}/0.m4s");
    let start = Instant::now();
    let first = fetch(&server, &viewer, &segment_path).await;
    let cold = start.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(first.status, StatusCode::OK);
    let mut times = Vec::new();
    for _ in 0..20 {
        let start = Instant::now();
        let response = fetch(&server, &viewer, &segment_path).await;
        times.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(response.body, first.body);
    }
    times.sort_by(f64::total_cmp);
    // Fetch out of order as well as repeatedly; each fragment is self-contained
    // with an absolute decode time, and audio segment zero must not disappear.
    let mut last_video = Vec::new();
    for track in [0, 1] {
        let playlist = fetch(&server, &viewer, &format!("{root}/{track}.m3u8")).await;
        assert_eq!(playlist.status, StatusCode::OK);
        let playlist = String::from_utf8(playlist.body.to_vec()).unwrap();
        let init = fetch(&server, &viewer, &format!("{root}/{track}/init.mp4")).await;
        let paths: Vec<_> = playlist.lines().filter(|s| s.ends_with(".m4s")).collect();
        assert!(paths.len() >= 2);
        for path in paths.into_iter().rev() {
            let a = fetch(&server, &viewer, &format!("{root}/{path}")).await;
            let b = fetch(&server, &viewer, &format!("{root}/{path}")).await;
            assert_eq!(a.status, StatusCode::OK, "fragment {path}; playlist {playlist}");
            assert_eq!(a.body, b.body);
            let mut bytes = init.body.to_vec();
            bytes.extend(&a.body);
            if track == 0 && last_video.is_empty() {
                last_video = bytes.clone();
            }
            let parsed = flicks_container::isobmff::open(bytes.as_slice()).unwrap();
            assert!(!parsed.tracks[0].samples.is_empty());
            if track == 1 {
                assert_eq!(
                    parsed.tracks[0].timescale,
                    parsed.tracks[0].entries[0].audio.as_ref().unwrap().sample_rate as u32
                );
                assert!(parsed.tracks[0].samples.iter().all(|s| s.is_sync));
                if audio_codec == "aac" {
                    assert!(parsed.tracks[0].samples.iter().all(|s| s.duration == 1024));
                }
            }
        }
    }
    eprintln!(
        "HLS_PERF {video_codec}/{audio_codec} open={open_gop} bytes={} cold_ms={cold:.3} n=20 p50_ms={:.3} p95_ms={:.3} max_ms={:.3}",
        first.body.len(),
        times[9],
        times[18],
        times[19]
    );
    if !cfg!(debug_assertions) {
        assert!(cold < 50.0 && times[18] < 50.0);
    }
    let probe = Command::new(&ffprobe)
        .args(["-v", "error", "-rw_timeout", "10000000", "-show_streams", "-of", "json"])
        .arg(&url)
        .output()
        .unwrap();
    assert!(probe.status.success(), "{}", String::from_utf8_lossy(&probe.stderr));
    let streams: Value = serde_json::from_slice(&probe.stdout).unwrap();
    assert!(
        streams["streams"].as_array().unwrap().iter().any(|s| s["codec_name"] == video_codec),
        "{streams}; {}",
        String::from_utf8_lossy(&probe.stderr)
    );
    assert!(streams["streams"].as_array().unwrap().iter().any(|s| s["codec_name"] == audio_codec));
    for seek in ["0", "8"] {
        let decoded = Command::new(&ffmpeg)
            .args(["-v", "error", "-xerror", "-rw_timeout", "10000000", "-ss", seek, "-i"])
            .arg(&url)
            .args(["-t", "2", "-map", "0:v:0", "-map", "0:a:0", "-f", "null", "-"])
            .output()
            .unwrap();
        assert!(decoded.status.success(), "seek {seek}: {}", String::from_utf8_lossy(&decoded.stderr));
        if open_gop && video_codec == "h264" {
            // x264 recovery-point seeks also warn on the original MKV. Require
            // the same pixels below; do not mistake a source warning for remux loss.
            let source = Command::new(&ffmpeg)
                .args(["-v", "error", "-xerror", "-ss", seek, "-i"])
                .arg(&source_input)
                .args(["-t", "2", "-f", "null", "-"])
                .output()
                .unwrap();
            assert!(source.status.success());
            assert_eq!(decoded.stderr.is_empty(), source.stderr.is_empty());
        } else {
            assert!(decoded.stderr.is_empty(), "seek {seek}: {}", String::from_utf8_lossy(&decoded.stderr));
        }
    }
    // All decoded video pixels, not merely a parser accepting the boxes.
    let hashes = |source: &str, network: bool, seek: Option<&str>| {
        let mut command = Command::new(&ffmpeg);
        command.args(["-v", "error", "-xerror"]);
        if network {
            command.args(["-rw_timeout", "10000000"]);
        }
        if let Some(seek) = seek {
            command.args(["-ss", seek]);
        }
        let output = command.args(["-i", source, "-map", "0:v:0", "-f", "framemd5", "-"]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter(|s| !s.starts_with('#'))
            .map(|s| s.rsplit(',').next().unwrap().trim().to_owned())
            .collect::<Vec<_>>()
    };
    let all = hashes(&url, true, None);
    assert!(all.len() >= 280);
    assert_eq!(all, hashes(source_input.to_str().unwrap(), false, None));
    if open_gop {
        // Start a new decoder directly on the later segment, not on a preceding
        // HLS rendition. Leading pictures must not appear or corrupt its frames.
        let path = reference.path().join("last-segment.mp4");
        std::fs::write(&path, last_video).unwrap();
        let standalone = hashes(path.to_str().unwrap(), false, None);
        assert!(standalone.len() >= 90);
        assert!(all.ends_with(&standalone), "standalone open-GOP segment differs from source tail");
    }
    // Account for the common reordering delay when comparing absolute audio PTS.
    // Player seeks remain relative to playback start, as with an ordinary file.
    let init = fetch(&server, &viewer, &format!("{root}/{video}/init.mp4")).await;
    let mut bytes = init.body.to_vec();
    bytes.extend(&first.body);
    let doc = flicks_container::isobmff::open(bytes.as_slice()).unwrap();
    let first_sample = doc.tracks[0].samples.get(0).unwrap();
    let delay = (first_sample.pts - first_sample.dts) as f64 / f64::from(doc.tracks[0].timescale);
    let sought = hashes(&url, true, Some("8"));
    assert!(sought.len() >= 90);
    let source_sought = hashes(source_input.to_str().unwrap(), false, Some("8"));
    assert!(
        sought == source_sought,
        "seek video frames differ: HLS {} source {}, first HLS/source positions {:?}/{:?}",
        sought.len(),
        source_sought.len(),
        all.iter().position(|h| h == &sought[0]),
        all.iter().position(|h| h == &source_sought[0])
    );
    let audio_time = |source: &str, target: f64| {
        let output = Command::new(&ffprobe)
            .args([
                "-v",
                "error",
                "-select_streams",
                "a:0",
                "-show_entries",
                "frame=pts_time,nb_samples:stream=sample_rate",
                "-read_intervals",
                &format!("{target:.6}%+6"),
                "-of",
                "json",
                source,
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let doc: Value = serde_json::from_slice(&output.stdout).unwrap();
        let rate: f64 = doc["streams"][0]["sample_rate"].as_str().unwrap().parse().unwrap();
        let frame = doc["frames"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["pts_time"].as_str().and_then(|s| s.parse::<f64>().ok()).is_some_and(|p| p >= target))
            .unwrap();
        (
            frame["pts_time"].as_str().unwrap().parse::<f64>().unwrap(),
            frame["nb_samples"].as_u64().unwrap() as f64 / rate,
        )
    };
    for target in [0.0, 8.0] {
        let (hls, frame) = audio_time(&url, target + delay);
        let (source, _) = audio_time(source_input.to_str().unwrap(), target);
        let error = (hls - delay - source).abs();
        eprintln!(
            "HLS_AUDIO {video_codec}/{audio_codec} target={target} error_ms={:.3} frame_ms={:.3}",
            error * 1000.0,
            frame * 1000.0
        );
        assert!(
            error <= if audio_codec == "aac" { frame.min(0.023) } else { frame },
            "audio offset {:.3} ms exceeds one frame {:.3} ms",
            error * 1000.0,
            frame * 1000.0
        );
    }
    let unsigned = https(server.addr, pinned(&server.state), get(&format!("{root}/master.m3u8"))).await.unwrap();
    assert_eq!(unsigned.status, StatusCode::UNAUTHORIZED);
    for bad in [
        format!("/hls/{}/master.m3u8?{query}", id + 1),
        format!("/files/{id}?{query}"),
        format!("{root}/master.m3u8?{query}x"),
    ] {
        assert_eq!(
            https(server.addr, pinned(&server.state), get(&bad)).await.unwrap().status,
            StatusCode::UNAUTHORIZED
        );
    }
    // The URL is valid before revocation, and unusable immediately afterwards.
    assert_eq!(https(server.addr, pinned(&server.state), get(grant)).await.unwrap().status, StatusCode::OK);
    let key = keys::public_bytes(viewer.key.verifying_key());
    server
        .state
        .db
        .call(move |conn| {
            conn.execute("DELETE FROM devices WHERE key=?1", [key])?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(https(server.addr, pinned(&server.state), get(grant)).await.unwrap().status, StatusCode::UNAUTHORIZED);
    adapter.abort();
}

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
    // And it still answers.
    let response = https(server.addr, pinned(state), get("/info?probe=AAAAAAAAAAAAAAAAAAAAAA")).await.unwrap();
    assert_eq!(response.status, StatusCode::OK);
}

/// On the paused clock, so how fast this machine answers can't change the count.
#[tokio::test(start_paused = true)]
async fn unauthenticated_requests_share_one_rate_limit() {
    let state = State::open(&data_dir("rate-limit"), 0).unwrap();
    let mut statuses = Vec::new();
    for _ in 0..40 {
        statuses.push(server::handle(&state, get("/info?probe=AAAAAAAAAAAAAAAAAAAAAA")).await.status());
    }
    // A burst of 20 is answered and the rest refused.
    assert_eq!(statuses[..20], [StatusCode::OK; 20]);
    assert_eq!(statuses[20..], [StatusCode::TOO_MANY_REQUESTS; 20]);
    // It refills at 10 a second.
    tokio::time::advance(std::time::Duration::from_millis(100)).await;
    assert_eq!(server::handle(&state, get("/info?probe=AAAAAAAAAAAAAAAAAAAAAA")).await.status(), StatusCode::OK);
    let refused = server::handle(&state, get("/info?probe=AAAAAAAAAAAAAAAAAAAAAA")).await.status();
    assert_eq!(refused, StatusCode::TOO_MANY_REQUESTS);
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

async fn api_request(state: &State, client: &Connection, path: &str, range: Option<&str>) -> server::Reply {
    let auth = sign(
        &client.key,
        &client.server_key,
        "GET",
        path,
        range.unwrap_or("").as_bytes(),
        b"",
        flicks_server::unix_now(),
    );
    server::handle(state, signed_request(Method::GET, path, &auth, range, b"")).await
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
        assert_eq!(api_request(&server.state, &viewer, path, None).await.status(), StatusCode::BAD_REQUEST);
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
            conn.execute(
                "UPDATE library_files SET poster='/p1.jpg', backdrop='/b1.jpg', still='/e1.jpg' WHERE id=?1",
                [id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let detail: Value = viewer.call(Method::GET, &format!("/items/{id}"), None::<&()>).await.unwrap();
    assert_eq!(detail["item"]["poster"], "w342-p1.jpg");
    assert_eq!(detail["item"]["backdrop"], "w780-b1.jpg");
    assert_eq!(detail["item"]["still"], "w780-e1.jpg");
    let list: Value = viewer.call(Method::GET, "/items", None::<&()>).await.unwrap();
    assert_eq!(list[0]["item"], detail["item"]);
    let art_path = "/artwork/w342-p1.jpg";
    let reply = api_request(&server.state, &viewer, art_path, None).await;
    assert_eq!(reply.status(), StatusCode::OK);
    assert_eq!(reply.headers()["content-type"], "image/jpeg");
    assert_eq!(reply.into_body().collect().await.unwrap().to_bytes().as_ref(), jpeg);
    assert_eq!(server::handle(&server.state, get(art_path)).await.status(), StatusCode::UNAUTHORIZED);
    std::fs::write(art.join("still-v2-0123456789abcdef.jpg"), jpeg).unwrap();
    server
        .state
        .db
        .call(move |conn| {
            conn.execute("UPDATE library_files SET still='still-v2-0123456789abcdef.jpg' WHERE id=?1", [id])?;
            Ok(())
        })
        .await
        .unwrap();
    let detail: Value = viewer.call(Method::GET, &format!("/items/{id}"), None::<&()>).await.unwrap();
    assert_eq!(detail["item"]["backdrop"], "w780-b1.jpg");
    assert_eq!(detail["item"]["still"], "still-v2-0123456789abcdef.jpg");
    let list: Value = viewer.call(Method::GET, "/items", None::<&()>).await.unwrap();
    assert_eq!(list[0]["item"], detail["item"]);
    let reply = api_request(&server.state, &viewer, "/artwork/still-v2-0123456789abcdef.jpg", None).await;
    assert_eq!(reply.status(), StatusCode::OK);
    assert_eq!(reply.headers()["content-type"], "image/jpeg");
    assert_eq!(reply.into_body().collect().await.unwrap().to_bytes().as_ref(), jpeg);
    for path in [
        "/artwork/../server.key",
        "/artwork/%2e%2e%2fserver.key",
        "/artwork/w342-..jpg",
        "/artwork/w342-x/y.jpg",
        "/artwork/w342-.jpg",
        "/artwork/w100-p1.jpg",
    ] {
        assert_eq!(api_request(&server.state, &viewer, path, None).await.status(), StatusCode::BAD_REQUEST);
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(server.state.data_dir.join("server.key"), art.join("w342-secret.jpg")).unwrap();
        assert_eq!(
            api_request(&server.state, &viewer, "/artwork/w342-secret.jpg", None).await.status(),
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
    let now_ms = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap();
    for bad in [
        json!({"position": -1,"played":false,"updated_at":2000}),
        json!({"position": 0,"played":false,"updated_at":-1}),
        json!({"position": 0,"played":false,"updated_at":2000,"user":1}),
        // More than the 60 s of allowed clock skew ahead.
        json!({"position": 0,"played":false,"updated_at":now_ms + 3_600_000}),
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
        let reply = api_request(&server.state, &viewer, &path, range).await;
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
        let reply = api_request(&server.state, &viewer, &path, Some(range)).await;
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
    // A clock a little ahead is fine.
    let skewed = json!({"position": 5000, "played": true, "updated_at": now_ms + 30_000});
    let result: Value = viewer.call(Method::PUT, &p, Some(&skewed)).await.unwrap();
    assert_eq!(result, skewed);

    // A watcher rescans after a new file appears, without another folder edit.
    std::fs::write(media.path().join("Second.mp4"), &bytes).unwrap();
    catalog_ready(&viewer, 2).await;
    std::fs::remove_file(media.path().join("Second.mp4")).unwrap();
    catalog_ready(&viewer, 1).await;
    server.root.call::<()>(Method::DELETE, &format!("/admin/folders/{}", folder.id), None::<&()>).await.unwrap();
    assert_eq!(api_request(&server.state, &viewer, &path, None).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(api_request(&server.state, &viewer, &format!("/play/{id}"), None).await.status(), StatusCode::NOT_FOUND);
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
    // No scan worker: under load it could read the folder list after the folder below is added,
    // then watch it and rescan the files this test swaps out, rewriting the stale entries it checks.
    let state = State::open(&data_dir("catalog-paths"), 0).unwrap();
    flicks_server::api::migrate(&state).await.unwrap();
    // Only signs; requests go straight to `server::handle`.
    let signer = Connection::pinned(state.key.clone(), state.key.verifying_key(), "unused:0".into()).unwrap();
    let media = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let bytes = catalog_mp4();
    let root = std::fs::canonicalize(media.path()).unwrap();
    let path = root.join("Movie.mp4");
    std::fs::write(&path, &bytes).unwrap();
    // Populate a real scan; the stale catalog entry must remain while its path is replaced.
    let r = root.clone();
    let item = state
        .db
        .call(move |conn| {
            flicks_server::db::add_folder(conn, r.to_str().unwrap())?;
            flicks_library::Scanner::new(None, r.join("artwork")).scan(conn, &r).unwrap();
            Ok(flicks_library::item(conn, 1).unwrap().unwrap())
        })
        .await
        .unwrap();
    assert_eq!(
        api_request(&state, &signer, &format!("/files/{}", item.id), Some("bytes=0-0")).await.status(),
        StatusCode::PARTIAL_CONTENT
    );
    // A truncated-to-empty file is a valid empty 200 without Range; no byte range can satisfy it.
    std::fs::write(&path, []).unwrap();
    let empty = api_request(&state, &signer, &format!("/files/{}", item.id), None).await;
    assert_eq!(empty.status(), StatusCode::OK);
    assert_eq!(empty.headers()["content-length"], "0");
    let empty = api_request(&state, &signer, &format!("/files/{}", item.id), Some("bytes=0-")).await;
    assert_eq!(empty.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(empty.headers()["content-range"], "bytes */0");
    std::fs::write(outside.path().join("secret"), b"private").unwrap();
    std::fs::remove_file(&path).unwrap();
    symlink(outside.path().join("secret"), &path).unwrap();
    for prefix in ["files", "play"] {
        assert_eq!(
            api_request(&state, &signer, &format!("/{prefix}/{}", item.id), None).await.status(),
            StatusCode::FORBIDDEN
        );
    }
    // Even an in-root symlink is refused.
    std::fs::remove_file(&path).unwrap();
    std::fs::write(root.join("inside"), &bytes).unwrap();
    symlink(root.join("inside"), &path).unwrap();
    assert_eq!(
        api_request(&state, &signer, &format!("/files/{}", item.id), None).await.status(),
        StatusCode::FORBIDDEN
    );
    // A directory component repointed after scanning is refused too.
    symlink(outside.path(), root.join("linked")).unwrap();
    let linked = root.join("linked/secret");
    state
        .db
        .call(move |conn| {
            conn.execute("UPDATE library_files SET path=?1 WHERE id=?2", rusqlite::params![linked.to_str(), item.id])?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        api_request(&state, &signer, &format!("/files/{}", item.id), None).await.status(),
        StatusCode::FORBIDDEN
    );
    // Lexical traversal in a corrupt/stale catalog cannot escape either.
    let escaped = root.join("..").join(outside.path().file_name().unwrap()).join("secret");
    state
        .db
        .call(move |conn| {
            conn.execute("UPDATE library_files SET path=?1 WHERE id=?2", rusqlite::params![escaped.to_str(), item.id])?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        api_request(&state, &signer, &format!("/files/{}", item.id), None).await.status(),
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

/// FLK-26 acceptance against the actual release `flicks serve` with an isolated HOME:
/// generated 16 s 4K HEVC Main10 PQ (closed and open GOP, 5.1 AAC) and, with
/// FLICKS_TRANSCODE_MEDIA=<file>, one real read-only episode (its folder is added as is).
/// Every rendition (direct, v1080/v720/v480, stereo audio) plays in AVPlayer from the start
/// and after a seek through a test-only HTTP adapter that forwards to the real pinned-HTTPS
/// server and keeps one variant in the master. Prints HLS_TRANSCODE_PERF / HLS_AVPLAYER /
/// HLS_PSNR lines for docs/perf.md.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "binds port 7686, encodes 4K fixtures and plays in AVPlayer; run alone in release mode"]
async fn transcode_ladder_avplayer() {
    use serde_json::Value;
    use std::collections::HashMap;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    struct Process(std::process::Child);
    impl Drop for Process {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    struct Source {
        label: String,
        path: PathBuf,
        width: u32,
        height: u32,
        fps: f64,
        pq: bool,
        seek: f64,
    }
    struct Variant {
        uri: String,
        attrs: HashMap<String, String>,
    }
    fn attributes(line: &str) -> HashMap<String, String> {
        let mut out = HashMap::new();
        let (mut key, mut value, mut in_key, mut quoted) = (String::new(), String::new(), true, false);
        for c in line.split_once(':').map_or("", |(_, b)| b).chars().chain(std::iter::once(',')) {
            match c {
                '"' => quoted = !quoted,
                '=' if in_key => in_key = false,
                ',' if !quoted => {
                    out.insert(std::mem::take(&mut key), std::mem::take(&mut value));
                    in_key = true;
                }
                c if in_key => key.push(c),
                c => value.push(c),
            }
        }
        out
    }
    fn bare(uri: &str) -> String {
        uri.split('?').next().unwrap_or_default().to_owned()
    }
    /// (EXT-X-MAP URI, [(EXTINF text, segment URI)]), queries removed.
    fn media_playlist(text: &str) -> (String, Vec<(String, String)>) {
        let init = text.lines().find(|l| l.starts_with("#EXT-X-MAP:")).map(|l| bare(&attributes(l)["URI"])).unwrap();
        let mut segments = Vec::new();
        let mut duration = None;
        for line in text.lines() {
            if let Some(d) = line.strip_prefix("#EXTINF:") {
                duration = Some(d.trim_end_matches(',').to_owned());
            } else if !line.starts_with('#') && !line.is_empty() {
                segments.push((duration.take().unwrap(), bare(line)));
            }
        }
        (init, segments)
    }
    fn probe(ffprobe: &Path, args: &[&str], input: &Path) -> Value {
        let output = Command::new(ffprobe).args(["-v", "error", "-of", "json"]).args(args).arg(input).output().unwrap();
        assert!(output.status.success(), "ffprobe {}: {}", input.display(), String::from_utf8_lossy(&output.stderr));
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn text(v: &Value) -> &str {
        v.as_str().unwrap_or_default()
    }
    fn idr(data: &[u8], hevc: bool) -> bool {
        let mut rest = data;
        while let Some(len) = rest.get(..4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize) {
            let Some(&head) = rest.get(4..4 + len).and_then(|n| n.first()) else {
                return false;
            };
            let found = if hevc { matches!((head >> 1) & 0x3f, 19 | 20) } else { head & 0x1f == 5 };
            if found {
                return true;
            }
            rest = &rest[4 + len..];
        }
        false
    }
    async fn fetch(viewer: &Connection, path: &str) -> net::Response {
        let auth = sign(&viewer.key, &viewer.server_key, "GET", path, b"", b"", flicks_server::unix_now());
        net::send(&viewer.addr, viewer.tls.clone(), signed_request(Method::GET, path, &auth, None, b"")).await.unwrap()
    }
    /// The test-only adapter: `/only/<variant uri>/hls/...` forwards `/hls/...` (query intact) to
    /// the real HTTPS server and, for the master, keeps only that variant's STREAM-INF.
    async fn relay(
        upstream: &str,
        tls: Arc<ClientConfig>,
        log: &Mutex<Vec<String>>,
        request: Request<hyper::body::Incoming>,
    ) -> hyper::Response<Full<Bytes>> {
        let reply = |status: StatusCode, body: Vec<u8>, mime: &'static str| {
            hyper::Response::builder()
                .status(status)
                .header(hyper::header::CONTENT_TYPE, mime)
                .body(Full::new(Bytes::from(body)))
                .unwrap()
        };
        let path = request.uri().path().to_owned();
        let query = request.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
        let Some((only, rest)) = path.strip_prefix("/only/").and_then(|p| p.split_once('/')) else {
            return reply(StatusCode::NOT_FOUND, Vec::new(), "text/plain");
        };
        let rest = format!("/{rest}");
        let Ok(response) = net::send(upstream, tls, get(&format!("{rest}{query}"))).await else {
            return reply(StatusCode::BAD_GATEWAY, Vec::new(), "text/plain");
        };
        log.lock().unwrap().push(format!("{} {rest}", response.status.as_u16()));
        let mime = if rest.ends_with(".m3u8") { "application/vnd.apple.mpegurl" } else { "video/mp4" };
        if response.status != StatusCode::OK || !rest.ends_with("/master.m3u8") || only == "auto" {
            return reply(response.status, response.body.to_vec(), mime);
        }
        let (mut out, mut pending, mut found) = (String::new(), None, false);
        for line in String::from_utf8_lossy(&response.body).lines() {
            if line.starts_with("#EXT-X-STREAM-INF:") {
                pending = Some(line.to_owned());
            } else if let Some(inf) = pending.take().filter(|_| !line.starts_with('#')) {
                if bare(line) == only {
                    out.push_str(&format!("{inf}\n{line}\n"));
                    found = true;
                }
            } else {
                out.push_str(&format!("{line}\n"));
            }
        }
        if !found {
            return reply(StatusCode::NOT_FOUND, Vec::new(), "text/plain");
        }
        reply(StatusCode::OK, out.into_bytes(), mime)
    }

    let (ffmpeg, ffprobe) = flicks_testkit::require_oracles!();
    if !flicks_testkit::ffmpeg_has_encoder(&ffmpeg, "libx265") {
        flicks_testkit::skip("transcode_ladder_avplayer", "ffmpeg lacks libx265");
        return;
    }
    assert!(std::net::TcpStream::connect("127.0.0.1:7686").is_err(), "port 7686 already in use");
    let work = tempfile::tempdir().unwrap();
    let player = work.path().join("avplayer");
    let compiled = Command::new("swiftc")
        .args(["-O", "-o"])
        .arg(&player)
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/avplayer.swift"))
        .output()
        .unwrap();
    assert!(compiled.status.success(), "swiftc: {}", String::from_utf8_lossy(&compiled.stderr));
    let hdr_eligible = Command::new(&player).arg("--hdr-eligible").output().unwrap().stdout.starts_with(b"true");
    let has_scale_vt = Command::new(&ffmpeg).args(["-hide_banner", "-filters"]).output().is_ok_and(|o| {
        String::from_utf8_lossy(&o.stdout).lines().any(|l| l.split_whitespace().nth(1) == Some("scale_vt"))
    });

    // Fixtures: 4 s closed or open GOPs, nominal bitrate above the 1080p tier's 8 Mbps.
    let synthetic = flicks_testkit::fixtures_dir("transcode/18mbps-pq");
    let mut sources = Vec::new();
    for open_gop in [false, true] {
        let path = synthetic.join(format!("PQ {} GOP.mkv", if open_gop { "open" } else { "closed" }));
        let params = format!(
            "pools=4:frame-threads=2:keyint=96:min-keyint=96:scenecut=0:open-gop={}:bframes=3:repeat-headers=1:hdr10=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):max-cll=1000,400",
            u8::from(open_gop)
        );
        flicks_testkit::fixtures::generate(&path, |tmp| {
        let output = Command::new(&ffmpeg)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=3840x2160:rate=24"])
            .args([
                "-f",
                "lavfi",
                "-i",
                "aevalsrc=0.2*sin(2*PI*220*t)|0.2*sin(2*PI*330*t)|0.2*sin(2*PI*440*t)|0.1*sin(2*PI*60*t)|0.2*sin(2*PI*550*t)|0.2*sin(2*PI*660*t):s=48000:c=5.1",
            ])
            .args(["-t", "16", "-pix_fmt", "yuv420p10le", "-c:v", "libx265", "-preset", "ultrafast", "-b:v", "18M", "-maxrate", "20M", "-bufsize", "10M"])
            .args(["-x265-params", &params])
            .args(["-color_primaries", "bt2020", "-color_trc", "smpte2084", "-colorspace", "bt2020nc"])
            .args(["-c:a", "aac", "-b:a", "384k"])
            .arg(tmp)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        true
        }).unwrap();
        let info = probe(
            &ffprobe,
            &[
                "-read_intervals",
                "%+#16",
                "-show_frames",
                "-show_entries",
                "format=bit_rate:stream=codec_name,profile,channels:frame=media_type,pix_fmt,color_transfer,color_primaries",
            ],
            &path,
        );
        let streams = info["streams"].as_array().unwrap();
        assert_eq!((text(&streams[0]["codec_name"]), text(&streams[0]["profile"])), ("hevc", "Main 10"));
        let picture = info["frames"].as_array().unwrap().iter().find(|f| f["media_type"] == "video").unwrap();
        assert_eq!(
            (text(&picture["color_transfer"]), text(&picture["color_primaries"]), text(&picture["pix_fmt"])),
            ("smpte2084", "bt2020", "yuv420p10le")
        );
        assert_eq!(streams[1]["channels"], 6);
        let bitrate: u64 = text(&info["format"]["bit_rate"]).parse().unwrap();
        assert!(bitrate > 8_200_000, "fixture bitrate {bitrate} must exceed the 1080p tier");
        eprintln!("HLS_FIXTURE {} bitrate={bitrate}", path.display());
        let label = format!("4k-pq-{}", if open_gop { "open" } else { "closed" });
        sources.push(Source { label, path, width: 3840, height: 2160, fps: 24.0, pq: true, seek: 9.0 });
    }
    let real = std::env::var_os("FLICKS_TRANSCODE_MEDIA").map(PathBuf::from);
    if let Some(path) = &real {
        let info = probe(
            &ffprobe,
            &["-select_streams", "v:0", "-show_entries", "stream=codec_name,width,height,r_frame_rate:format=duration"],
            path,
        );
        let stream = &info["streams"][0];
        assert_eq!(stream["codec_name"], "h264", "FLICKS_TRANSCODE_MEDIA must be an H.264 episode");
        let (n, d) = text(&stream["r_frame_rate"]).split_once('/').unwrap();
        let duration: f64 = text(&info["format"]["duration"]).parse().unwrap();
        sources.push(Source {
            label: "real-h264".into(),
            path: path.clone(),
            width: stream["width"].as_u64().unwrap() as u32,
            height: stream["height"].as_u64().unwrap() as u32,
            fps: n.parse::<f64>().unwrap() / d.parse::<f64>().unwrap(),
            pq: false,
            seek: (duration / 2.0).floor() + 0.5,
        });
    }

    // The production server, its logs kept to check that no grant is ever written.
    let home = tempfile::tempdir().unwrap();
    let process = Process(
        Command::new(env!("CARGO_BIN_EXE_flicks"))
            .arg("serve")
            .env("HOME", home.path())
            .env_remove("FLICKS_TMDB_KEY")
            .stdout(Stdio::from(std::fs::File::create(work.path().join("serve.out")).unwrap()))
            .stderr(Stdio::from(std::fs::File::create(work.path().join("serve.err")).unwrap()))
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
    let link = open_window(&root, "transcode", false).await;
    let key = keys::generate().unwrap();
    let request = pair::Request::new(&key, &root.server_key, &link.code, "AVPlayer").unwrap();
    let paired =
        net::send(&root.addr, root.tls.clone(), post_json("/pair/redeem", serde_json::to_vec(&request).unwrap()))
            .await
            .unwrap();
    assert_eq!(paired.status, StatusCode::OK);
    let viewer =
        Connection { key, server_key: root.server_key.clone(), addr: root.addr.clone(), tls: root.tls.clone() };
    let mut folders = vec![synthetic];
    folders.extend(real.as_ref().and_then(|p| p.parent()).map(Path::to_path_buf));
    for folder in folders {
        let _: Folder = root
            .call(Method::POST, "/admin/folders", Some(&AddFolder { path: folder.to_string_lossy().into_owned() }))
            .await
            .unwrap();
    }
    let sizes: Vec<u64> = sources.iter().map(|s| std::fs::metadata(&s.path).unwrap().len()).collect();
    let ids: Vec<i64> = tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            let items: Vec<Value> = viewer.call(Method::GET, "/items", None::<&()>).await.unwrap();
            let ids: Vec<_> = sizes
                .iter()
                .filter_map(|size| {
                    items.iter().find(|i| i["item"]["size"] == *size).and_then(|i| i["item"]["id"].as_i64())
                })
                .collect();
            if ids.len() == sizes.len() {
                break ids;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("sources catalogued");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adapter_addr = listener.local_addr().unwrap();
    let served: Arc<Mutex<Vec<String>>> = Arc::default();
    let (upstream, upstream_tls, log) = (viewer.addr.clone(), viewer.tls.clone(), served.clone());
    let adapter = tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let (upstream, tls, log) = (upstream.clone(), upstream_tls.clone(), log.clone());
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    let (upstream, tls, log) = (upstream.clone(), tls.clone(), log.clone());
                    async move { Ok::<_, std::convert::Infallible>(relay(&upstream, tls, &log, request).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(tcp), service)
                    .await;
            });
        }
    });

    let mut tokens = Vec::new();
    for (source, id) in sources.iter().zip(ids) {
        let label = &source.label;
        let frame = 1.0 / source.fps;
        let response = fetch(&viewer, &format!("/hls/{id}/master.m3u8")).await;
        assert_eq!(response.status, StatusCode::OK, "{label}: {}", String::from_utf8_lossy(&response.body));
        let master = String::from_utf8(response.body.to_vec()).unwrap();
        eprintln!("HLS_MASTER {label}\n{master}");
        let lines: Vec<&str> = master.lines().collect();
        let variants: Vec<Variant> = lines
            .windows(2)
            .filter(|w| w[0].starts_with("#EXT-X-STREAM-INF:"))
            .map(|w| Variant { uri: bare(w[1]), attrs: attributes(w[0]) })
            .collect();
        let bandwidth = |v: &Variant| v.attrs["BANDWIDTH"].parse::<u64>().unwrap();
        let direct = variants
            .iter()
            .find(|v| v.uri.strip_suffix(".m3u8").is_some_and(|s| s.parse::<usize>().is_ok()))
            .expect("direct rendition");
        if source.pq {
            assert_eq!(direct.attrs.get("VIDEO-RANGE").map(String::as_str), Some("PQ"));
        }
        let tiers: Vec<&Variant> = variants.iter().filter(|v| v.uri != direct.uri).collect();
        assert!(!tiers.is_empty(), "{label}: no transcoded tiers");
        for tier in &tiers {
            assert!(["v1080.m3u8", "v720.m3u8", "v480.m3u8"].contains(&tier.uri.as_str()), "{label}: {}", tier.uri);
            assert!(bandwidth(tier) < bandwidth(direct), "{label}: {} is not smaller than direct", tier.uri);
        }
        if source.pq {
            assert_eq!(tiers.len(), 3, "{label}: a 4K source offers v1080, v720 and v480");
        }
        let media: Vec<_> = lines.iter().filter(|l| l.starts_with("#EXT-X-MEDIA:")).map(|l| attributes(l)).collect();
        let stereo = media
            .iter()
            .find(|m| {
                m.get("TYPE").is_some_and(|t| t == "AUDIO")
                    && m.get("CHANNELS").is_some_and(|c| c == "2")
                    && tiers.iter().all(|t| t.attrs.get("AUDIO") == m.get("GROUP-ID"))
            })
            .expect("every transcoded tier uses a stereo audio group");
        let stereo_uri = bare(&stereo["URI"]);

        // Every rendition shares the direct rendition's cuts exactly.
        let mut ladder = Vec::new();
        for uri in std::iter::once(&direct.uri).chain(tiers.iter().map(|t| &t.uri)).chain([&stereo_uri]) {
            let playlist = fetch(&viewer, &format!("/hls/{id}/{uri}")).await;
            assert_eq!(playlist.status, StatusCode::OK, "{label} {uri}");
            ladder.push((uri.clone(), media_playlist(std::str::from_utf8(&playlist.body).unwrap())));
        }
        let cuts: Vec<String> = ladder[0].1.1.iter().map(|(d, _)| d.clone()).collect();
        for (uri, (_, segments)) in &ladder {
            assert_eq!(segments.iter().map(|(d, _)| d.clone()).collect::<Vec<_>>(), cuts, "{label}: {uri} cuts differ");
        }
        let starts: Vec<f64> = cuts
            .iter()
            .scan(0.0, |t, d| {
                let start = *t;
                *t += d.parse::<f64>().unwrap();
                Some(start)
            })
            .collect();
        let n = cuts.len();
        assert!(n >= 4, "{label}: {n} segments");
        let mut picks = vec![n / 2, 1, n - 1];
        if source.pq {
            picks.push(0);
        }
        picks.dedup();

        // Direct segments: keyframe presentation times, and catalogued cuts are real source keyframes.
        let (direct_init, direct_segments) = &ladder[0].1;
        let direct_init = fetch(&viewer, &format!("/hls/{id}/{direct_init}")).await.body;
        let mut direct_start = HashMap::new();
        let mut expected_counts = HashMap::new();
        for &seq in &picks {
            let segment = fetch(&viewer, &format!("/hls/{id}/{}", direct_segments[seq].1)).await;
            assert_eq!(segment.status, StatusCode::OK);
            let mut bytes = direct_init.to_vec();
            bytes.extend(&segment.body);
            let doc = flicks_container::isobmff::open(bytes.as_slice()).unwrap();
            let track = &doc.tracks[0];
            // The keyframe's presentation time (open-GOP leading pictures present before it).
            direct_start.insert(seq, track.samples.get(0).unwrap().pts as f64 / f64::from(track.timescale));
            let start = starts[seq];
            let end = start + cuts[seq].parse::<f64>().unwrap();
            let packets = probe(
                &ffprobe,
                &[
                    "-select_streams",
                    "v:0",
                    "-read_intervals",
                    &format!("{:.3}%{:.3}", (start - 1.0).max(0.0), end + 1.0),
                    "-show_entries",
                    "packet=pts_time,flags",
                ],
                &source.path,
            );
            // Audio sometimes outlasts video; count actual pictures, not EXTINF × fps.
            expected_counts.insert(
                seq,
                packets["packets"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|p| {
                        text(&p["pts_time"]).parse::<f64>().is_ok_and(|t| t >= start - 0.000001 && t < end - 0.000001)
                    })
                    .count(),
            );
            assert!(
                packets["packets"].as_array().unwrap().iter().any(|p| text(&p["flags"]).contains('K')
                    && (text(&p["pts_time"]).parse::<f64>().unwrap() - start).abs() <= frame + 0.001),
                "{label}: cut {start} is not within one frame of a source keyframe"
            );
        }

        // Transcoded tiers: nonsequential segments, timing, codec, size, frames, IDR, alignment, PSNR.
        let mut sizes = HashMap::new();
        for (uri, (init_uri, segments)) in ladder.iter().skip(1).take(tiers.len()) {
            let tier = tiers.iter().find(|t| &t.uri == uri).unwrap();
            let init = fetch(&viewer, &format!("/hls/{id}/{init_uri}")).await;
            assert_eq!(init.status, StatusCode::OK);
            for (i, &seq) in picks.iter().enumerate() {
                let path = format!("/hls/{id}/{}", segments[seq].1);
                let started = Instant::now();
                let segment = fetch(&viewer, &path).await;
                let cold = started.elapsed().as_secs_f64() * 1000.0;
                assert_eq!(segment.status, StatusCode::OK, "{label} {path}");
                let started = Instant::now();
                let repeat = fetch(&viewer, &path).await;
                let repeat_ms = started.elapsed().as_secs_f64() * 1000.0;
                assert_eq!(repeat.status, StatusCode::OK);
                assert_eq!(segment.body, repeat.body, "LRU repeat changed");
                let after = fetch(&viewer, &format!("/hls/{id}/{init_uri}")).await;
                assert_eq!(after.body, init.body, "init must be stable across independent encoder sessions");
                let seconds: f64 = cuts[seq].parse().unwrap();
                let kbps = segment.body.len() as f64 * 8.0 / seconds / 1000.0;
                // /init generated segment zero and put it in the LRU; don't
                // report a cache-hit latency as thousands of times realtime.
                if seq != 0 {
                    eprintln!(
                        "HLS_TRANSCODE_PERF source={label} tier={uri} seq={seq} seg_s={seconds:.3} bytes={} kbps={kbps:.0} cold_ms={cold:.1} repeat_ms={repeat_ms:.1} realtime_x={:.2}",
                        segment.body.len(),
                        seconds * 1000.0 / cold
                    );
                }
                if i == 0 && !cfg!(debug_assertions) {
                    assert!(seconds * 1000.0 / cold >= 2.0, "{label} {uri}: segment {seq} encodes under 2x real time");
                }
                assert!(kbps * 1000.0 <= bandwidth(tier) as f64 * 1.5, "{label} {uri}: {kbps} kbps over BANDWIDTH");
                let mut bytes = init.body.to_vec();
                bytes.extend(&segment.body);
                let file = work.path().join(format!("{label}-{}-{seq}.mp4", uri.trim_end_matches(".m3u8")));
                std::fs::write(&file, &bytes).unwrap();
                let info = probe(
                    &ffprobe,
                    &[
                        "-select_streams",
                        "v:0",
                        "-count_frames",
                        "-show_frames",
                        "-show_entries",
                        "stream=codec_name,profile,width,height,nb_read_frames:frame=pix_fmt,color_transfer,color_primaries",
                    ],
                    &file,
                );
                let stream = &info["streams"][0];
                let codec = text(&stream["codec_name"]);
                assert!(codec == "h264" || codec == "hevc", "{label} {uri}: {codec}");
                let (w, h) = (stream["width"].as_u64().unwrap() as u32, stream["height"].as_u64().unwrap() as u32);
                let box_h: u32 = uri.trim_start_matches('v').trim_end_matches(".m3u8").parse().unwrap();
                let aspect = f64::from(source.width) / f64::from(source.height);
                assert!(w < source.width && h < source.height && h <= box_h && w % 2 == 0 && h % 2 == 0, "{w}x{h}");
                assert!((f64::from(w) / f64::from(h) / aspect - 1.0).abs() < 0.01, "{label} {uri}: aspect {w}x{h}");
                assert!(h == box_h || f64::from(w) >= f64::from(box_h) * 16.0 / 9.0 - 16.0, "{label} {uri}: {w}x{h}");
                if let Some(resolution) = tier.attrs.get("RESOLUTION") {
                    assert_eq!(resolution, &format!("{w}x{h}"));
                }
                sizes.insert(uri.clone(), (w, h));
                let frames: usize = text(&stream["nb_read_frames"]).parse().unwrap();
                assert_eq!(frames, expected_counts[&seq], "{label} {uri} seq {seq}: frame count");
                let doc = flicks_container::isobmff::open(bytes.as_slice()).unwrap();
                let track = &doc.tracks[0];
                let first = track.samples.get(0).unwrap();
                let data = bytes.get(first.offset as usize..first.offset as usize + first.size as usize).unwrap();
                assert!(
                    first.is_sync && idr(data, codec == "hevc"),
                    "{label} {uri} seq {seq}: does not start with IDR"
                );
                let start = first.pts as f64 / f64::from(track.timescale);
                assert!(
                    (start - direct_start[&seq]).abs() <= frame + 0.001,
                    "{label} {uri} seq {seq}: starts at {start}, direct at {}",
                    direct_start[&seq]
                );
                assert_eq!(codec, "h264");
                for f in info["frames"].as_array().unwrap() {
                    assert_eq!(
                        (text(&f["color_transfer"]), text(&f["color_primaries"]), text(&f["pix_fmt"])),
                        ("bt709", "bt709", "yuv420p")
                    );
                }
                if i != 0 {
                    continue;
                }
                // Compare the same presented pictures on the same clock after independently
                // demuxing/decoding both inputs. The HDR oracle also applies Apple's transfer.
                let hw = source.pq;
                let reference = if hw {
                    assert!(has_scale_vt, "PQ oracle needs ffmpeg scale_vt");
                    format!(
                        "scale_vt=w={w}:h={h}:color_matrix=bt709:color_primaries=bt709:color_transfer=bt709,hwdownload,format=p010le"
                    )
                } else {
                    format!("scale={w}:{h}:flags=lanczos")
                };
                let mut command = Command::new(&ffmpeg);
                command.args(["-hide_banner", "-nostats", "-v", "info", "-i"]).arg(&file);
                if hw {
                    command.args(["-hwaccel", "videotoolbox", "-hwaccel_output_format", "videotoolbox_vld"]);
                }
                let output = command
                    .args(["-seek_timestamp", "1", "-ss", &format!("{:.6}", (starts[seq] - frame / 2.0).max(0.0)), "-t", &cuts[seq], "-i"])
                    .arg(&source.path)
                    .args([
                        "-lavfi",
                        &format!(
                            "[0:v]settb=1/24,setpts=N,format=yuv420p[a];[1:v]{reference},settb=1/24,setpts=N,format=yuv420p[b];[a][b]psnr=shortest=1"
                        ),
                        "-f",
                        "null",
                        "-",
                    ])
                    .output()
                    .unwrap();
                let log = String::from_utf8_lossy(&output.stderr);
                assert!(output.status.success(), "{log}");
                let average = log.rsplit("average:").next().and_then(|s| s.split_whitespace().next()).unwrap();
                let psnr = if average == "inf" { f64::INFINITY } else { average.parse::<f64>().unwrap() };
                eprintln!(
                    "HLS_PSNR source={label} tier={uri} seq={seq} oracle={} average_db={psnr:.2}",
                    if hw { "scale_vt" } else { "scale" }
                );
                assert!(psnr >= if hw { 28.0 } else { 30.0 }, "{label} {uri}: PSNR {psnr:.2} dB");
            }
        }

        // Stereo audio: AAC, two channels, and each picked segment covers its cut.
        let (init, segments) = &ladder.last().unwrap().1;
        let init = fetch(&viewer, &format!("/hls/{id}/{init}")).await;
        for &seq in &picks {
            let segment = fetch(&viewer, &format!("/hls/{id}/{}", segments[seq].1)).await;
            assert_eq!(segment.status, StatusCode::OK);
            let mut bytes = init.body.to_vec();
            bytes.extend(&segment.body);
            let file = work.path().join(format!("{label}-stereo-{seq}.mp4"));
            std::fs::write(&file, &bytes).unwrap();
            let info = probe(&ffprobe, &["-show_entries", "stream=codec_name,channels,sample_rate"], &file);
            assert_eq!(
                (text(&info["streams"][0]["codec_name"]), &info["streams"][0]["channels"]),
                ("aac", &Value::from(2))
            );
            if source.pq && seq == picks[0] {
                // The fixture puts distinct notes into L, R and C. The actual encoded
                // stereo output must keep L/R separate and spread C to both speakers.
                let raw = Command::new(&ffmpeg)
                    .args(["-v", "error", "-i"])
                    .arg(&file)
                    .args(["-t", "0.5", "-f", "f32le", "pipe:1"])
                    .output()
                    .unwrap();
                assert!(raw.status.success());
                let pcm: Vec<_> =
                    raw.stdout.as_chunks::<4>().0.iter().map(|b| f64::from(f32::from_le_bytes(*b))).collect();
                let frames = pcm.as_chunks::<2>().0;
                let amplitude = |channel: usize, hz: f64| {
                    let angle = std::f64::consts::TAU * hz / 48000.;
                    let (mut a, mut b) = (0., 0.);
                    for (i, x) in frames.iter().enumerate() {
                        let (sin, cos) = (angle * i as f64).sin_cos();
                        a += sin * x[channel];
                        b += cos * x[channel];
                    }
                    2. * a.hypot(b) / frames.len() as f64
                };
                assert!(amplitude(0, 220.) > 0.1 && amplitude(1, 330.) > 0.1);
                assert!(amplitude(1, 220.) < 0.03 && amplitude(0, 330.) < 0.03);
                assert!(amplitude(0, 440.) > 0.07 && amplitude(1, 440.) > 0.07);
            }
            let doc = flicks_container::isobmff::open(bytes.as_slice()).unwrap();
            let track = &doc.tracks[0];
            let seconds =
                track.samples.iter().map(|s| u64::from(s.duration)).sum::<u64>() as f64 / f64::from(track.timescale);
            assert!(
                (seconds - cuts[seq].parse::<f64>().unwrap()).abs() <= 0.05,
                "{label} stereo seq {seq}: {seconds} s"
            );
        }

        // Each rendition in AVPlayer from the start and after a seek past the first segments.
        let seek_seq = starts.iter().rposition(|s| *s <= source.seek).unwrap();
        assert!(seek_seq >= 2);
        let stereo_dir = stereo_uri.trim_end_matches(".m3u8");
        let auto = Variant { uri: "auto".into(), attrs: HashMap::new() };
        // Also play the intact native master, with no test-side variant filtering.
        for variant in
            std::iter::once(direct).chain(tiers.iter().copied()).chain((source.pq && !hdr_eligible).then_some(&auto))
        {
            let grant: Value = viewer.call(Method::POST, &format!("/hls/{id}/token"), None::<&()>).await.unwrap();
            let grant = grant["url"].as_str().unwrap().to_owned();
            let token = grant.split_once("token=").unwrap().1.to_owned();
            // On an SDR-only display AVPlayer correctly refuses a PQ-only multivariant
            // playlist. Force the original video via its *actual authenticated media*
            // playlist in this case; never falsify VIDEO-RANGE to make the test pass.
            let grant = if source.pq && variant.uri == direct.uri && !hdr_eligible {
                eprintln!(
                    "HLS_DIRECT_PQ {label}: AVPlayer HDR-ineligible display; testing direct video media playlist"
                );
                grant.replacen("master.m3u8", &direct.uri, 1)
            } else {
                grant
            };
            served.lock().unwrap().clear();
            let (program, url, seek) = (
                player.clone(),
                format!("http://{adapter_addr}/only/{}{grant}", variant.uri),
                format!("{:.3}", source.seek),
            );
            let output =
                tokio::task::spawn_blocking(move || Command::new(program).arg(url).arg(seek).output().unwrap())
                    .await
                    .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).replace(&token, "REDACTED");
            assert!(!stdout.contains(&token), "AVPlayer report leaked the grant");
            let runs: Vec<Value> = stdout
                .lines()
                .filter_map(|l| l.strip_prefix("AVPLAYER "))
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            let served = served.lock().unwrap().clone();
            eprintln!("HLS_AVPLAYER_LOG source={label} variant={} {stdout}{stderr}\nserved: {served:?}", variant.uri);
            assert!(output.status.success() && runs.len() == 2, "{label} {}: AVPlayer failed", variant.uri);
            let expected = if variant.uri == direct.uri {
                Some((source.width, source.height))
            } else {
                sizes.get(&variant.uri).copied()
            };
            for run in &runs {
                let access = run["access"].as_array().unwrap();
                let last = access.last().cloned().unwrap_or_default();
                eprintln!(
                    "HLS_AVPLAYER source={label} variant={} phase={} ready_ms={:.0} seek_ms={:.0} first_frame_ms={:.0} size={}x{} indicated_bps={} observed_bps={} stalls={} errors={}",
                    variant.uri,
                    text(&run["phase"]),
                    run["ready_ms"].as_f64().unwrap_or(f64::NAN),
                    run["seek_ms"].as_f64().unwrap_or(0.0),
                    run["first_frame_ms"].as_f64().unwrap_or(f64::NAN),
                    run["width"],
                    run["height"],
                    last["indicated_bps"],
                    last["observed_bps"],
                    last["stalls"],
                    run["errors"].as_array().map_or(0, Vec::len)
                );
                let decoded = (run["width"].as_u64().unwrap() as u32, run["height"].as_u64().unwrap() as u32);
                if let Some(size) = expected {
                    assert_eq!(decoded, size);
                } else {
                    assert!(sizes.values().any(|s| *s == decoded), "HDR-ineligible player must choose an SDR tier");
                }
                assert!(!access.is_empty(), "{label} {}: empty access log", variant.uri);
                assert!(run["errors"].as_array().unwrap().is_empty(), "{label} {}: {}", variant.uri, run["errors"]);
            }
            let dir = variant.uri.trim_end_matches(".m3u8");
            assert!(served.iter().all(|s| s.starts_with("200 ")), "{label} {}: {served:?}", variant.uri);
            assert!(
                served.iter().any(|s| if variant.uri == "auto" {
                    tiers.iter().any(|t| s.ends_with(&format!("/{}/{seek_seq}.m4s", t.uri.trim_end_matches(".m3u8"))))
                } else {
                    s.ends_with(&format!("/{dir}/{seek_seq}.m4s"))
                }),
                "{label} {}: no seek segment",
                variant.uri
            );
            if variant.uri != direct.uri {
                assert!(served.iter().any(|s| s.contains(&format!("/{stereo_dir}/")) && s.ends_with(".m4s")));
            }
            tokens.push(token);
        }

        // Ordinary refusals through the real server: unknown tier, past the end, altered or doubled credentials.
        let grant: Value = viewer.call(Method::POST, &format!("/hls/{id}/token"), None::<&()>).await.unwrap();
        let grant = grant["url"].as_str().unwrap().to_owned();
        let query = grant.split_once('?').unwrap().1.to_owned();
        tokens.push(query.trim_start_matches("token=").to_owned());
        let tier = tiers[0].uri.trim_end_matches(".m3u8");
        for (path, expected) in [
            (format!("/hls/{id}/v9999.m3u8?{query}"), StatusCode::NOT_FOUND),
            (format!("/hls/{id}/{tier}/{n}.m4s?{query}"), StatusCode::NOT_FOUND),
            (format!("/hls/{id}/{tier}/init.mp4?{query}x"), StatusCode::UNAUTHORIZED),
        ] {
            assert_eq!(
                net::send(&viewer.addr, viewer.tls.clone(), get(&path)).await.unwrap().status,
                expected,
                "unexpected status for HLS refusal"
            );
        }
        let path = format!("/hls/{id}/{tier}/init.mp4?{query}");
        let auth = sign(&viewer.key, &viewer.server_key, "GET", &path, b"", b"", flicks_server::unix_now());
        let doubled = signed_request(Method::GET, &path, &auth, None, b"");
        assert_eq!(
            net::send(&viewer.addr, viewer.tls.clone(), doubled).await.unwrap().status,
            StatusCode::UNAUTHORIZED
        );
    }
    adapter.abort();
    drop(process);
    for name in ["serve.out", "serve.err"] {
        let log = std::fs::read_to_string(work.path().join(name)).unwrap_or_default();
        for token in &tokens {
            let tag = token.rsplit('.').next().unwrap();
            assert!(!log.contains(tag), "{name} contains an HLS grant");
        }
    }
}
