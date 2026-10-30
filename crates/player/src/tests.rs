use super::*;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn fixture(container: &str, video: &str, audio: &str, size: &str, seconds: u32) -> PathBuf {
    let path = flicks_testkit::temp_path(&flicks_testkit::fixtures_dir("player").join(format!("fixture.{container}")));
    let mut cmd = Command::new(flicks_testkit::ffmpeg().unwrap());
    cmd.args([
        "-v",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        &format!("testsrc2=size={size}:rate=24"),
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=997:sample_rate=48000",
        "-t",
        &seconds.to_string(),
        "-c:v",
        video,
    ]);
    match video {
        "libx264" => {
            cmd.args(["-preset", "fast", "-x264-params", "threads=2:keyint=24:min-keyint=24:scenecut=0:bframes=3"]);
        }
        "libx265" => {
            cmd.args([
                "-preset",
                "fast",
                "-pix_fmt",
                "yuv420p10le",
                "-x265-params",
                "pools=2:frame-threads=2:keyint=24:min-keyint=24:scenecut=0:log-level=error",
            ]);
        }
        "libvpx-vp9" => {
            cmd.args(["-deadline", "realtime", "-cpu-used", "8"]);
        }
        "libsvtav1" => {
            cmd.args(["-preset", "12", "-svtav1-params", "lp=2"]);
        }
        _ => {}
    }
    cmd.args(["-c:a", audio, "-ac", "2"]);
    assert!(cmd.arg(&path).status().unwrap().success());
    path
}

fn player(path: &Path) -> Player {
    Player::with_audio(Source::Local(path.to_owned()), AudioSink::Null).unwrap()
}

fn run(p: &mut Player, seconds: f64, paced: bool) -> (f64, f64) {
    p.play();
    let start = Instant::now();
    let mut maximum = 0f64;
    let mut last = -1.0;
    for tick in 0..((seconds + 2.0) * 240.0) as u64 {
        let now = start + Duration::from_secs_f64(tick as f64 / 240.0);
        if paced {
            std::thread::sleep(now.saturating_duration_since(Instant::now()));
        }
        if let Some(frame) = p.next_frame_for_display(if paced { Instant::now() } else { now }).unwrap() {
            assert!(frame.pts() >= last, "out of presentation order");
            last = frame.pts();
            maximum = maximum.max(p.stats().av_offset.abs());
        }
        if p.state() == State::Ended {
            break;
        }
    }
    assert_eq!(
        p.state(),
        State::Ended,
        "stuck at {} video_end={} audio_end={} frames={}",
        p.position,
        p.video_end,
        p.audio_end,
        p.frames.len()
    );
    (start.elapsed().as_secs_f64(), maximum)
}

#[test]
fn playback_seek_pause_tracks_and_subtitles() {
    let _ff = flicks_testkit::require_ffmpeg!();
    for extension in ["mp4", "mkv"] {
        let path = fixture(extension, "libx264", "aac", "160x96", 3);
        let mut p = player(&path);
        assert_eq!(p.state(), State::Paused);
        let (_, offset) = run(&mut p, 3.0, false);
        assert_eq!(p.stats().displayed, 72);
        assert_eq!(p.stats().dropped, 0);
        assert!(offset < 0.020, "offset={offset}");
        let now = Instant::now();
        p.seek(1.4).unwrap();
        assert_eq!(p.state(), State::Paused);
        let frame = p.next_frame_for_display(now).unwrap().unwrap();
        assert!((1.4..1.5).contains(&frame.pts()));
        p.set_subtitles(Some(flicks_subtitle::parse_srt(b"1\n00:00:01,000 --> 00:00:02,000\nhello\n").unwrap()));
        assert_eq!(p.active_cues()[0].text(), "hello");
        assert!(p.select_audio(Some(999)).is_err());
        assert!(p.select_subtitle(Some(0)).is_err());
        p.play();
        p.next_frame_for_display(now).unwrap();
        p.next_frame_for_display(now + Duration::from_millis(100)).unwrap();
        p.pause().unwrap();
        let frozen = p.position();
        p.next_frame_for_display(now + Duration::from_secs(1)).unwrap();
        assert_eq!(p.position(), frozen);
        p.select_audio(None).unwrap();
        assert_eq!(p.selected_audio(), None);
        assert!(p.seek(f64::NAN).is_err());
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn software_codecs_and_audio_layouts() {
    let _ff = flicks_testkit::require_ffmpeg!();
    for (video, audio) in [("libx264", "aac"), ("libx265", "ac3"), ("libvpx-vp9", "libopus"), ("libsvtav1", "eac3")] {
        let path = fixture("mkv", video, audio, "128x96", 1);
        let mut p = player(&path);
        p.software = true;
        p.seek(0.0).unwrap();
        let (_, offset) = run(&mut p, 1.0, false);
        assert_eq!(p.stats().displayed, 24);
        assert_eq!(p.stats().dropped, 0);
        assert!(offset < 0.020, "{video}/{audio} offset={offset}");
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn truncated_and_hostile_files_fail_without_panicking() {
    let _ff = flicks_testkit::require_ffmpeg!();
    for extension in ["mp4", "mkv"] {
        let path = fixture(extension, "libx264", "aac", "64x64", 1);
        let bytes = std::fs::read(&path).unwrap();
        for cut in [0, 1, 16, bytes.len() / 2, bytes.len() - 100] {
            let bad = flicks_testkit::temp_path(&path);
            std::fs::write(&bad, &bytes[..cut]).unwrap();
            let result = Player::with_audio(Source::Local(bad.clone()), AudioSink::Null).and_then(|mut p| {
                p.play();
                let start = Instant::now();
                for tick in 0..1000 {
                    p.next_frame_for_display(start + Duration::from_millis(tick * 5))?;
                }
                Ok(())
            });
            assert!(result.is_err(), "accepted {extension} truncation at {cut}/{}", bytes.len());
            std::fs::remove_file(bad).unwrap();
        }
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn hardware_failure_replays_from_keyframe_and_late_display_counts_drops() {
    let _ff = flicks_testkit::require_ffmpeg!();
    let path = fixture("mp4", "libx264", "aac", "160x96", 2);
    let mut p = player(&path);
    if p.is_hardware() {
        let Codec::Avc { mut avcc } = p.tracks[p.video_track].codec.clone() else { panic!("avc") };
        avcc[4] &= 0xfc; // A valid configuration with the wrong NAL length field size.
        p.video = video::Decoder::Hardware(flicks_platform::Decoder::new(flicks_platform::Codec::H264, &avcc).unwrap());
        run(&mut p, 2.0, false);
        assert_eq!(p.stats().hardware_fallbacks, 1);
        assert_eq!(p.stats().displayed, 48);
        assert_eq!(p.stats().dropped, 0);
    }
    let mut p = player(&path);
    let now = Instant::now();
    p.play();
    p.next_frame_for_display(now).unwrap();
    p.next_frame_for_display(now + Duration::from_millis(250)).unwrap();
    assert!(p.stats().dropped >= 4);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn embedded_tracks_and_active_subtitles() {
    let ff = flicks_testkit::require_ffmpeg!();
    let seed = fixture("mkv", "libx264", "aac", "160x96", 3);
    let srt = seed.with_extension("srt");
    std::fs::write(&srt, "1\n00:00:00,200 --> 00:00:02,500\nhello\n").unwrap();
    let path = flicks_testkit::temp_path(&seed);
    assert!(
        Command::new(ff)
            .args(["-v", "error", "-y", "-i"])
            .arg(&seed)
            .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-i"])
            .arg(&srt)
            .args([
                "-t",
                "3",
                "-map",
                "0:v",
                "-map",
                "0:a",
                "-map",
                "1:a",
                "-map",
                "2:s",
                "-c:v",
                "copy",
                "-c:a",
                "aac",
                "-c:s",
                "srt",
                "-disposition:a:0",
                "0",
                "-disposition:a:1",
                "default",
                "-disposition:s:0",
                "default"
            ])
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let mut p = player(&path);
    assert_eq!(p.selected_audio(), Some(2));
    assert_eq!(p.selected_subtitle(), Some(3));
    p.select_audio(Some(1)).unwrap();
    assert_eq!(p.selected_audio(), Some(1));
    p.play();
    let start = Instant::now();
    for tick in 0..241 {
        p.next_frame_for_display(start + Duration::from_secs_f64(tick as f64 / 240.0)).unwrap();
    }
    assert_eq!(p.active_cues()[0].text(), "hello");
    p.pause().unwrap();
    p.seek(1.3).unwrap();
    p.next_frame_for_display(Instant::now()).unwrap();
    assert_eq!(p.active_cues().len(), 1, "cue spanning the seek target");
    let mut fresh = player(&path);
    fresh.seek(1.3).unwrap();
    fresh.next_frame_for_display(Instant::now()).unwrap();
    assert_eq!(fresh.active_cues().len(), 1, "indexed cue on a first seek");
    p.select_subtitle(None).unwrap();
    assert!(p.active_cues().is_empty());
    for path in [seed, srt, path] {
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
#[ignore = "4K software throughput, not a real-time requirement"]
fn software_4k_throughput() {
    let path = fixture("mkv", "libx265", "aac", "3840x2160", 1);
    let mut p = player(&path);
    p.software = true;
    p.seek(0.0).unwrap();
    let (elapsed, _) = run(&mut p, 1.0, false);
    eprintln!(
        "4K HEVC software: {} frames / {elapsed:.3}s = {:.1}fps",
        p.stats().decoded,
        p.stats().decoded as f64 / elapsed
    );
    assert_eq!(p.stats().displayed, 24);
    std::fs::remove_file(path).unwrap();
}

#[test]
#[ignore = "real-time playback and seek measurement"]
fn realtime_and_seek() {
    for (video, size) in [("libx264", "1920x1080"), ("libx265", "3840x2160")] {
        for container in ["mkv", "mp4"] {
            let path = fixture(container, video, "aac", size, 10);
            let start = Instant::now();
            let mut p = player(&path);
            let open = start.elapsed();
            let (elapsed, offset) = run(&mut p, 10.0, true);
            eprintln!(
                "{video} {size} {container}: open={open:?}, elapsed={elapsed:.3}s, stats={:?}, max_offset={:.3}ms",
                p.stats(),
                offset * 1000.0
            );
            assert_eq!(p.stats().dropped, 0);
            assert!(offset < 0.020);
            for target in [1.0, 8.0, 3.0] {
                let start = Instant::now();
                p.seek(target).unwrap();
                let frame = p.next_frame_for_display(Instant::now()).unwrap().unwrap();
                let elapsed = start.elapsed();
                eprintln!("seek {container}/{video} {target}s -> {:.3}s: {elapsed:?}", frame.pts());
                assert!(elapsed < Duration::from_millis(100));
            }
            std::fs::remove_file(path).unwrap();
        }
    }
}

#[test]
#[ignore = "read-only real-file check; FLICKS_PLAYER_MEDIA names a file"]
fn real_file() {
    let path = PathBuf::from(std::env::var_os("FLICKS_PLAYER_MEDIA").unwrap());
    let mut p = player(&path);
    eprintln!(
        "real-file duration={} hardware={} tracks={:?}",
        p.duration(),
        p.is_hardware(),
        p.tracks().iter().map(|t| (t.kind, t.codec.name())).collect::<Vec<_>>()
    );
    p.play();
    let start = Instant::now();
    for tick in 0..2400 {
        p.next_frame_for_display(start + Duration::from_secs_f64(tick as f64 / 240.0)).unwrap();
    }
    eprintln!("real-file first 10s: {:?}, wall={:?}", p.stats(), start.elapsed());
    assert_eq!(p.stats().dropped, 0);
    for target in [60.0, 600.0, 120.0] {
        let start = Instant::now();
        p.pause().unwrap();
        p.seek(target).unwrap();
        let frame = p.next_frame_for_display(Instant::now()).unwrap().unwrap();
        eprintln!("real-file seek {target} -> {}: {:?}", frame.pts(), start.elapsed());
    }
}

#[test]
fn network_ranges_and_seeks() {
    use flicks_container::matroska::ByteSource;
    use flicks_server::{db, server};
    use std::sync::Arc;
    let _ff = flicks_testkit::require_ffmpeg!();
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let temp = tempfile::tempdir_in(flicks_testkit::fixtures_dir("player")).unwrap();
    let media = temp.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let path = fixture("mp4", "libx264", "aac", "1920x1080", 4);
    let bytes = std::fs::read(&path).unwrap();
    std::fs::rename(&path, media.join("Fixture.2026.mp4")).unwrap();
    let (connection, item) = runtime.block_on(async {
        let (addr, state) = server::spawn(&temp.path().join("server"), "127.0.0.1:0".parse().unwrap()).await.unwrap();
        let root =
            flicks_client::Connection::pinned(state.key.clone(), state.key.verifying_key(), addr.to_string()).unwrap();
        let _: db::Folder = root
            .call(
                hyper::Method::POST,
                "/admin/folders",
                Some(&server::AddFolder { path: media.to_string_lossy().into_owned() }),
            )
            .await
            .unwrap();
        let pairing: server::Pairing = root
            .call(
                hyper::Method::POST,
                "/admin/pair",
                Some(&server::OpenPairing { user: "player".into(), admin: false, addrs: vec![addr.to_string()] }),
            )
            .await
            .unwrap();
        let mut client = flicks_client::Client::open(&temp.path().join("client")).unwrap();
        let server = client.pair(&pairing.link, "Headless Player").await.unwrap();
        let connection = Arc::new(client.connect(&server.key).await.unwrap());
        let entries = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let entries = connection.items().await.unwrap();
                if !entries.is_empty() {
                    break entries;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        (connection, entries[0].item.clone())
    });
    let source = || Source::Network { connection: connection.clone(), file: item.id, size: item.size };
    let reader = source::Bytes::open(source()).unwrap();
    let mut buf = vec![0; 300_000];
    for at in [0, 300_000, 20, 500_000, 17] {
        reader.read_at(at, &mut buf).unwrap();
        assert_eq!(buf, bytes[at as usize..at as usize + buf.len()]);
    }
    reader.invalidate().unwrap();
    reader.read_at(2, &mut buf).unwrap();
    assert_eq!(buf, bytes[2..2 + buf.len()]);
    drop(reader);
    let mut p = Player::with_audio(source(), AudioSink::Null).unwrap();
    let (_, offset) = run(&mut p, 4.0, false);
    assert_eq!(p.stats().displayed, 96);
    assert_eq!(p.stats().dropped, 0);
    assert!(offset < 0.020);
    for at in [1.0, 3.0, 0.0, 2.0] {
        let start = Instant::now();
        p.seek(at).unwrap();
        assert!(p.next_frame_for_display(Instant::now()).unwrap().is_some());
        let elapsed = start.elapsed();
        eprintln!("local TLS server seek {at}s: {elapsed:?}");
        assert!(elapsed < Duration::from_millis(300));
    }
    drop(p);
    drop(runtime);
}

#[test]
#[ignore = "two-hour paced playback; generates ~2 GB; takes two hours"]
fn two_hour_playback() {
    let ff = flicks_testkit::require_ffmpeg!();
    let seed = fixture("mp4", "libx264", "aac", "1920x1080", 2);
    let mp4 = flicks_testkit::temp_path(&flicks_testkit::fixtures_dir("player").join("two-hour.mp4"));
    let mkv = mp4.with_extension("mkv");
    assert!(
        Command::new(&ff)
            .args(["-v", "error", "-y", "-stream_loop", "-1", "-i"])
            .arg(&seed)
            .args([
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=997:sample_rate=48000",
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-t",
                "7200",
                "-frames:v",
                "172800",
                "-c:v",
                "copy",
                "-c:a",
                "aac",
                "-ac",
                "2"
            ])
            .arg(&mp4)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new(&ff)
            .args(["-v", "error", "-y", "-i"])
            .arg(&mp4)
            .args(["-c", "copy"])
            .arg(&mkv)
            .status()
            .unwrap()
            .success()
    );
    let mut players = [player(&mp4), player(&mkv)];
    if let demux::Demux::Mp4 { file, .. } = &players[0].demux {
        assert_eq!(file.tracks[players[0].video_track].samples.len(), 172_800);
    }
    for p in &mut players {
        p.fill().unwrap(); // Initial buffering completes before starting either clock.
        p.play();
    }
    let start = Instant::now();
    let mut offsets = [0f64; 2];
    for tick in 0..(7202 * 120) {
        let deadline = start + Duration::from_secs_f64(tick as f64 / 120.0);
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        for (p, max) in players.iter_mut().zip(&mut offsets) {
            if p.next_frame_for_display(Instant::now()).unwrap().is_some() {
                *max = max.max(p.stats().av_offset.abs());
            }
        }
        if players.iter().all(|p| p.state() == State::Ended) {
            break;
        }
    }
    for (p, offset) in players.iter().zip(offsets) {
        eprintln!(
            "2h: {:?}, max offset={:.3}ms, wall={:?}, position={}",
            p.stats(),
            offset * 1000.0,
            start.elapsed(),
            p.position()
        );
        assert_eq!(p.state(), State::Ended);
        assert_eq!(p.stats().displayed, 172_800);
        assert_eq!(p.stats().dropped, 0);
        assert!(offset < 0.020);
    }
    for path in [seed, mp4, mkv] {
        std::fs::remove_file(path).unwrap();
    }
}
