//! Made-up SRT and WebVTT samples in `fixtures/` with the answer known for each, container
//! payloads, fuzzing (truncation, bit flips, junk: nothing may panic) and the lookup measurement.

use flicks_subtitle::{Cue, Error, MAX_INPUT, Span, Track, parse_srt, parse_webvtt};
use std::time::Instant;

const SRT: &str = include_str!("fixtures/basic.srt");
const VTT: &str = include_str!("fixtures/basic.vtt");

fn plain(t: &Track) -> Vec<(u64, u64, String)> {
    t.cues().iter().map(|c| (c.start_ms, c.end_ms, c.text())).collect()
}

fn span(text: &str, italic: bool, bold: bool, underline: bool) -> Span {
    Span { text: text.into(), italic, bold, underline }
}

fn expected_srt() -> Vec<(u64, u64, String)> {
    let cue = |s, e, t: &str| (s, e, t.to_string());
    vec![
        cue(1000, 3500, "Good morning, everyone."),
        cue(4000, 6000, "The kettle is on\nand the toast is burning."),
        cue(5500, 8000, "Overlaps the cue above."),
        cue(12000, 14000, "Seven o'clock."),
        cue(14500, 15000, "No blank line before this cue's index."),
        cue(20000, 21000, "42"),
        cue(3_723_004, 3_724_500, "Tom & Jerry <3"),
    ]
}

fn expected_vtt() -> Vec<(u64, u64, String)> {
    let cue = |s, e, t: &str| (s, e, t.to_string());
    vec![
        cue(1000, 3500, "Hello there."),
        cue(4000, 6000, "Soft and loud\nsecond line coloured"),
        cue(6000, 7000, "Overlapping start"),
        cue(6500, 8000, "No blank line before this cue."),
        cue(300_000, 302_000, "Five minutes in, with a timestamp tag."),
        cue(3_600_000, 3_601_000, "Hour form & entity\u{a0}ok."),
    ]
}

#[test]
fn srt_sample_cues() {
    let t = parse_srt(SRT.as_bytes()).unwrap();
    assert_eq!(plain(&t), expected_srt());
}

#[test]
fn srt_sample_styles() {
    let t = parse_srt(SRT.as_bytes()).unwrap();
    let c = &t.cues()[1];
    assert_eq!(c.lines[0], vec![span("The kettle is on", true, false, false)]);
    assert_eq!(
        c.lines[1],
        vec![
            span("and ", false, false, false),
            span("the", false, true, false),
            span(" ", false, false, false),
            span("toast", false, false, true),
            span(" is burning.", false, false, false),
        ]
    );
}

#[test]
fn webvtt_sample_cues_and_styles() {
    let t = parse_webvtt(VTT.as_bytes()).unwrap();
    assert_eq!(plain(&t), expected_vtt());
    assert_eq!(t.cues()[1].lines[0], vec![span("Soft ", true, false, false), span("and loud", true, true, false)]);
}

#[test]
fn crlf_bom_cr_and_utf16_read_the_same() {
    let crlf = SRT.replace('\n', "\r\n");
    let cr = SRT.replace('\n', "\r");
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice(crlf.as_bytes());
    let mut le = vec![0xFF, 0xFE];
    let mut be = vec![0xFE, 0xFF];
    for u in SRT.encode_utf16() {
        le.extend_from_slice(&u.to_le_bytes());
        be.extend_from_slice(&u.to_be_bytes());
    }
    for input in [crlf.as_bytes(), cr.as_bytes(), &bom, &le, &be] {
        assert_eq!(plain(&parse_srt(input).unwrap()), expected_srt());
    }
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice(VTT.replace('\n', "\r\n").as_bytes());
    assert_eq!(plain(&parse_webvtt(&bom).unwrap()), expected_vtt());
}

#[test]
fn webvtt_needs_its_header() {
    assert_eq!(parse_webvtt(SRT.as_bytes()).unwrap_err(), Error::NotWebVtt);
    assert_eq!(parse_webvtt(b"WEBVTTX\n\n").unwrap_err(), Error::NotWebVtt);
    assert!(parse_webvtt(b"WEBVTT").unwrap().cues().is_empty());
}

#[test]
fn oversize_input_is_refused() {
    let big = vec![b'a'; MAX_INPUT + 1];
    assert_eq!(parse_srt(&big).unwrap_err(), Error::TooLarge);
    assert_eq!(parse_webvtt(&big).unwrap_err(), Error::TooLarge);
    assert!(parse_srt(&big[..MAX_INPUT]).unwrap().cues().is_empty());
}

#[test]
fn active_cues_at_the_right_times() {
    let t = parse_srt(SRT.as_bytes()).unwrap();
    let at = |ms| t.active_at(ms).iter().map(|c| c.start_ms).collect::<Vec<_>>();
    assert_eq!(at(0), Vec::<u64>::new());
    assert_eq!(at(999), Vec::<u64>::new());
    assert_eq!(at(1000), [1000]);
    assert_eq!(at(3499), [1000]);
    assert_eq!(at(3500), Vec::<u64>::new()); // end is exclusive
    assert_eq!(at(5000), [4000]);
    assert_eq!(at(5500), [4000, 5500]); // overlap: both, in start order
    assert_eq!(at(5999), [4000, 5500]);
    assert_eq!(at(6000), [5500]);
    assert_eq!(at(8000), Vec::<u64>::new());
    assert_eq!(at(3_723_004), [3_723_004]);
    assert_eq!(at(u64::MAX), Vec::<u64>::new());
}

#[test]
fn a_long_cue_stays_active_past_many_short_ones() {
    let cue = |s, e| Cue::from_payload(s, e, b"x").unwrap();
    let t = Track::new(vec![cue(0, 10_000), cue(10, 20), cue(30, 40), cue(50, 60)]);
    let at = |ms| t.active_at(ms).iter().map(|c| c.start_ms).collect::<Vec<_>>();
    assert_eq!(at(15), [0, 10]);
    assert_eq!(at(25), [0]);
    assert_eq!(at(9_999), [0]);
    assert_eq!(at(10_000), Vec::<u64>::new());
}

#[test]
fn container_payloads() {
    let c = Cue::from_payload(1000, 2500, b"<i>Hi</i>\r\nthere &amp; <b>you").unwrap();
    assert_eq!((c.start_ms, c.end_ms), (1000, 2500));
    assert_eq!(c.text(), "Hi\nthere & you");
    assert!(c.lines[0][0].italic);
    assert!(c.lines[1][1].bold);
    assert!(Cue::from_payload(1000, 1000, b"x").is_none());
    assert!(Cue::from_payload(2000, 1000, b"x").is_none());
    assert!(Cue::from_payload(0, 1000, b"").is_none());
    assert!(Cue::from_payload(0, 1000, b"<c.x></c>{\\an8}").is_none());
    // A WebVTT block's text can hold voice, class and timestamp tags.
    let c = Cue::from_payload(0, 1000, b"<v Bob>Hello <00:00.500>again</v>").unwrap();
    assert_eq!(c.text(), "Hello again");
}

#[test]
fn markup_edge_cases() {
    let text = |s: &str| Cue::from_payload(0, 1, s.as_bytes()).map(|c| c.text());
    assert_eq!(text("a < b and c > d").as_deref(), Some("a < b and c > d"));
    assert_eq!(text("1 <3 2").as_deref(), Some("1 <3 2"));
    assert_eq!(text("</i></i>x").as_deref(), Some("x"));
    assert_eq!(text("&#65;&#x42;&bogus;&#xD800;").as_deref(), Some("AB&bogus;&#xD800;"));
    assert_eq!(text("a\u{0}\u{1b}b\tc").as_deref(), Some("ab c"));
    assert_eq!(text("{\\an8").as_deref(), Some("{\\an8"));
    assert_eq!(text("<i>unclosed").as_deref(), Some("unclosed"));
    assert_eq!(text("ünï <b>çödé</b> 日本語").as_deref(), Some("ünï çödé 日本語"));
}

// ---- fuzzing ----

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Everything a parser returns must satisfy the invariants `active_at` relies on, and every
/// entry point must return rather than panic.
fn exercise(data: &[u8], rng: &mut Rng) {
    for track in [parse_srt(data), parse_webvtt(data)].into_iter().flatten() {
        let cues = track.cues();
        assert!(cues.windows(2).all(|w| w[0].start_ms <= w[1].start_ms));
        for c in cues {
            assert!(c.end_ms > c.start_ms);
            assert!(!c.lines.is_empty() && c.lines.iter().all(|l| !l.is_empty()));
        }
        for t in [0, 1000, 5500, 3_723_004, rng.next() % 4_000_000, u64::MAX] {
            let active = track.active_at(t);
            let want = cues.iter().filter(|c| c.start_ms <= t && t < c.end_ms).count();
            assert_eq!(active.len(), want);
        }
    }
    let _ = Cue::from_payload(rng.next() % 1000, rng.next() % 1000, data);
}

#[test]
fn fuzz_truncation_bit_flips_and_junk() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut seeds: Vec<Vec<u8>> = vec![SRT.as_bytes().to_vec(), VTT.as_bytes().to_vec()];
    seeds.push(SRT.replace('\n', "\r\n").into_bytes());
    seeds.push([&[0xFF, 0xFE][..], &SRT.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>()].concat());
    for seed in &seeds {
        // Every truncation.
        for n in 0..=seed.len() {
            exercise(&seed[..n], &mut rng);
        }
        // Bit flips and byte overwrites.
        for _ in 0..1500 {
            let mut d = seed.clone();
            for _ in 0..=rng.below(8) {
                let i = rng.below(d.len());
                match rng.below(3) {
                    0 => d[i] ^= 1 << rng.below(8),
                    1 => d[i] = rng.next() as u8,
                    _ => {
                        d.remove(i);
                    }
                }
            }
            exercise(&d, &mut rng);
        }
    }
    // Junk, and junk built from the tokens the parsers look for.
    let tokens: [&str; 16] = [
        "-->",
        "\n",
        "\r\n",
        "00:00:01,000",
        "00:01.5",
        "<i>",
        "</b>",
        "<",
        ">",
        "&amp;",
        "&#",
        "{\\an",
        "}",
        "WEBVTT",
        "NOTE",
        "99999999999999999999:",
    ];
    for _ in 0..3000 {
        let mut d = Vec::new();
        for _ in 0..rng.below(60) {
            if rng.below(4) == 0 {
                d.push(rng.next() as u8);
            } else {
                d.extend_from_slice(tokens[rng.below(tokens.len())].as_bytes());
            }
        }
        exercise(&d, &mut rng);
        let mut v = b"WEBVTT\n\n".to_vec();
        v.extend_from_slice(&d);
        exercise(&v, &mut rng);
    }
    for len in [0, 1, 2, 3, 1000] {
        let d: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        exercise(&d, &mut rng);
    }
}

#[test]
fn hostile_shapes_stay_bounded() {
    let mut rng = Rng(7);
    // Deep tag nesting, a million angle brackets, one cue with a huge number of lines, and more
    // cues than the cap.
    let nested = format!("00:00:00,000 --> 00:00:01,000\n{}x\n", "<i><b><u>".repeat(100_000));
    exercise(nested.as_bytes(), &mut rng);
    exercise("<".repeat(1_000_000).as_bytes(), &mut rng);
    exercise(format!("00:00:00,000 --> 00:00:01,000\n{}", "line\n".repeat(500_000)).as_bytes(), &mut rng);
    let t = parse_srt(format!("00:00:00,000 --> 00:00:01,000\n{}", "line\n".repeat(500_000)).as_bytes()).unwrap();
    assert!(t.cues()[0].lines.len() <= 16);
    let many: String = (0..150_000).map(|i| format!("00:00:{:02},000 --> 00:00:59,999\nx\n\n", i % 59)).collect();
    assert_eq!(parse_srt(many.as_bytes()).unwrap().cues().len(), flicks_subtitle::MAX_CUES);
}

// ---- lookup cost ----

fn two_hour_srt(cues: usize) -> String {
    let ts = |ms: u64| format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    let step = 7_200_000 / cues as u64;
    let mut out = String::new();
    for i in 0..cues as u64 {
        let start = i * step;
        out += &format!(
            "{}\n{} --> {}\n<i>Line {i} of made-up dialogue.</i>\nAnd a second line here.\n\n",
            i + 1,
            ts(start),
            ts(start + step * 3 / 4)
        );
    }
    out
}

/// Prints parse time and `active_at` cost for a 2-hour file queried at every 24 fps frame.
/// `cargo test -p flicks-subtitle --release -- --ignored --nocapture lookup_cost`
#[test]
#[ignore]
fn lookup_cost() {
    let report = |name: &str, t: &Track| {
        let frames = 7_200_000 / 42; // every ~42 ms
        let (mut hits, mut worst) = (0usize, 0u128);
        let start = Instant::now();
        for f in 0..frames {
            let one = Instant::now();
            hits += t.active_at(f * 42).len();
            worst = worst.max(one.elapsed().as_nanos());
        }
        let total = start.elapsed();
        println!(
            "{name}: {} cues, {frames} lookups in {total:?} = {} ns each, worst {worst} ns, {hits} hits",
            t.cues().len(),
            total.as_nanos() / frames as u128
        );
    };
    for cues in [1_500, 2_500, 10_000] {
        let text = two_hour_srt(cues);
        let start = Instant::now();
        let t = parse_srt(text.as_bytes()).unwrap();
        println!("parse {cues} cues ({} KiB): {:?}", text.len() / 1024, start.elapsed());
        report(&format!("{cues}-cue file"), &t);
    }
    // Worst case for the lookup: one cue that lasts the whole film in front of 100,000 short ones.
    let mut cues: Vec<Cue> =
        (0..99_999).map(|i| Cue::from_payload(i * 72, i * 72 + 54, b"Short line").unwrap()).collect();
    cues.push(Cue::from_payload(0, 7_200_000, b"whole film").unwrap());
    report("99,999 cues plus a film-long cue", &Track::new(cues));
}
