//! `corpus.tsv`: 2,000 paths in real naming styles with the answer known for each (see
//! `gen_corpus.py`). A path is right when title (punctuation and case aside), year, season,
//! episode, last episode and edition all match.

use flicks_metadata::{normalize, parse};

struct Row {
    path: &'static str,
    title: &'static str,
    year: Option<u16>,
    season: Option<u16>,
    episode: Option<u16>,
    end: Option<u16>,
    edition: Option<&'static str>,
}

fn rows() -> Vec<Row> {
    let num = |s: &str| (!s.is_empty()).then(|| s.parse().expect("number in corpus"));
    include_str!("corpus.tsv")
        .lines()
        .map(|line| {
            let c: Vec<&str> = line.split('\t').collect();
            assert_eq!(c.len(), 7, "bad corpus line {line:?}");
            Row {
                path: c[0],
                title: c[1],
                year: num(c[2]),
                season: num(c[3]),
                episode: num(c[4]),
                end: num(c[5]),
                edition: (!c[6].is_empty()).then_some(c[6]),
            }
        })
        .collect()
}

#[test]
fn corpus_is_at_least_98_percent_correct() {
    let rows = rows();
    assert_eq!(rows.len(), 2000);
    let mut wrong = Vec::new();
    for r in &rows {
        let p = parse(r.path);
        let ok = normalize(&p.title) == normalize(r.title)
            && p.year == r.year
            && p.season == r.season
            && p.episode == r.episode
            && p.episode_end == r.end
            && p.edition.as_deref() == r.edition;
        if !ok {
            wrong.push(format!(
                "{}\n    got  {:?} {:?} s{:?} e{:?}-{:?} {:?}\n    want {:?} {:?} s{:?} e{:?}-{:?} {:?}",
                r.path,
                p.title,
                p.year,
                p.season,
                p.episode,
                p.episode_end,
                p.edition,
                r.title,
                r.year,
                r.season,
                r.episode,
                r.end,
                r.edition
            ));
        }
    }
    let right = rows.len() - wrong.len();
    println!("{right}/{} correct ({:.2}%)", rows.len(), right as f64 * 100.0 / rows.len() as f64);
    for w in wrong.iter().take(60) {
        println!("{w}");
    }
    assert!(right * 100 >= rows.len() * 98, "{} wrong of {}", wrong.len(), rows.len());
}

/// Hostile names: every prefix and a byte-flipped copy of every corpus path, plus odd input,
/// parse without panicking.
#[test]
fn parse_survives_truncated_and_mangled_paths() {
    for r in rows() {
        for (i, _) in r.path.char_indices() {
            parse(&r.path[..i]);
        }
        let mut bytes = r.path.as_bytes().to_vec();
        for k in 0..bytes.len() {
            let orig = bytes[k];
            bytes[k] ^= 0x21;
            parse(&String::from_utf8_lossy(&bytes));
            bytes[k] = orig;
        }
    }
    for odd in [
        "",
        "/",
        "\\",
        ".",
        "..",
        "////",
        "[]",
        "(((((",
        ")))))",
        "{edition-",
        "{edition-}",
        "S99999E99999",
        "s1e",
        "1x",
        "x1",
        "- - - -",
        "[a] - 999999999999999",
        "\u{0}\u{ffff}\u{1F600}",
        &"a/".repeat(10_000),
        &"S01E01.".repeat(10_000),
        &"(".repeat(100_000),
    ] {
        parse(odd);
    }
}
