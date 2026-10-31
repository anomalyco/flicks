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

// Store whole tracks consecutively to exercise distant DTS-interleaved range reads.
fn separate_tracks(seed: &Path, output: &Path, audio_end: Option<f64>) {
    use flicks_container::isobmff::{self as mp4, Brand, Mp4Writer, TrackConfig, WriteSample, WriterOptions};
    let source = std::fs::File::open(seed).unwrap();
    let file = mp4::open(&source).unwrap();
    let mut writer = Mp4Writer::new(std::fs::File::create(output).unwrap(), WriterOptions::new(Brand::Mp4)).unwrap();
    for track in &file.tracks {
        let mut config = TrackConfig::new(track.entries[0].clone(), track.timescale);
        config.media_start = Some(-track.edit_offset);
        writer.add_track(config).unwrap();
    }
    for (id, track) in file.tracks.iter().enumerate() {
        for (i, sample) in track.samples.iter().enumerate() {
            if track.kind == mp4::TrackKind::Audio
                && audio_end.is_some_and(|end| {
                    end == 0.0 || (sample.pts + track.edit_offset) as f64 / track.timescale as f64 >= end
                })
            {
                break;
            }
            let data = file.read_sample(&source, id, i).unwrap();
            writer
                .write_sample(
                    id,
                    WriteSample {
                        data: &data,
                        duration: sample.duration,
                        composition_offset: (sample.pts - sample.dts) as i32,
                        is_sync: sample.is_sync,
                    },
                )
                .unwrap();
        }
    }
    writer.finish().unwrap().sync_all().unwrap();
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
        } else {
            // A driven clock must let the background decoder finish before advancing
            // media time; real-time tests deliberately do not wait here.
            p.fill().unwrap();
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
fn video_tail_and_empty_audio_reach_ended() {
    let _ff = flicks_testkit::require_ffmpeg!();
    let path = fixture("mp4", "libx264", "aac", "160x96", 1);
    for end in [0.0, 0.791, 0.995] {
        let short = flicks_testkit::temp_path(&path);
        separate_tracks(&path, &short, Some(end));
        let mut decoder = worker::Decoder::open(Source::Local(short.clone())).unwrap();
        // Shorten the audio table, including an enabled track with zero samples.
        decoder.duration = end; // Trim the last decoded AAC frame to the precise audio end.
        let mut p = Player::from_decoder(decoder, AudioSink::Null).unwrap();
        run(&mut p, 1.0, false);
        assert_eq!(p.stats().displayed, 24, "audio end {end}");
        assert_eq!(p.stats().dropped, 0);
        assert!((p.position() - 1.0).abs() < 0.003);
        std::fs::remove_file(short).unwrap();
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn missing_output_uses_null_sink_but_other_errors_propagate() {
    let _ff = flicks_testkit::require_ffmpeg!();
    let path = fixture("mp4", "libx264", "aac", "64x64", 1);
    let mut p = Player::with_output(Source::Local(path.clone()), Err(flicks_audio::Error::NoDevice)).unwrap();
    assert!(matches!(p.sink, AudioSink::Null));
    run(&mut p, 1.0, false);
    assert_eq!(p.stats().displayed, 24);
    assert!(matches!(
        Player::with_output(Source::Local(path.clone()), Err(flicks_audio::Error::Device("test failure".into()))),
        Err(Error::Audio(flicks_audio::Error::Device(_)))
    ));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn seek_recovers_from_a_failed_pending_batch() {
    let _ff = flicks_testkit::require_ffmpeg!();
    let path = fixture("mp4", "libx264", "aac", "64x64", 1);
    let mut p = player(&path);
    // A worker failure not yet observed by the display caller must not prevent
    // replacing the old decode operation with a valid seek.
    p.worker.request(worker::Command::Seek { target: 0.0, audio: Some(999), subtitle: None }).unwrap();
    p.seek(0.0).unwrap();
    run(&mut p, 1.0, false);
    assert_eq!(p.stats().displayed, 24);
    assert_eq!(p.stats().dropped, 0);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn software_codecs_and_audio_layouts() {
    let _ff = flicks_testkit::require_ffmpeg!();
    for (video, audio) in [("libx264", "aac"), ("libx265", "ac3"), ("libvpx-vp9", "libopus"), ("libsvtav1", "eac3")] {
        let path = fixture("mkv", video, audio, "128x96", 1);
        let mut decoder = worker::Decoder::open(Source::Local(path.clone())).unwrap();
        decoder.software = true;
        decoder.restart(0.0).unwrap();
        let mut p = Player::from_decoder(decoder, AudioSink::Null).unwrap();
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
                    p.fill()?;
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
    let mut decoder = worker::Decoder::open(Source::Local(path.clone())).unwrap();
    if decoder.video.hardware() {
        let Codec::Avc { mut avcc } = decoder.tracks[decoder.video_track].codec.clone() else { panic!("avc") };
        avcc[4] &= 0xfc; // A valid configuration with the wrong NAL length field size.
        decoder.video =
            video::Decoder::Hardware(flicks_platform::Decoder::new(flicks_platform::Codec::H264, &avcc).unwrap());
        let mut p = Player::from_decoder(decoder, AudioSink::Null).unwrap();
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
        p.fill().unwrap();
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
    let mut decoder = worker::Decoder::open(Source::Local(path.clone())).unwrap();
    decoder.software = true;
    decoder.restart(0.0).unwrap();
    let mut p = Player::from_decoder(decoder, AudioSink::Null).unwrap();
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
        p.fill().unwrap();
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let _ff = flicks_testkit::require_ffmpeg!();
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let temp = tempfile::tempdir_in(flicks_testkit::fixtures_dir("player")).unwrap();
    let media = temp.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let seed = fixture("mp4", "libx264", "aac", "1920x1080", 16);
    let path = flicks_testkit::temp_path(&seed);
    separate_tracks(&seed, &path, None);
    std::fs::remove_file(seed).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() > 8 * 1024 * 1024, "tracks must be separated beyond the read-ahead window");
    std::fs::rename(&path, media.join("Fixture.2026.mp4")).unwrap();
    // Count TCP/TLS connections without terminating TLS or bypassing signatures.
    // The client uses exactly one HTTP request per connection.
    let connections = Arc::new(AtomicUsize::new(0));
    let blocked = Arc::new(AtomicBool::new(false));
    let stalled = Arc::new(AtomicBool::new(false));
    let fail_once = Arc::new(AtomicBool::new(false));
    let (connection, item, server_key) = runtime.block_on(async {
        let (addr, state) = server::spawn(&temp.path().join("server"), "127.0.0.1:0".parse().unwrap()).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        let (connections, blocked, stalled, fail_once) =
            (connections.clone(), blocked.clone(), stalled.clone(), fail_once.clone());
        tokio::spawn(async move {
            while let Ok((mut incoming, _)) = listener.accept().await {
                connections.fetch_add(1, Ordering::SeqCst);
                if fail_once.swap(false, Ordering::SeqCst) {
                    continue;
                }
                let (blocked, stalled) = (blocked.clone(), stalled.clone());
                tokio::spawn(async move {
                    while blocked.load(Ordering::SeqCst) {
                        stalled.store(true, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    let mut outgoing = tokio::net::TcpStream::connect(addr).await.unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut incoming, &mut outgoing).await;
                });
            }
        });
        let addr = proxy;
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
        (connection, entries[0].item.clone(), server.key)
    });
    let source = || Source::Network { connection: connection.clone(), file: item.id, size: item.size };
    let before = connections.load(Ordering::SeqCst);
    for bad in ["../1/master.m3u8", "/admin/status.m3u8", "https://other/0.m3u8", "0/0.m4s?token=x", "%2e%2e/1.m3u8"] {
        assert!(matches!(runtime.block_on(connection.hls(item.id, bad)), Err(flicks_client::Error::BadAddress)));
    }
    assert_eq!(connections.load(Ordering::SeqCst), before);
    fail_once.store(true, Ordering::SeqCst);
    let master = runtime.block_on(connection.hls(item.id, "master.m3u8")).unwrap().0;
    assert!(master.starts_with(b"#EXTM3U\n"));
    assert_eq!(connections.load(Ordering::SeqCst) - before, 2, "retried signed HLS GET");
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
    let before = connections.load(Ordering::SeqCst);
    let open = Instant::now();
    let mut p = Player::with_audio(source(), AudioSink::Null).unwrap();
    eprintln!("unthrottled network open to decoded preroll: {:?}", open.elapsed());
    assert!(p.stats().adaptive.is_none(), "unthrottled loopback stays on byte ranges");
    let (_, offset) = run(&mut p, 16.0, false);
    let ranges = connections.load(Ordering::SeqCst) - before;
    eprintln!(
        "distant interleaved MP4: {} bytes, 384 frames, {ranges} signed range requests / TLS connections",
        bytes.len()
    );
    assert!(ranges < 100, "range-cache thrash: {ranges}");
    assert_eq!(p.stats().displayed, 384);
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
    fail_once.store(true, Ordering::SeqCst);
    let before = connections.load(Ordering::SeqCst);
    let retry = source::Bytes::open(source()).unwrap();
    let mut first = [0; 16];
    retry.read_at(0, &mut first).unwrap();
    assert_eq!(first, bytes[..16]);
    assert!(connections.load(Ordering::SeqCst) - before >= 2);
    drop(retry);

    for silent in [false, true] {
        let mut p = Player::with_audio(source(), AudioSink::Null).unwrap();
        if silent {
            p.select_audio(None).unwrap();
        }
        blocked.store(true, Ordering::SeqCst);
        stalled.store(false, Ordering::SeqCst);
        p.play();
        let start = Instant::now();
        let mut now = start;
        let mut calls = Vec::new();
        for _ in 0..4800 {
            now += Duration::from_secs_f64(1.0 / 240.0);
            let call = Instant::now();
            p.next_frame_for_display(now).unwrap();
            calls.push(call.elapsed());
            if p.state() == State::Buffering && stalled.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(p.state(), State::Buffering);
        assert!(stalled.load(Ordering::SeqCst), "must stall on real network I/O");
        let at = p.position();
        now += Duration::from_secs(5);
        let call = Instant::now();
        assert!(p.next_frame_for_display(now).unwrap().is_none());
        let stalled_call = call.elapsed();
        assert_eq!(p.position(), at);
        blocked.store(false, Ordering::SeqCst);
        calls.sort();
        eprintln!(
            "network stall silent={silent}: display p99={:?}, max={:?}, stalled call={stalled_call:?}",
            calls[calls.len() * 99 / 100],
            calls.last()
        );
        assert!(stalled_call < Duration::from_millis(1));
        assert!(calls[calls.len() * 99 / 100] < Duration::from_millis(1));
        for _ in 0..4800 {
            p.fill().unwrap();
            now += Duration::from_secs_f64(1.0 / 240.0);
            p.next_frame_for_display(now).unwrap();
            if p.state() == State::Ended {
                break;
            }
        }
        assert_eq!(p.state(), State::Ended);
        assert_eq!(p.stats().dropped, 0, "underrun must re-anchor the clock");
        assert_eq!(p.stats().displayed, 384);
    }
    let downloads = flicks_client::download::Downloads::open(&temp.path().join("client")).unwrap();
    assert!(matches!(source().prefer_download(&downloads, &server_key), Source::Network { .. }));
    runtime.block_on(async {
        downloads
            .start(
                &connection,
                &flicks_client::merge::Source {
                    server: server_key.clone(),
                    server_name: "test".into(),
                    item: item.clone(),
                    progress: None,
                    available: true,
                    latency: None,
                },
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while downloads.local_file(&server_key, item.id).is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    });
    let local = source().prefer_download(&downloads, &server_key);
    assert!(matches!(local, Source::Local(_)));
    let mut offline = Player::with_audio(local, AudioSink::Null).unwrap();
    drop(runtime);
    run(&mut offline, 16.0, false);
    assert_eq!(offline.stats().displayed, 384);
    assert!(offline.stats().adaptive.is_none());
}

#[test]
#[ignore = "two-hour paced playback; generates ~10 GB; takes two hours"]
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
    for path in [&mp4, &mkv] {
        std::fs::File::open(path).unwrap().sync_all().unwrap();
    }
    let decoder = worker::Decoder::open(Source::Local(mp4.clone())).unwrap();
    if let demux::Demux::Mp4 { file, .. } = &decoder.demux {
        assert_eq!(file.tracks[decoder.video_track].samples.len(), 172_800);
    }
    drop(decoder);
    let runs = [mp4.clone(), mkv.clone()].map(|path| std::thread::spawn(move || paced_playback(&path, 7202, |_| {})));
    let results = runs.map(|run| run.join().unwrap());
    assert!(results.iter().all(|(state, _, _)| *state == State::Ended));
    assert!(results.iter().all(|(_, stats, _)| stats.displayed == 172_800 && stats.dropped == 0));
    assert!(results.iter().all(|(_, _, offset)| *offset < 0.020));
    for path in [seed, mp4, mkv] {
        std::fs::remove_file(path).unwrap();
    }
}

// One pacing thread per player in both the short diagnostic and the two-hour acceptance run.
fn paced_playback(path: &Path, seconds: u32, mut sample_call: impl FnMut(Duration)) -> (State, Stats, f64) {
    let mut p = player(path);
    // Model a display-link caller, not a background CLI timer. Open first so the decoder
    // worker doesn't inherit the pacer's scheduling priority.
    #[cfg(target_os = "macos")]
    flicks_platform::set_user_interactive_qos().unwrap();
    let label = path.extension().unwrap_or_default().to_string_lossy();
    p.play();
    let start = Instant::now();
    let (mut previous, mut previous_end) = (start, start);
    let (mut max_gap, mut max_call) = (Duration::ZERO, Duration::ZERO);
    let mut max_offset = 0f64;
    let mut peak = String::new();
    let mut tick = 0;
    let mut skipped_ticks = 0;
    while tick < seconds * 120 {
        // This is what was already decoded *before* the caller went to sleep, not a batch just polled.
        let ready = (p.frames.front().map(|f| f.video.pts()), p.frames.back().map(|f| f.video.pts()));
        let (position, audio_end, dropped) = (p.position(), p.audio_end, p.stats().dropped);
        let deadline = start + Duration::from_secs_f64(tick as f64 / 120.0);
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        let now = Instant::now();
        let pts = p.next_frame_for_display(now).unwrap().map(|frame| frame.pts());
        let end = Instant::now();
        let call = end - now;
        let gap = now - previous;
        max_gap = max_gap.max(gap);
        max_call = max_call.max(call);
        sample_call(call);
        if let Some(pts) = pts {
            let stats = p.stats();
            let delta = stats.dropped - dropped;
            let offset = stats.av_offset.abs();
            if offset > max_offset || delta > 0 {
                let event = format!(
                    "tick={tick}, gap={gap:?} (outside call {:?}), wake late={:?}, call={call:?}, \
                     clock={position:.6}->{:.6}, prequeued PTS={ready:.6?}, audio end={audio_end:.6}, \
                     released={pts:.6}, offset={:.3}ms, drops={delta}",
                    now - previous_end,
                    now.saturating_duration_since(deadline),
                    p.position(),
                    stats.av_offset * 1000.0
                );
                if delta > 0 {
                    eprintln!("{label} drop: {event}");
                }
                if offset > max_offset {
                    max_offset = offset;
                    peak = event;
                }
            }
        }
        previous = now;
        previous_end = end;
        if p.state() == State::Ended {
            break;
        }
        // A display link doesn't replay expired refreshes back-to-back. The poll above
        // used the actual wake time; its offset and all of its drops still count.
        let next = (tick + 1).max((start.elapsed().as_secs_f64() * 120.0) as u32 + 1);
        skipped_ticks += next - (tick + 1);
        tick = next;
    }
    eprintln!(
        "{label}: {:?}, max offset={:.3}ms [{peak}], wall={:?}, position={}, \
         max call={max_call:?}, max gap={max_gap:?}, skipped refresh ticks={skipped_ticks}",
        p.stats(),
        max_offset * 1000.0,
        start.elapsed(),
        p.position()
    );
    (p.state(), p.stats(), max_offset)
}

#[test]
#[ignore = "10min diagnostic using FLICKS_PLAYER_MEDIA and optionally FLICKS_PLAYER_MEDIA_2, no file writes"]
fn scheduling_diagnostic() {
    let first = PathBuf::from(std::env::var_os("FLICKS_PLAYER_MEDIA").unwrap());
    let second = std::env::var_os("FLICKS_PLAYER_MEDIA_2").map(PathBuf::from).unwrap_or_else(|| first.clone());
    let runs = [first, second].map(|path| {
        std::thread::spawn(move || {
            let mut calls = Vec::with_capacity(72_000);
            paced_playback(&path, 600, |call| calls.push(call));
            calls.sort_unstable();
            eprintln!(
                "{}: call p50={:?}, p99={:?}",
                path.display(),
                calls[calls.len() / 2],
                calls[calls.len() * 99 / 100]
            );
        })
    });
    for run in runs {
        run.join().unwrap();
    }
}

#[test]
#[ignore = "plays 3s on the real audio output device"]
fn device_clock_pause_resume() {
    let path = fixture("mp4", "libx264", "aac", "160x96", 3);
    let mut p = Player::open(Source::Local(path.clone())).unwrap();
    p.play();
    let start = Instant::now();
    while p.position() < 0.5 && start.elapsed().as_secs() < 5 {
        p.next_frame_for_display(Instant::now()).unwrap();
        std::thread::sleep(Duration::from_millis(4));
    }
    p.pause().unwrap();
    let at = p.position();
    std::thread::sleep(Duration::from_millis(100));
    p.next_frame_for_display(Instant::now()).unwrap();
    assert_eq!(p.position(), at);
    p.play();
    let mut max_offset = 0f64;
    while p.state() != State::Ended && start.elapsed().as_secs() < 6 {
        if p.next_frame_for_display(Instant::now()).unwrap().is_some() {
            max_offset = max_offset.max(p.stats().av_offset.abs());
        }
        std::thread::sleep(Duration::from_millis(4));
    }
    eprintln!(
        "device clock pause/resume: {:?}, max offset={:.3}ms, end={}",
        p.stats(),
        max_offset * 1000.0,
        p.position()
    );
    assert_eq!(p.state(), State::Ended);
    assert_eq!(p.stats().dropped, 0);
    assert!(max_offset < 0.020);
    std::fs::remove_file(path).unwrap();
}

/// Presentation times (seconds) of an MP4's first video track with its edit offset applied,
/// sorted in presentation order (including any duplicates). Read-only.
#[cfg(target_os = "macos")]
fn presentation_times(path: &Path) -> (Vec<f64>, u32) {
    use flicks_container::isobmff as mp4;
    let file = std::fs::File::open(path).unwrap();
    let mp4 = mp4::open_progressive(&file, 256 << 20, 2_000_000).unwrap();
    let track = mp4.tracks.iter().find(|t| t.kind == mp4::TrackKind::Video).unwrap();
    let mut times: Vec<f64> = track
        .samples
        .iter()
        .map(|s| (s.pts + track.edit_offset) as f64 / track.timescale as f64)
        .filter(|t| *t >= -0.001)
        .collect();
    times.sort_by(f64::total_cmp);
    (times, track.height)
}

/// FLK-47 acceptance: `Source::Network` through the real signed production path to the actual
/// release `flicks serve` (isolated HOME), via a loopback proxy that shapes downstream bytes of
/// every TLS connection together on wall time: 20, 8, 3, 8 Mbps for 35 s each, then 20 Mbps for
/// the last 70 s. Display is wall-clock paced at 120 Hz with no fill() calls. Plays a generated
/// 240 s 3840x2160 HEVC Main10 (closed GOP, IDR every 4 s, ~11 Mbps) and, with
/// FLICKS_PLAYER_MEDIA=<file>, one real read-only MP4 (its folder is added as is). Prints
/// ADAPTIVE_* lines for docs/perf.md.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "binds port 7686, generates ~330 MB of 4K HEVC, ~4 min paced per source; release flicks serve"]
fn adaptive_throttle() {
    use flicks_container::isobmff as mp4;
    use flicks_server::server::{AddFolder, OpenPairing, Pairing, Status};
    use std::collections::HashMap;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    const STEPS: [(f64, u64); 5] =
        [(0.0, 20_000_000), (35.0, 8_000_000), (70.0, 3_000_000), (105.0, 8_000_000), (140.0, 20_000_000)];
    const RUN: f64 = 210.0;
    const STARTUP_MAX: f64 = 3.0;
    const BUFFER_SECONDS_MAX: f64 = 60.0;
    const BUFFER_BYTES_MAX: usize = 128 << 20;
    struct Process(std::process::Child);
    impl Drop for Process {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    /// The shared link: `next` is when it is free again; `start` None means unshaped.
    struct Link {
        start: Option<Instant>,
        next: Instant,
    }
    let rate = |elapsed: f64| STEPS.iter().rev().find(|(at, _)| elapsed >= *at).map_or(STEPS[0].1, |s| s.1);

    let ff = flicks_testkit::require_ffmpeg!();
    let exe = std::env::var_os("FLICKS_SERVER_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| flicks_testkit::workspace_root().join("target/release/flicks"));
    assert!(exe.exists(), "build it first: cargo build --release -p flicks-server, or set FLICKS_SERVER_EXE");
    assert!(std::net::TcpStream::connect("127.0.0.1:7686").is_err(), "port 7686 already in use");

    // 4 s seed, then a stream-copied 240 s loop with continuous audio.
    let synthetic = tempfile::tempdir_in(flicks_testkit::fixtures_dir("player")).unwrap();
    let seed = synthetic.path().join("seed.mp4");
    let generated = synthetic.path().join("Adaptive.2026.mp4");
    let ok = |c: &mut Command| assert!(c.stderr(Stdio::inherit()).status().unwrap().success());
    ok(Command::new(&ff)
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=3840x2160:rate=24,noise=alls=14:allf=t+u"])
        .args(["-t", "4", "-an", "-c:v", "libx265", "-preset", "veryfast", "-pix_fmt", "yuv420p10le", "-tag:v", "hvc1"])
        .arg("-x265-params")
        .arg(concat!(
            "keyint=96:min-keyint=96:scenecut=0:open-gop=0",
            ":bitrate=11000:vbv-maxrate=13000:vbv-bufsize=13000:log-level=error"
        ))
        .arg(&seed));
    ok(Command::new(&ff)
        .args(["-v", "error", "-y", "-stream_loop", "-1", "-i"])
        .arg(&seed)
        // Real CoreAudio device clock without sending programme audio to the desk's speakers.
        .args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo", "-map", "0:v", "-map", "1:a"])
        .args(["-t", "240", "-frames:v", "5760", "-c:v", "copy", "-c:a", "aac", "-ac", "2", "-movflags", "+faststart"])
        .arg(&generated));
    std::fs::remove_file(&seed).unwrap();
    {
        let file = std::fs::File::open(&generated).unwrap();
        let m = mp4::open(&file).unwrap();
        let v = m.tracks.iter().find(|t| t.kind == mp4::TrackKind::Video).unwrap();
        let bytes: u64 = v.samples.iter().map(|s| u64::from(s.size)).sum();
        let mbps = bytes as f64 * 8.0 / (v.samples.len() as f64 / 24.0) / 1e6;
        let sync: Vec<_> = v.samples.iter().enumerate().filter(|(_, s)| s.is_sync).map(|(i, _)| i).collect();
        eprintln!(
            "ADAPTIVE_FIXTURE 3840x2160 HEVC Main10 frames={} avg={mbps:.2}Mbps idr={}",
            v.samples.len(),
            sync.len()
        );
        assert_eq!((v.width, v.height, v.samples.len()), (3840, 2160, 5760));
        assert!((10.0..=12.5).contains(&mbps), "average video bitrate {mbps:.2} Mbps");
        assert!(sync.iter().enumerate().all(|(n, &i)| i == n * 96) && sync.len() == 60, "IDR every 4 s");
    }
    let mut sources = vec![("generated-4k", generated.clone())];
    sources.extend(std::env::var_os("FLICKS_PLAYER_MEDIA").map(|p| ("real", PathBuf::from(p))));

    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    let home = tempfile::tempdir().unwrap();
    let _server = Process(
        Command::new(&exe)
            .arg("serve")
            .env("HOME", home.path())
            .env_remove("FLICKS_TMDB_KEY")
            .stdout(Stdio::from(std::fs::File::create(home.path().join("serve.out")).unwrap()))
            .stderr(Stdio::from(std::fs::File::create(home.path().join("serve.err")).unwrap()))
            .spawn()
            .unwrap(),
    );
    let link = Arc::new(Mutex::new(Link { start: None, next: Instant::now() }));
    let proxied = Arc::new(AtomicU64::new(0));
    let (connection, items) = runtime.block_on(async {
        let data = home.path().join("Library/Application Support/Flicks");
        let root = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(root) = flicks_server::cli::local(&data)
                    && root.call::<Status>(hyper::Method::GET, "/admin/status", None::<&()>).await.is_ok()
                {
                    break root;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        let (link, proxied) = (link.clone(), proxied.clone());
        tokio::spawn(async move {
            while let Ok((incoming, _)) = listener.accept().await {
                let (link, proxied) = (link.clone(), proxied.clone());
                tokio::spawn(async move {
                    let Ok(outgoing) = tokio::net::TcpStream::connect(("127.0.0.1", 7686)).await else { return };
                    let ((mut client_in, mut client_out), (mut server_in, mut server_out)) =
                        (incoming.into_split(), outgoing.into_split());
                    let up = tokio::spawn(async move {
                        let _ = tokio::io::copy(&mut client_in, &mut server_out).await;
                        let _ = server_out.shutdown().await;
                    });
                    let mut buf = vec![0; 8192];
                    while let Ok(n @ 1..) = server_in.read(&mut buf).await {
                        let done = {
                            let mut l = link.lock().unwrap();
                            match l.start {
                                None => None,
                                Some(start) => {
                                    // Carry a few milliseconds of scheduler lateness, not unlimited
                                    // idle credit. Rounding every 8 KiB sleep up would shape a
                                    // 20 Mbps link to ~15 Mbps even on an otherwise idle Mac.
                                    let at = l.next.max(Instant::now() - Duration::from_millis(4));
                                    let bps = rate(at.saturating_duration_since(start).as_secs_f64());
                                    l.next = at + Duration::from_secs_f64(n as f64 * 8.0 / bps as f64);
                                    Some(l.next)
                                }
                            }
                        };
                        if let Some(done) = done {
                            tokio::time::sleep_until(tokio::time::Instant::from_std(done)).await;
                        }
                        proxied.fetch_add(n as u64, Ordering::Relaxed);
                        if client_out.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                    let _ = client_out.shutdown().await;
                    up.abort();
                });
            }
        });
        let mut folders = vec![synthetic.path().to_path_buf()];
        folders.extend(sources.get(1).and_then(|(_, p)| p.parent()).map(Path::to_path_buf));
        for folder in folders {
            let _: flicks_server::db::Folder = root
                .call(
                    hyper::Method::POST,
                    "/admin/folders",
                    Some(&AddFolder { path: folder.to_string_lossy().into_owned() }),
                )
                .await
                .unwrap();
        }
        let pairing: Pairing = root
            .call(
                hyper::Method::POST,
                "/admin/pair",
                Some(&OpenPairing { user: "player".into(), admin: false, addrs: vec![proxy.to_string()] }),
            )
            .await
            .unwrap();
        let mut client = flicks_client::Client::open(&home.path().join("client")).unwrap();
        let server = client.pair(&pairing.link, "Adaptive Player").await.unwrap();
        // The link also lists the server's own addresses; every byte must cross the proxy.
        let connection = Arc::new(flicks_client::Connection {
            addr: proxy.to_string(),
            ..client.connect(&server.key).await.unwrap()
        });
        let sizes: Vec<u64> = sources.iter().map(|(_, p)| std::fs::metadata(p).unwrap().len()).collect();
        let items: Vec<(i64, u64)> = tokio::time::timeout(Duration::from_secs(180), async {
            loop {
                let entries = connection.items().await.unwrap();
                let found: Vec<_> = sizes
                    .iter()
                    .filter_map(|size| entries.iter().find(|e| e.item.size == *size).map(|e| (e.item.id, e.item.size)))
                    .collect();
                if found.len() == sizes.len() {
                    break found;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("sources catalogued");
        (connection, items)
    });

    for ((label, path), (file, size)) in sources.iter().zip(items) {
        let (expected, source_height) = presentation_times(path);
        let height = |tier: Tier| match tier {
            Tier::Direct => source_height,
            Tier::P1080 => 1080,
            Tier::P720 => 720,
            Tier::P480 => 480,
        };
        let rss_stop = Arc::new(AtomicBool::new(false));
        let rss_peak = Arc::new(AtomicU64::new(0));
        let rss = {
            let (stop, peak) = (rss_stop.clone(), rss_peak.clone());
            std::thread::spawn(move || {
                let pid = std::process::id().to_string();
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(out) = Command::new("ps").args(["-o", "rss=", "-p", &pid]).output()
                        && let Ok(kib) = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>()
                    {
                        peak.fetch_max(kib, Ordering::Relaxed);
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            })
        };
        let bytes_before = proxied.load(Ordering::Relaxed);
        let open = Instant::now();
        {
            let mut l = link.lock().unwrap();
            l.start = Some(open);
            l.next = open;
        }
        let sink = if *label == "generated-4k" {
            AudioSink::Device(Box::new(flicks_audio::Output::open().expect("CoreAudio device for paced acceptance")))
        } else {
            // Decode the real episode's entire audio track, without playing it into the room.
            AudioSink::Null
        };
        let mut p = Player::with_audio(Source::Network { connection: connection.clone(), file, size }, sink).unwrap();
        p.play();
        let mut shown: Vec<f64> = Vec::new();
        let mut startup = None;
        let (mut rebuffers, mut rebuffer_time, mut buffering_since) = (0u32, 0f64, None::<Instant>);
        let mut switches: Vec<(f64, f64, Tier, Tier)> = Vec::new();
        let mut per_tier: HashMap<Tier, f64> = HashMap::new();
        let mut last: Option<(f64, Tier)> = None;
        let (mut max_av, mut max_seconds, mut max_bytes, mut wrong_size) = (0f64, 0f64, 0usize, 0u32);
        let mut max_call = Duration::ZERO;
        let mut max_switch_call = Duration::ZERO;
        let mut max_switch_av = 0f64;
        let mut phase_ends = Vec::new();
        for tick in 0.. {
            let deadline = open + Duration::from_secs_f64(tick as f64 / 120.0);
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            let now = Instant::now();
            let wall = now.duration_since(open).as_secs_f64();
            if wall >= RUN || p.state() == State::Ended {
                break;
            }
            let frame = p.next_frame_for_display(now).unwrap();
            let call = now.elapsed();
            let returned = wall + call.as_secs_f64();
            max_call = max_call.max(call);
            let adaptive = p.stats().adaptive.expect("network playback reports adaptive stats");
            max_seconds = max_seconds.max(adaptive.buffer_seconds);
            max_bytes = max_bytes.max(adaptive.buffer_bytes);
            if let Some(frame) = frame {
                startup.get_or_insert(returned);
                let (pts, tier) = (frame.pts(), adaptive.tier);
                wrong_size += u32::from(frame.size()[1] != height(tier));
                max_av = max_av.max(p.stats().av_offset.abs());
                if let Some((previous, from)) = last {
                    *per_tier.entry(from).or_default() += pts - previous;
                    if from != tier {
                        switches.push((returned, pts, from, tier));
                        max_switch_call = max_switch_call.max(call);
                        max_switch_av = max_switch_av.max(p.stats().av_offset.abs());
                    }
                }
                last = Some((pts, tier));
                shown.push(pts);
            }
            let buffering = startup.is_some() && p.state() == State::Buffering;
            match (buffering, buffering_since) {
                (true, None) => {
                    rebuffers += 1;
                    buffering_since = Some(now);
                }
                (false, Some(since)) => {
                    rebuffer_time += now.duration_since(since).as_secs_f64();
                    buffering_since = None;
                }
                _ => {}
            }
            if phase_ends.len() < STEPS.len() && wall >= STEPS.get(phase_ends.len() + 1).map_or(RUN, |s| s.0) - 0.5 {
                phase_ends.push((wall, adaptive.tier, adaptive.throughput_bps, adaptive.buffer_seconds));
            }
        }
        rss_stop.store(true, Ordering::Relaxed);
        rss.join().unwrap();
        let stats = p.stats();
        let downloaded = proxied.load(Ordering::Relaxed) - bytes_before;
        eprintln!(
            "ADAPTIVE {label}: startup={:.3}s rebuffers={rebuffers} ({rebuffer_time:.3}s) switches={} frames={} \
             dropped={} max_av={:.3}ms peak_buffer={max_seconds:.1}s/{:.1}MiB peak_rss={:.0}MiB \
             max_call={max_call:?} switch_call={max_switch_call:?} switch_av={:.3}ms wrong_size={wrong_size} proxied={:.1}MB",
            startup.unwrap_or(f64::NAN),
            switches.len(),
            shown.len(),
            stats.dropped,
            max_av * 1000.0,
            max_bytes as f64 / 1048576.0,
            rss_peak.load(Ordering::Relaxed) as f64 / 1024.0,
            max_switch_av * 1000.0,
            downloaded as f64 / 1e6
        );
        eprintln!("ADAPTIVE {label} media seconds per tier: {per_tier:?}");
        for (wall, pts, from, to) in &switches {
            eprintln!("ADAPTIVE {label} switch wall={wall:.1}s pts={pts:.3}s {from:?} -> {to:?}");
        }
        for ((at, bps), (wall, tier, throughput, buffer)) in STEPS.iter().zip(&phase_ends) {
            eprintln!(
                "ADAPTIVE {label} step {at:.0}s@{:.0}Mbps end wall={wall:.1}s shown={tier:?} \
                 throughput={:.2}Mbps buffer={buffer:.1}s",
                *bps as f64 / 1e6,
                *throughput as f64 / 1e6
            );
        }

        // Every presented frame is the next source frame: none missing or repeated across switches.
        let frame = expected.get(1).zip(expected.first()).map_or(1.0 / 24.0, |(b, a)| b - a);
        let tolerance = (frame / 4.0).min(0.002);
        assert!(last.is_some_and(|(pts, _)| pts > RUN - STARTUP_MAX - 1.0), "{label}: only {} frames", shown.len());
        for (i, pts) in shown.iter().enumerate() {
            let want = expected.get(i).copied().unwrap_or(f64::NAN);
            assert!(
                (pts - want).abs() <= tolerance,
                "{label}: presented frame {i} pts={pts:.4}s, source frame {i} is {want:.4}s (missing or duplicate)"
            );
        }
        assert!(startup.is_some_and(|s| s < STARTUP_MAX), "{label}: startup {startup:?}");
        assert_eq!((rebuffers, stats.dropped), (0, 0), "{label}: rebuffers and dropped frames");
        assert!(max_av < 0.020, "{label}: A/V {max_av}");
        assert_eq!(wrong_size, 0, "{label}: frames whose size doesn't match the reported tier");
        assert!(max_seconds <= BUFFER_SECONDS_MAX && max_bytes <= BUFFER_BYTES_MAX, "{label}: buffer");
        assert!(
            switches.iter().any(|&(wall, _, _, to)| (70.0..150.0).contains(&wall) && to == Tier::P480),
            "{label}: no low-tier response to the 3 Mbps phase"
        );
        let (wall, _, _, to) = *switches.last().expect("switches");
        assert!(to == Tier::Direct && wall < RUN - 10.0, "{label}: no return to full quality ({to:?} at {wall:.1}s)");

        // A seek on the same adaptive network player, still on the 20 Mbps link.
        let target = 37.5;
        let start = Instant::now();
        p.pause().unwrap();
        p.seek(target).unwrap();
        let frame = p.next_frame_for_display(Instant::now()).unwrap().expect("frame after seek");
        let elapsed = start.elapsed();
        let tier = p.stats().adaptive.expect("adaptive").tier;
        eprintln!("ADAPTIVE {label} seek {target}s -> {:.3}s {tier:?}: {elapsed:?}", frame.pts());
        assert!((target - 0.001..target + 0.25).contains(&frame.pts()), "{label}: seek landed at {}", frame.pts());
        assert_eq!(frame.size()[1], height(tier), "{label}: seek frame size vs tier");
        assert!(elapsed < Duration::from_secs(2), "{label}: seek took {elapsed:?}");
        drop(p);
        link.lock().unwrap().start = None;
    }
}
