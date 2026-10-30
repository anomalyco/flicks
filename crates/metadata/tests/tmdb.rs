//! TMDB matching against canned responses (`tmdb.json`, see `gen_corpus.py`; the live API is
//! never called) and the artwork cache.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use flicks_metadata::{ArtworkKind, Error, Media, Tmdb, cache_artwork, normalize, parse};

/// The canned answer for a search URL, as "movie|<normalized query>|<year or empty>".
fn replay() -> Tmdb {
    let canned: HashMap<String, serde_json::Value> =
        serde_json::from_str(include_str!("tmdb.json")).expect("tmdb.json");
    Tmdb::new("test-key", move |url| {
        let rest = url.strip_prefix("https://api.themoviedb.org/3/search/").ok_or(Error::Http("404".into()))?;
        let (kind, query) = rest.split_once('?').ok_or(Error::Http("404".into()))?;
        let kind = if kind == "movie" { "movie" } else { "tv" };
        let (mut text, mut year) = (String::new(), "");
        for pair in query.split('&') {
            match pair.split_once('=') {
                Some(("query", v)) => text = decode(v),
                Some(("year" | "first_air_date_year", v)) => year = v,
                _ => {}
            }
        }
        let body = canned.get(&format!("{kind}|{}|{year}", normalize(&text))).ok_or(Error::Http("404".into()))?;
        Ok(serde_json::to_vec(body).expect("json"))
    })
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).expect("hex"));
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).expect("utf8")
}

#[test]
fn library_of_200_matches_at_least_95_percent() {
    let tmdb = replay();
    let lines: Vec<&str> = include_str!("library.tsv").lines().collect();
    assert_eq!(lines.len(), 200);
    let mut wrong = Vec::new();
    for line in &lines {
        let c: Vec<&str> = line.split('\t').collect();
        let want: u64 = c[1].parse().expect("id");
        let parsed = parse(c[0]);
        let got = tmdb.find(&parsed).expect("canned response").map(|m| m.id).unwrap_or(0);
        if got != want || (c[2] == "tv") != parsed.is_episode() {
            wrong.push(format!("{} -> {parsed:?}: got {got}, want {want}", c[0]));
        }
    }
    let right = lines.len() - wrong.len();
    println!("{right}/200 matched ({:.1}%)", right as f64 / 2.0);
    for w in &wrong {
        println!("{w}");
    }
    assert!(right * 100 >= lines.len() * 95, "{} of 200 wrong", wrong.len());
}

#[test]
fn a_year_picks_between_same_titled_entries() {
    let tmdb = replay();
    let line = include_str!("library.tsv").lines().find(|l| l.contains("Dune - Part Two")).expect("Dune");
    let m = tmdb.find(&parse(line.split('\t').next().expect("path"))).expect("ok").expect("match");
    assert_eq!((m.media, m.year), (Media::Movie, Some(2024)));
    assert!(m.poster.is_some() && m.backdrop.is_some());
}

#[test]
fn an_unknown_title_matches_nothing() {
    let none = Tmdb::new("k", |_| Ok(br#"{"page":1,"results":[],"total_pages":1,"total_results":0}"#.to_vec()));
    assert_eq!(none.find(&parse("Movies/Home Video (2011)/Home Video (2011).mkv")).expect("ok"), None);
}

#[test]
fn no_key_means_no_client() {
    if std::env::var_os("FLICKS_TMDB_KEY").is_none() {
        assert!(Tmdb::from_env().is_none());
    }
}

#[test]
fn unusable_responses_are_errors_not_panics() {
    let p = parse("Movies/Inception (2010)/Inception (2010).mkv");
    for body in ["", "not json", "[]", "{}", r#"{"results":"x"}"#, r#"{"results":[{"id":"x","title":1}]}"#] {
        let tmdb = Tmdb::new("k", move |_| Ok(body.as_bytes().to_vec()));
        let _ = tmdb.find(&p);
    }
    let down = Tmdb::new("k", |_| Err(Error::Http("down".into())));
    assert!(down.find(&p).is_err());
    assert!(down.find(&parse("")).expect("nothing to look up").is_none());
}

fn jpeg(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 90]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Jpeg).expect("encode");
    out.into_inner()
}

#[test]
fn artwork_is_downloaded_once_and_stored_as_webp() {
    let fetches = Arc::new(AtomicUsize::new(0));
    let count = fetches.clone();
    let poster = jpeg(342, 513);
    let tmdb = Tmdb::new("k", move |url| {
        assert_eq!(url, "https://image.tmdb.org/t/p/w342/poster100001.jpg");
        count.fetch_add(1, Ordering::SeqCst);
        Ok(poster.clone())
    });
    let dir = tempfile::tempdir().expect("tempdir");
    let file = cache_artwork(&tmdb, dir.path(), ArtworkKind::Poster, "/poster100001.jpg").expect("cached");
    let again = cache_artwork(&tmdb, dir.path(), ArtworkKind::Poster, "/poster100001.jpg").expect("cached");
    assert_eq!(file, again);
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    let bytes = std::fs::read(&file).expect("read");
    assert!(bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()));
    let decoded = webp::Decoder::new(&bytes).decode().expect("webp decodes");
    assert_eq!((decoded.width(), decoded.height()), (342, 513));
    assert!(bytes.len() < jpeg(342, 513).len() * 2, "{} bytes", bytes.len());
}

#[test]
fn artwork_refuses_bad_paths_and_images() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tmdb = Tmdb::new("k", |_| Ok(b"not a jpeg".to_vec()));
    for bad in ["", "/", "/../x.jpg", "/a/b.jpg", "a.jpg", "/a.png", "/a b.jpg", "/..%2f.jpg"] {
        assert!(cache_artwork(&tmdb, dir.path(), ArtworkKind::Poster, bad).is_err(), "{bad}");
    }
    assert!(cache_artwork(&tmdb, dir.path(), ArtworkKind::Poster, "/ok.jpg").is_err());
    assert!(std::fs::read_dir(dir.path()).expect("dir").next().is_none(), "nothing written");
}
