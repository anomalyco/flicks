use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use flicks_container::isobmff::{AvcConfig, Brand, Mp4Writer, SampleEntry, TrackConfig, WriteSample, WriterOptions};
use flicks_container::matroska::{MkvWriter, MuxOptions, TrackKind, TrackSpec};
use flicks_metadata::Tmdb;
use rusqlite::Connection;

use crate::*;

/// A Matroska file with `seconds` one-second keyframes, one audio track and one subtitle track.
fn mkv(seconds: usize) -> Vec<u8> {
    let mut video = TrackSpec::new(TrackKind::Video, "V_MPEG4/ISO/AVC");
    video.video_size = Some((1920, 1080));
    let mut audio = TrackSpec::new(TrackKind::Audio, "A_AAC");
    audio.audio = Some((48000.0, 6, None));
    audio.language = Some("eng".into());
    let mut subs = TrackSpec::new(TrackKind::Subtitle, "S_TEXT/UTF8");
    subs.language = Some("fre".into());
    subs.name = Some("Français".into());
    let mut w = MkvWriter::new(Cursor::new(Vec::new()), vec![video, audio, subs], MuxOptions::default()).unwrap();
    for s in 0..seconds {
        let t = s as i64 * 1_000_000_000;
        w.write_frame(0, t, true, &[s as u8; 100], Some(1_000_000_000)).unwrap();
        w.write_frame(1, t, true, &[1; 20], Some(1_000_000_000)).unwrap();
        w.write_frame(2, t, true, b"hello", Some(1_000_000_000)).unwrap();
    }
    w.finish().unwrap().into_inner()
}

fn mp4(seconds: u32) -> Vec<u8> {
    let avc = AvcConfig::new(vec![vec![0x67, 0x64, 0, 0x28, 1, 2]], vec![vec![0x68, 3]], 4);
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), WriterOptions::new(Brand::Mp4)).unwrap();
    let v = w.add_track(TrackConfig::new(SampleEntry::avc(avc, 1280, 720), 1000)).unwrap();
    let a = w.add_track(TrackConfig::new(SampleEntry::aac(vec![0x11, 0x90], 2, 48000), 48000)).unwrap();
    for s in 0..seconds {
        let sample = |data| WriteSample { data, duration: 1000, composition_offset: 0, is_sync: true };
        w.write_sample(v, sample(&[s as u8; 50])).unwrap();
        w.write_sample(a, WriteSample { data: &[2; 10], duration: 48000, composition_offset: 0, is_sync: true })
            .unwrap();
    }
    w.finish().unwrap().into_inner()
}

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    conn
}

fn scanner() -> Scanner {
    Scanner::new(None, PathBuf::from("unused"))
}

fn count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM library_files", [], |r| r.get(0)).unwrap()
}

#[test]
fn probes_mkv() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("Movie (2010).mkv"), &mkv(5));
    let mut conn = db();
    let report = scanner().scan(&mut conn, dir.path()).unwrap();
    assert_eq!((report.added, report.opened, report.failed), (1, 1, 0));

    let it = &by_root(&conn, &fs::canonicalize(dir.path()).unwrap()).unwrap()[0];
    assert_eq!((it.title.as_str(), it.year), ("Movie", Some(2010)));
    assert_eq!(it.container, "mkv");
    assert_eq!((it.video_codec.as_deref(), it.width, it.height), (Some("h264"), Some(1920), Some(1080)));
    assert_eq!(it.duration_ms, Some(5000));
    assert_eq!(it.oshash.len(), 16);

    let s = streams(&conn, it.id).unwrap();
    assert_eq!(
        (s[0].kind, s[0].codec.as_str(), s[0].channels, s[0].language.as_str()),
        (StreamKind::Audio, "aac", 6, "eng")
    );
    assert_eq!((s[1].kind, s[1].codec.as_str(), s[1].title.as_str()), (StreamKind::Subtitle, "subrip", "Français"));
    let k = keyframes(&conn, it.id).unwrap();
    assert_eq!(k.iter().map(|k| k.time_ms).collect::<Vec<_>>(), [0, 1000, 2000, 3000, 4000]);
    assert!(k.windows(2).all(|w| w[0].offset < w[1].offset));
}

#[test]
fn probes_mp4() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("Clip.mp4"), &mp4(4));
    let mut conn = db();
    scanner().scan(&mut conn, dir.path()).unwrap();
    let it = &recent(&conn, 10).unwrap()[0];
    assert_eq!(it.container, "mp4");
    assert_eq!((it.video_codec.as_deref(), it.width, it.height), (Some("h264"), Some(1280), Some(720)));
    assert_eq!(it.duration_ms, Some(4000));
    let s = streams(&conn, it.id).unwrap();
    assert_eq!((s.len(), s[0].codec.as_str(), s[0].channels), (1, "aac", 2));
    assert_eq!(keyframes(&conn, it.id).unwrap().len(), 4);
}

#[test]
fn oshash_matches_the_published_algorithm() {
    // The size plus the u64 words of a file under 128 KiB, counted twice (first and last chunk
    // overlap completely when the file fits in one chunk).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.bin");
    let bytes: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    fs::write(&path, &bytes).unwrap();
    let words: u64 = bytes.as_chunks::<8>().0.iter().map(|w| u64::from_le_bytes(*w)).fold(0, u64::wrapping_add);
    let want = 4096u64.wrapping_add(words).wrapping_add(words);
    let file = fs::File::open(&path).unwrap();
    assert_eq!(crate::probe::oshash(&file, 4096).unwrap(), want);
}

#[test]
fn unchanged_rescan_opens_no_media_files() {
    let dir = tempfile::tempdir().unwrap();
    for show in 0..3 {
        for ep in 0..4 {
            write(&dir.path().join(format!("Show {show}/Season 01/Show {show} - S01E0{ep}.mkv")), &mkv(2 + ep));
        }
    }
    let mut conn = db();
    let first = scanner().scan(&mut conn, dir.path()).unwrap();
    assert_eq!((first.added, first.opened, first.listed), (12, 12, 7));

    let again = scanner().scan(&mut conn, dir.path()).unwrap();
    assert_eq!(again, ScanReport { listed: 7, ..Default::default() }, "one listing per folder, nothing opened");

    // One rewritten file is the only file opened.
    std::thread::sleep(Duration::from_millis(20));
    write(&dir.path().join("Show 1/Season 01/Show 1 - S01E02.mkv"), &mkv(9));
    let third = scanner().scan(&mut conn, dir.path()).unwrap();
    assert_eq!((third.opened, third.updated, third.added, third.listed), (1, 1, 0, 7));
    assert_eq!(count(&conn), 12);

    // A removed folder goes with its files.
    fs::remove_dir_all(dir.path().join("Show 2")).unwrap();
    let fourth = scanner().scan(&mut conn, dir.path()).unwrap();
    assert_eq!((fourth.removed, fourth.opened), (4, 0));
    assert_eq!(count(&conn), 8);
}

#[test]
fn episodes_are_named_from_their_path() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("Severance (2022)/Season 02/Severance - S02E03.mkv"), &mkv(1));
    let mut conn = db();
    scanner().scan(&mut conn, dir.path()).unwrap();
    let it = &recent(&conn, 1).unwrap()[0];
    assert_eq!((it.title.as_str(), it.season, it.episode, it.tmdb_id), ("Severance", Some(2), Some(3), None));
    assert_eq!(search(&conn, "severa", 10).unwrap().len(), 1);
    assert_eq!(search(&conn, "nothing", 10).unwrap().len(), 0);
}

#[cfg(unix)]
#[test]
fn links_out_of_the_library_are_skipped() {
    let outside = tempfile::tempdir().unwrap();
    write(&outside.path().join("Secret.mkv"), &mkv(1));
    write(&outside.path().join("dir/Hidden.mkv"), &mkv(1));
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("Inside.mkv"), &mkv(1));
    std::os::unix::fs::symlink(outside.path().join("Secret.mkv"), dir.path().join("Link.mkv")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("dir"), dir.path().join("Folder")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("Inside.mkv"), dir.path().join("Alias.mkv")).unwrap();
    let mut conn = db();
    let report = scanner().scan(&mut conn, dir.path()).unwrap();
    let mut names: Vec<String> = recent(&conn, 10)
        .unwrap()
        .iter()
        .map(|i| Path::new(&i.path).file_name().unwrap().to_string_lossy().into())
        .collect();
    names.sort();
    assert_eq!(names, ["Alias.mkv", "Inside.mkv"]);
    assert_eq!(report.opened, 2);

    let root = fs::canonicalize(dir.path()).unwrap();
    assert!(resolve(&root, &dir.path().join("Alias.mkv")).is_some());
    assert!(resolve(&root, &dir.path().join("Link.mkv")).is_none());
}

#[test]
fn an_unreadable_library_folder_keeps_the_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lib");
    write(&root.join("A.mkv"), &mkv(1));
    let mut conn = db();
    scanner().scan(&mut conn, &root).unwrap();
    fs::remove_dir_all(&root).unwrap();
    assert!(scanner().scan(&mut conn, &root).is_err());
    assert_eq!(count(&conn), 1);
}

#[test]
fn a_file_that_is_not_media_is_retried_not_cataloged() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("Broken.mkv"), b"not a matroska file at all");
    write(&dir.path().join("Good.mkv"), &mkv(1));
    let mut conn = db();
    let first = scanner().scan(&mut conn, dir.path()).unwrap();
    assert_eq!((first.added, first.failed), (1, 1));
    // Its folder is looked at again, but the good file isn't reopened.
    let second = scanner().scan(&mut conn, dir.path()).unwrap();
    assert_eq!((second.opened, second.failed, second.added), (1, 1, 0));
    assert_eq!(count(&conn), 1);
}

/// TMDB responses as recorded: one show search, and a JPEG for the poster.
fn tmdb(requests: Arc<AtomicUsize>) -> Tmdb {
    Tmdb::new("key", move |url| {
        requests.fetch_add(1, Ordering::SeqCst);
        if url.contains("image.tmdb.org") {
            return Ok(vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]);
        }
        let body = if url.contains("search/tv") && url.contains("Severance") {
            r#"{"results":[{"id":95396,"name":"Severance","original_name":"Severance","first_air_date":"2022-02-17","poster_path":"/p1.jpg","backdrop_path":"/b1.jpg"}]}"#
        } else {
            r#"{"results":[]}"#
        };
        Ok(body.as_bytes().to_vec())
    })
}

#[test]
fn matches_on_tmdb_once_per_title_and_works_without_a_key() {
    let dir = tempfile::tempdir().unwrap();
    for ep in 1..=3 {
        write(&dir.path().join(format!("Severance (2022)/Season 01/Severance - S01E0{ep}.mkv")), &mkv(1));
    }
    write(&dir.path().join("Home Video 2019.mkv"), &mkv(1));
    let mut conn = db();

    // No key: everything is cataloged under its parsed name and waits for a match.
    scanner().scan(&mut conn, dir.path()).unwrap();
    assert_eq!(count(&conn), 4);
    assert_eq!(pending_count(&conn), 4);

    // A key later: the next scan matches what is waiting, one request per distinct title.
    let requests = Arc::new(AtomicUsize::new(0));
    let art = tempfile::tempdir().unwrap();
    let with_key = Scanner::new(Some(tmdb(requests.clone())), art.path().to_path_buf());
    with_key.scan(&mut conn, dir.path()).unwrap();
    // Two searches for the unknown title (with the year, then without), one for the show,
    // and two images.
    assert_eq!(requests.load(Ordering::SeqCst), 5);
    assert_eq!(pending_count(&conn), 0);
    let shows: Vec<Item> = search(&conn, "severance", 10).unwrap();
    assert_eq!(shows.len(), 3);
    assert!(shows.iter().all(|i| i.tmdb_id == Some(95396) && i.poster.as_deref() == Some("/p1.jpg")));
    assert_eq!(shows.iter().map(|i| i.episode).collect::<Vec<_>>(), [Some(1), Some(2), Some(3)]);
    assert!(art.path().join("w342-p1.jpg").is_file());
    let home = search(&conn, "home video", 10).unwrap();
    assert_eq!((home.len(), home[0].tmdb_id), (1, None));

    // Settled: another scan makes no requests.
    with_key.scan(&mut conn, dir.path()).unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 5);
}

fn pending_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM library_files WHERE matched = 0", [], |r| r.get(0)).unwrap()
}

#[test]
fn a_new_file_appears_within_two_seconds() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("Shows/Existing.mkv"), &mkv(1));
    let roots = vec![dir.path().to_path_buf()];
    let mut conn = db();
    scanner().scan(&mut conn, dir.path()).unwrap();

    let (tx, rx) = mpsc::channel();
    let _watcher = Watcher::start(&roots, move |dirs| {
        let _ = tx.send(dirs);
    })
    .unwrap();
    // FSEvents can replay changes from just before it started: wait them out and drop them, so
    // only what the new file causes can make it appear.
    std::thread::sleep(Duration::from_millis(1000));
    while rx.try_recv().is_ok() {}

    let start = Instant::now();
    write(&dir.path().join("Shows/New Movie (2024).mkv"), &mkv(2));
    let mut seen = false;
    while start.elapsed() < Duration::from_secs(2) && !seen {
        if let Ok(dirs) = rx.recv_timeout(Duration::from_millis(50)) {
            scanner().scan_changed(&mut conn, &roots, &dirs).unwrap();
            seen = !search(&conn, "new movie", 1).unwrap().is_empty();
        }
    }
    let took = start.elapsed();
    println!("new file visible after {took:?}");
    assert!(seen, "not in the catalog after {took:?}");
    assert!(took < Duration::from_secs(2));
}

/// The 10,000-file first scan recorded in docs/perf.md:
/// `cargo test -p flicks-library --release -- --ignored --nocapture first_scan_of_10k_files`
#[test]
#[ignore]
fn first_scan_of_10k_files() {
    let dir = tempfile::tempdir().unwrap();
    let base = mkv(60);
    let t = Instant::now();
    // 40 shows x 5 seasons x 50 episodes. Each file is a real 60-second Matroska file with a
    // different tail, so no two files have the same size or hash.
    for show in 0..40 {
        for season in 1..=5 {
            let folder = dir.path().join(format!("Show {show:02} (2010)/Season {season:02}"));
            fs::create_dir_all(&folder).unwrap();
            for ep in 1..=50 {
                let file = fs::File::create(folder.join(format!("Show {show:02} - S{season:02}E{ep:02}.mkv"))).unwrap();
                std::io::Write::write_all(&mut &file, &base).unwrap();
                // Sparse padding to a movie-sized file: the OSHash reads its last 64 KiB.
                file.set_len(4_000_000_000 + (show * 250 + (season - 1) * 50 + ep) as u64).unwrap();
            }
        }
    }
    println!("generated 10000 files in {:?}", t.elapsed());

    let mut conn = db();
    let t = Instant::now();
    let first = scanner().scan(&mut conn, dir.path()).unwrap();
    println!("first scan: {:?} {first:?}", t.elapsed());
    assert_eq!((first.added, first.failed), (10_000, 0));
    let t = Instant::now();
    let again = scanner().scan(&mut conn, dir.path()).unwrap();
    println!("unchanged rescan: {:?} {again:?}", t.elapsed());
    assert_eq!(again.opened, 0);
}
