#![forbid(unsafe_code)]
//! SRT and WebVTT subtitles as timed cues.
//!
//! - [`parse_srt`] and [`parse_webvtt`]: a sidecar file's bytes to a [`Track`].
//! - [`Cue::from_payload`]: one in-container cue (Matroska `S_TEXT/UTF8` and `S_TEXT/WEBVTT`
//!   block payloads, with the block's time and duration) to a [`Cue`]; collect them with
//!   [`Track::new`].
//! - [`Track::active_at`]: the cues showing at a time.
//!
//! Layout and drawing are the app's job. Cue text keeps `<i>`, `<b>` and `<u>` as [`Span`] flags;
//! every other tag, SRT `{\an8}` override and control character is dropped, and the HTML entities
//! `&amp; &lt; &gt; &quot; &apos; &nbsp; &lrm; &rlm;` plus numeric ones are decoded.
//!
//! Input is hostile. A bad cue (unparsable timestamp, end not after start, no text) is skipped
//! and the rest of the file still loads. Bytes are UTF-8 (BOM allowed) or UTF-16 with a BOM;
//! invalid UTF-8 becomes U+FFFD. Limits: [`MAX_INPUT`] bytes per file, [`MAX_CUES`] cues per
//! track, 8 KiB of text and 16 lines per cue.
//!
//! Formats: SRT is the de facto SubRip format; WebVTT is "WebVTT: The Web Video Text Tracks
//! Format" (W3C Candidate Recommendation, April 2019), read leniently (cue settings, `STYLE`,
//! `REGION` and `NOTE` blocks are skipped; a cue may follow another without a blank line).
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unimplemented,
        clippy::todo,
        clippy::unreachable
    )
)]

use std::fmt;

/// Largest file [`parse_srt`] and [`parse_webvtt`] accept, in bytes.
pub const MAX_INPUT: usize = 8 << 20;
/// Most cues a [`Track`] keeps; the rest are dropped.
pub const MAX_CUES: usize = 100_000;
const MAX_CUE_BYTES: usize = 8 << 10;
const MAX_LINES: usize = 16;

/// Why a whole file was refused. A single bad cue never causes this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The input is longer than [`MAX_INPUT`].
    TooLarge,
    /// A WebVTT file must start with `WEBVTT`.
    NotWebVtt,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooLarge => write!(f, "subtitle file is larger than {MAX_INPUT} bytes"),
            Error::NotWebVtt => write!(f, "WebVTT file does not start with WEBVTT"),
        }
    }
}

impl std::error::Error for Error {}

/// A run of text with one style.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Span {
    pub text: String,
    pub italic: bool,
    pub bold: bool,
    pub underline: bool,
}

/// One subtitle: shown from `start_ms` up to, not including, `end_ms`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub start_ms: u64,
    pub end_ms: u64,
    /// Never empty; every line has at least one span.
    pub lines: Vec<Vec<Span>>,
}

impl Cue {
    /// A cue from one container block: its text (as stored in Matroska `S_TEXT/UTF8` or
    /// `S_TEXT/WEBVTT`) and its start and end times. `None` if the end is not after the start
    /// or no text is left after stripping.
    pub fn from_payload(start_ms: u64, end_ms: u64, payload: &[u8]) -> Option<Cue> {
        if end_ms <= start_ms {
            return None;
        }
        let payload = payload.get(..payload.len().min(MAX_CUE_BYTES))?;
        let text = String::from_utf8_lossy(payload).replace("\r\n", "\n").replace('\r', "\n");
        let lines = parse_markup(&text);
        (!lines.is_empty()).then_some(Cue { start_ms, end_ms, lines })
    }

    /// The cue's text without styling, lines joined with `\n`.
    pub fn text(&self) -> String {
        let lines: Vec<String> = self.lines.iter().map(|l| l.iter().map(|s| s.text.as_str()).collect()).collect();
        lines.join("\n")
    }
}

/// Cues sorted by start time, with lookup by time.
#[derive(Debug, Clone, Default)]
pub struct Track {
    cues: Vec<Cue>,
    /// `max_end[i]` is the latest end among `cues[..=i]`.
    max_end: Vec<u64>,
}

impl Track {
    /// Sorts the cues by start time (equal starts keep their order) and drops any with an end
    /// not after the start, or no text, and any beyond [`MAX_CUES`].
    pub fn new(mut cues: Vec<Cue>) -> Track {
        cues.retain(|c| c.end_ms > c.start_ms && !c.lines.is_empty());
        cues.truncate(MAX_CUES);
        cues.sort_by_key(|c| (c.start_ms, c.end_ms));
        let mut latest = 0;
        let max_end = cues
            .iter()
            .map(|c| {
                latest = latest.max(c.end_ms);
                latest
            })
            .collect();
        Track { cues, max_end }
    }

    /// All cues, sorted by start time.
    pub fn cues(&self) -> &[Cue] {
        &self.cues
    }

    /// The cues showing at `t_ms` (`start_ms <= t_ms < end_ms`), in start order. Overlapping cues
    /// are all returned. Costs a binary search plus the cues back to the latest one still showing.
    pub fn active_at(&self, t_ms: u64) -> Vec<&Cue> {
        let mut out = Vec::new();
        let mut i = self.cues.partition_point(|c| c.start_ms <= t_ms);
        while i > 0 {
            i -= 1;
            // Nothing at or before `i` is still showing.
            if self.max_end.get(i).is_none_or(|&end| end <= t_ms) {
                break;
            }
            if let Some(c) = self.cues.get(i).filter(|c| c.end_ms > t_ms) {
                out.push(c);
            }
        }
        out.reverse();
        out
    }
}

/// Parses an SRT file. Fails only if it is larger than [`MAX_INPUT`].
pub fn parse_srt(data: &[u8]) -> Result<Track, Error> {
    let text = decode(data)?;
    Ok(Track::new(parse_cues(&text, false)))
}

/// Parses a WebVTT file. Fails if it is larger than [`MAX_INPUT`] or lacks the `WEBVTT` header.
pub fn parse_webvtt(data: &[u8]) -> Result<Track, Error> {
    let text = decode(data)?;
    let header = text.strip_prefix("WEBVTT").ok_or(Error::NotWebVtt)?;
    if !header.starts_with([' ', '\t', '\n', '\r']) && !header.is_empty() {
        return Err(Error::NotWebVtt);
    }
    Ok(Track::new(parse_cues(&text, true)))
}

fn decode(data: &[u8]) -> Result<String, Error> {
    if data.len() > MAX_INPUT {
        return Err(Error::TooLarge);
    }
    let text = if let Some(rest) = data.strip_prefix(&[0xFF, 0xFE]) {
        utf16(rest, u16::from_le_bytes)
    } else if let Some(rest) = data.strip_prefix(&[0xFE, 0xFF]) {
        utf16(rest, u16::from_be_bytes)
    } else {
        let data = data.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(data);
        String::from_utf8_lossy(data).into_owned()
    };
    Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
}

fn utf16(data: &[u8], unit: fn([u8; 2]) -> u16) -> String {
    let units = data.as_chunks::<2>().0.iter().map(|&c| unit(c));
    char::decode_utf16(units).map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER)).collect()
}

enum State<'a> {
    /// Between cues: stray lines (indexes, identifiers, junk) are ignored.
    Idle,
    /// Reading a cue's text lines.
    Text { start_ms: u64, end_ms: u64, lines: Vec<&'a str>, bytes: usize },
    /// A cue or block being skipped until the next blank line or timing line.
    Skip,
}

fn parse_cues(text: &str, webvtt: bool) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut state = if webvtt { State::Skip } else { State::Idle };
    for line in text.split('\n') {
        if cues.len() >= MAX_CUES {
            break;
        }
        let blank = line.trim().is_empty();
        if let Some((start_ms, end_ms)) = parse_timing(line) {
            finish(&mut state, &mut cues, true);
            state = State::Text { start_ms, end_ms, lines: Vec::new(), bytes: 0 };
        } else if blank {
            finish(&mut state, &mut cues, false);
            state = State::Idle;
        } else if line.contains("-->") {
            // A timing line we can't read: skip its cue, keep the file.
            finish(&mut state, &mut cues, true);
            state = State::Skip;
        } else {
            match &mut state {
                State::Text { lines, bytes, .. } => {
                    *bytes += line.len();
                    if *bytes <= MAX_CUE_BYTES {
                        lines.push(line);
                    }
                }
                State::Idle if webvtt && is_block_start(line) => state = State::Skip,
                State::Idle | State::Skip => {}
            }
        }
    }
    finish(&mut state, &mut cues, false);
    cues
}

/// Ends the cue being read. `before_timing` is set when the next line is a timing line, so a
/// last line made only of digits is the next cue's index (a blank line was missing), not text.
fn finish(state: &mut State<'_>, cues: &mut Vec<Cue>, before_timing: bool) {
    if let State::Text { start_ms, end_ms, lines, .. } = std::mem::replace(state, State::Idle) {
        let mut lines = lines;
        if before_timing && lines.last().is_some_and(|l| is_index(l)) {
            lines.pop();
        }
        let lines = parse_markup(&lines.join("\n"));
        if !lines.is_empty() {
            cues.push(Cue { start_ms, end_ms, lines });
        }
    }
}

fn is_index(line: &str) -> bool {
    let line = line.trim();
    !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit())
}

fn is_block_start(line: &str) -> bool {
    ["NOTE", "STYLE", "REGION"]
        .iter()
        .any(|word| line.strip_prefix(word).is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t'])))
}

/// `start --> end` with anything after the end time (WebVTT cue settings, SRT coordinates)
/// ignored. `None` for bad timestamps and for an end that is not after the start.
fn parse_timing(line: &str) -> Option<(u64, u64)> {
    let (start, rest) = line.split_once("-->")?;
    let start = parse_timestamp(start)?;
    let end = parse_timestamp(rest.split_whitespace().next()?)?;
    (end > start).then_some((start, end))
}

/// `[H+:]MM:SS[.,]fff` (SRT writes a comma, WebVTT a dot; hours optional) in milliseconds.
/// One or two digits for minutes and seconds, at most six for hours, so nothing overflows.
fn parse_timestamp(s: &str) -> Option<u64> {
    fn number(s: &str, max_digits: usize) -> Option<u64> {
        if s.is_empty() || s.len() > max_digits || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    }
    let mut parts = s.trim().split(':');
    let (hours, minutes, rest) = match (parts.next()?, parts.next()?, parts.next(), parts.next()) {
        (_, _, _, Some(_)) => return None,
        (h, m, Some(rest), None) => (number(h, 6)?, number(m, 2)?, rest),
        (m, rest, None, None) => (0, number(m, 2)?, rest),
    };
    let (seconds, fraction) = rest.split_once(['.', ',']).unwrap_or((rest, ""));
    let seconds = number(seconds, 2)?;
    if minutes >= 60 || seconds >= 60 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // The fraction is decimal: ",5" is 500 ms.
    let digits = fraction.get(..fraction.len().min(3))?;
    let millis = number(&format!("{digits:0<3}"), 3)?;
    Some(((hours * 60 + minutes) * 60 + seconds) * 1000 + millis)
}

#[derive(Default)]
struct Markup {
    lines: Vec<Vec<Span>>,
    line: Vec<Span>,
    text: String,
    /// Open `<i>`, `<b>` and `<u>` tags.
    depth: [u32; 3],
}

impl Markup {
    fn flush(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let [i, b, u] = self.depth.map(|d| d > 0);
        let text = std::mem::take(&mut self.text);
        match self.line.last_mut() {
            Some(last) if (last.italic, last.bold, last.underline) == (i, b, u) => last.text.push_str(&text),
            _ => self.line.push(Span { text, italic: i, bold: b, underline: u }),
        }
    }

    fn end_line(&mut self) {
        self.flush();
        let mut line = std::mem::take(&mut self.line);
        if let Some(first) = line.first_mut() {
            first.text = first.text.trim_start().to_string();
        }
        if let Some(last) = line.last_mut() {
            last.text = last.text.trim_end().to_string();
        }
        line.retain(|s| !s.text.is_empty());
        if !line.is_empty() && self.lines.len() < MAX_LINES {
            self.lines.push(line);
        }
    }

    fn push_run(&mut self, run: &str) {
        for c in run.chars() {
            match c {
                '\t' => self.text.push(' '),
                c if c.is_control() => {}
                c => self.text.push(c),
            }
        }
    }
}

/// Cue text to lines of styled spans. Tags may span lines; unclosed tags end with the cue.
fn parse_markup(text: &str) -> Vec<Vec<Span>> {
    let mut m = Markup::default();
    let mut pos = 0;
    while pos < text.len() {
        let rest = text.get(pos..).unwrap_or("");
        // The specials are ASCII, so every slice below is on a character boundary.
        let run = rest.bytes().position(|b| matches!(b, b'<' | b'{' | b'&' | b'\n')).unwrap_or(rest.len());
        if run > 0 {
            m.push_run(rest.get(..run).unwrap_or(""));
            pos += run;
            continue;
        }
        let (consumed, kept) = match rest.as_bytes().first() {
            Some(b'\n') => {
                m.end_line();
                (1, None)
            }
            Some(b'<') => (tag(rest, &mut m), None),
            Some(b'{') => (override_len(rest), None),
            _ => entity(rest),
        };
        if consumed == 0 {
            // Not markup after all: keep the character.
            m.push_run(rest.get(..1).unwrap_or(""));
            pos += 1;
        } else {
            if let Some(c) = kept {
                m.push_run(c.encode_utf8(&mut [0; 4]));
            }
            pos += consumed;
        }
    }
    m.end_line();
    m.lines
}

/// Handles a tag at the start of `rest`: returns the bytes consumed (0 if it isn't a tag).
/// `<i>`, `<b>` and `<u>` (with any class suffix) set style; every other tag is dropped.
fn tag(rest: &str, m: &mut Markup) -> usize {
    let close = rest.bytes().enumerate().skip(1).take(129).find(|&(_, b)| matches!(b, b'>' | b'<' | b'\n'));
    let Some((end, b'>')) = close else {
        return 0;
    };
    let inner = rest.get(1..end).unwrap_or("");
    let (closing, name) = match inner.strip_prefix('/') {
        Some(name) => (true, name),
        None => (false, inner),
    };
    if !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return 0;
    }
    let name: String = name.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    let slot = match name.to_ascii_lowercase().as_str() {
        "i" => 0,
        "b" => 1,
        "u" => 2,
        _ => return end + 1,
    };
    // The text so far has the old style.
    m.flush();
    if let Some(depth) = m.depth.get_mut(slot) {
        *depth = if closing { depth.saturating_sub(1) } else { depth.saturating_add(1) };
    }
    end + 1
}

/// Length of an SRT `{\...}` override at the start of `rest`, or 0.
fn override_len(rest: &str) -> usize {
    if !rest.starts_with("{\\") {
        return 0;
    }
    match rest.bytes().take(64).position(|b| b == b'}') {
        Some(end) => end + 1,
        None => 0,
    }
}

/// An entity at the start of `rest`: bytes consumed and the character, or (0, None).
fn entity(rest: &str) -> (usize, Option<char>) {
    let Some(end) = rest.bytes().take(12).position(|b| b == b';') else {
        return (0, None);
    };
    let name = rest.get(1..end).unwrap_or("");
    let c = match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{a0}'),
        "lrm" => Some('\u{200e}'),
        "rlm" => Some('\u{200f}'),
        _ => name.strip_prefix('#').and_then(|n| {
            let code = match n.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16),
                None => n.parse(),
            };
            code.ok().and_then(char::from_u32)
        }),
    };
    match c {
        Some(c) => (end + 1, Some(c)),
        None => (0, None),
    }
}
