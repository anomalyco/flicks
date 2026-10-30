//! Names from paths. Handles Plex/Jellyfin layouts (`Show (2010)/Season 01/Show - S01E02.mkv`,
//! `Movie (2010) {edition-IMAX}.mkv`), scene releases (`The.Movie.2010.1080p.BluRay.x264-GRP.mkv`)
//! and fansub names (`[Group] Show - 05 [1080p].mkv`).
//!
//! The file name is split into tokens; the title is everything before the first year, episode
//! marker or release tag. Folders only fill what the file name lacks (the show, a year).

/// What a path says about its file. Empty `title` means nothing usable was found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parsed {
    pub title: String,
    pub year: Option<u16>,
    pub season: Option<u16>,
    pub episode: Option<u16>,
    /// Last episode of a multi-episode file (`S01E01E02`, `S01E01-E03`).
    pub episode_end: Option<u16>,
    pub edition: Option<String>,
}

impl Parsed {
    pub fn is_episode(&self) -> bool {
        self.episode.is_some()
    }
}

/// Title key for comparing names: lower case letters and digits only, `&` as "and", common
/// Latin accents folded ("Shōgun" and "Amélie" match "Shogun" and "Amelie").
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '&' {
            out.push_str("and");
        } else if c.is_alphanumeric() {
            for c in c.to_lowercase() {
                match fold(c) {
                    Some(plain) => out.push_str(plain),
                    None => out.push(c),
                }
            }
        }
    }
    out
}

/// The plain ASCII spelling of a lower case Latin letter with a diacritic.
fn fold(c: char) -> Option<&'static str> {
    Some(match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'ç' | 'ć' | 'č' => "c",
        'ď' | 'đ' | 'ð' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => "e",
        'ğ' => "g",
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'ı' => "i",
        'ł' => "l",
        'ñ' | 'ń' | 'ň' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => "o",
        'ř' => "r",
        'ś' | 'š' | 'ş' => "s",
        'ť' | 'ţ' => "t",
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' => "u",
        'ý' | 'ÿ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        'æ' => "ae",
        'œ' => "oe",
        'ß' => "ss",
        _ => return None,
    })
}

const VIDEO_EXTENSIONS: &[&str] =
    &["mkv", "mp4", "m4v", "mov", "avi", "ts", "m2ts", "mts", "webm", "wmv", "mpg", "mpeg", "flv", "ogv", "divx"];

/// Release tags that end the title. Only words that are not plausible title words.
const TAGS: &[&str] = &[
    "bluray",
    "blu-ray",
    "bdrip",
    "brrip",
    "bdremux",
    "remux",
    "webrip",
    "web-dl",
    "web-rip",
    "webdl",
    "hdtv",
    "pdtv",
    "dvdrip",
    "dvdscr",
    "hdrip",
    "hdcam",
    "hdts",
    "telesync",
    "x264",
    "x265",
    "av1",
    "vp9",
    "h264",
    "h265",
    "hevc",
    "xvid",
    "divx",
    "aac",
    "ac3",
    "eac3",
    "dts",
    "dtshd",
    "dtsx",
    "truehd",
    "atmos",
    "10bit",
    "8bit",
    "hdr",
    "hdr10",
    "hdr10plus",
    "dovi",
    "uhd",
    "4k",
    "8k",
    "repack",
    "rerip",
    "subbed",
    "dubbed",
    "amzn",
    "dsnp",
    "atvp",
    "hmax",
];

/// Edition phrases (lower case, apostrophes removed), longest first, and their label.
const EDITIONS: &[(&[&str], &str)] = &[
    (&["directors", "cut"], "Director's Cut"),
    (&["director", "s", "cut"], "Director's Cut"),
    (&["extended", "cut"], "Extended"),
    (&["extended", "edition"], "Extended"),
    (&["extended", "version"], "Extended"),
    (&["theatrical", "cut"], "Theatrical"),
    (&["theatrical", "edition"], "Theatrical"),
    (&["theatrical", "version"], "Theatrical"),
    (&["special", "edition"], "Special Edition"),
    (&["ultimate", "edition"], "Ultimate Edition"),
    (&["ultimate", "cut"], "Ultimate Cut"),
    (&["collectors", "edition"], "Collector's Edition"),
    (&["final", "cut"], "Final Cut"),
    (&["extended"], "Extended"),
    (&["theatrical"], "Theatrical"),
    (&["unrated"], "Unrated"),
    (&["uncut"], "Uncut"),
    (&["remastered"], "Remastered"),
    (&["imax"], "IMAX"),
];

struct Token {
    text: String,
    /// Lower case without apostrophes and `+`.
    lower: String,
    /// Inside `()`, `[]` or `{}`.
    bracket: bool,
}

fn tokenize(s: &str) -> Vec<Token> {
    fn push(out: &mut Vec<Token>, cur: &mut String, bracket: bool) {
        if cur.is_empty() {
            return;
        }
        let text = std::mem::take(cur);
        let lower = text.chars().filter(|c| !matches!(c, '\'' | '’' | '+')).flat_map(char::to_lowercase).collect();
        out.push(Token { text, lower, bracket });
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0u32;
    for c in s.chars() {
        match c {
            '(' | '[' | '{' => {
                push(&mut out, &mut cur, depth > 0);
                depth += 1;
            }
            ')' | ']' | '}' => {
                push(&mut out, &mut cur, depth > 0);
                depth = depth.saturating_sub(1);
            }
            ' ' | '.' | '_' | '\t' | ',' => push(&mut out, &mut cur, depth > 0),
            _ => cur.push(c),
        }
    }
    push(&mut out, &mut cur, depth > 0);
    out
}

/// Up to `max` ascii digits starting at `at`: the value and the index after them.
fn num(b: &[u8], at: usize, max: usize) -> Option<(u16, usize)> {
    let mut i = at;
    let mut v = 0u16;
    while let Some(d) = b.get(i).filter(|d| d.is_ascii_digit()) {
        if i - at == max {
            return None;
        }
        v = v * 10 + u16::from(d - b'0');
        i += 1;
    }
    (i > at).then_some((v, i))
}

fn all_digits(s: &str, max: usize) -> Option<u16> {
    num(s.as_bytes(), 0, max).filter(|(_, i)| *i == s.len()).map(|(v, _)| v)
}

fn year_of(t: &Token) -> Option<u16> {
    all_digits(&t.text, 4).filter(|_| t.text.len() == 4).filter(|y| (1900..=2035).contains(y))
}

/// `s01`, `s01e02`, `s01e02e03`, `s01e02-e03`, `s01e02-03`: season, episode, last episode.
fn season_episode(t: &str) -> Option<(u16, Option<u16>, Option<u16>)> {
    let b = t.strip_prefix('s')?.as_bytes();
    let (season, mut i) = num(b, 0, 3)?;
    if i == b.len() {
        return Some((season, None, None));
    }
    if b.get(i) != Some(&b'e') {
        return None;
    }
    let (episode, j) = num(b, i + 1, 3)?;
    i = j;
    let mut end = None;
    while let Some(c) = b.get(i) {
        let mut k = i + 1;
        match c {
            b'e' => {}
            b'-' => {
                if b.get(k) == Some(&b'e') {
                    k += 1;
                }
            }
            _ => break,
        }
        let Some((n, j)) = num(b, k, 3) else { break };
        end = Some(n);
        i = j;
    }
    Some((season, Some(episode), end))
}

/// `1x05`, `10x12`.
fn nxn(t: &str) -> Option<(u16, u16)> {
    let b = t.as_bytes();
    let (season, i) = num(b, 0, 2)?;
    if b.get(i) != Some(&b'x') {
        return None;
    }
    let (episode, j) = num(b, i + 1, 3)?;
    (j == b.len()).then_some((season, episode))
}

/// `e05`, `ep05`.
fn bare_episode(t: &str) -> Option<u16> {
    let rest = t.strip_prefix("ep").or_else(|| t.strip_prefix('e'))?;
    all_digits(rest, 3)
}

/// `05`, `05v2` (an episode number after a dash).
fn dash_number(t: &str) -> Option<u16> {
    let (n, i) = num(t.as_bytes(), 0, 3)?;
    let rest = t.get(i..)?;
    (rest.is_empty() || (rest.starts_with('v') && all_digits(&rest[1..], 1).is_some())).then_some(n)
}

fn is_tag(t: &str) -> bool {
    let base = t.split('-').next().unwrap_or(t);
    if TAGS.contains(&t) || TAGS.contains(&base) {
        return true;
    }
    let resolution = base
        .strip_suffix(['p', 'i'])
        .is_some_and(|d| (3..=4).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_digit()));
    let audio = ["ddp", "dd", "aac", "eac3", "ac3"]
        .iter()
        .any(|p| base.strip_prefix(p).is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit())));
    let disc = ["cd", "disc", "disk"]
        .iter()
        .any(|p| base.strip_prefix(p).is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit())));
    resolution || audio || disc
}

/// How much of a bare number may be an episode: nothing, fansub names (`[Group] Show - 05`), or a
/// season folder around the file (`Season 1/05 - Pilot.mkv`).
#[derive(Clone, Copy, PartialEq)]
enum Bare {
    No,
    Group,
    Season,
}

struct Marker {
    season: Option<u16>,
    episode: Option<u16>,
    end: Option<u16>,
}

fn episode_marker(toks: &[Token], i: usize, bare: Bare, first: usize) -> Option<Marker> {
    let t = toks.get(i).filter(|t| !t.bracket)?;
    let next = toks.get(i + 1).filter(|t| !t.bracket);
    let m = |season, episode, end| Some(Marker { season, episode, end });
    if let Some((s, e, end)) = season_episode(&t.lower) {
        if e.is_none()
            && let Some(e) = next.and_then(|n| bare_episode(&n.lower))
        {
            return m(Some(s), Some(e), None);
        }
        return m(Some(s), e, end);
    }
    if let Some((s, e)) = nxn(&t.lower) {
        return m(Some(s), Some(e), None);
    }
    if t.lower == "season" {
        let s = next.and_then(|n| all_digits(&n.lower, 3))?;
        let ep = toks.get(i + 2).filter(|t| !t.bracket && matches!(t.lower.as_str(), "episode" | "ep"));
        if let Some(e) = ep.and_then(|_| toks.get(i + 3)).and_then(|n| all_digits(&n.lower, 3)) {
            return m(Some(s), Some(e), None);
        }
        return m(Some(s), None, None);
    }
    if bare == Bare::No {
        return None;
    }
    if let Some(e) = bare_episode(&t.lower) {
        return m(None, Some(e), None);
    }
    if t.lower == "episode" {
        return next.and_then(|n| all_digits(&n.lower, 3)).and_then(|e| m(None, Some(e), None));
    }
    if t.text == "-" {
        return next.and_then(|n| dash_number(&n.lower)).and_then(|e| m(None, Some(e), None));
    }
    if bare == Bare::Season && i == first && next.is_some() {
        return all_digits(&t.lower, 3).and_then(|e| m(None, Some(e), None));
    }
    None
}

#[derive(Default)]
struct Name {
    title: String,
    year: Option<u16>,
    season: Option<u16>,
    episode: Option<u16>,
    end: Option<u16>,
    edition: Option<String>,
    /// Starts with a `[Group]` tag.
    group: bool,
}

/// Plex `{edition-Director's Cut}`: the label and the name without it.
fn take_edition_tag(name: &str) -> (String, Option<String>) {
    let lower = name.to_ascii_lowercase();
    if let Some(s) = lower.find("{edition-")
        && let Some((label, rest)) = name.get(s + 9..).and_then(|r| r.split_once('}'))
    {
        let label = label.trim();
        let before = name.get(..s).unwrap_or("");
        return (format!("{before}{rest}"), (!label.is_empty()).then(|| label.to_string()));
    }
    (name.to_string(), None)
}

fn find_editions(toks: &[Token], start: usize) -> (Option<&'static str>, Vec<bool>) {
    let mut used = vec![false; toks.len()];
    let mut found = None;
    // Words before a bracketed year are the title ("The Final Cut (2004)"), not an edition.
    let year = toks.iter().rposition(|t| t.bracket && year_of(t).is_some());
    let mut i = start + 1;
    while i < toks.len() {
        let in_title = !toks[i].bracket && year.is_some_and(|y| i < y);
        let hit = EDITIONS
            .iter()
            .filter(|_| !in_title)
            .find(|(words, _)| words.iter().enumerate().all(|(k, w)| toks.get(i + k).is_some_and(|t| t.lower == *w)));
        if let Some((words, label)) = hit {
            found.get_or_insert(*label);
            for u in used.iter_mut().skip(i).take(words.len()) {
                *u = true;
            }
            i += words.len();
        } else {
            i += 1;
        }
    }
    (found, used)
}

fn parse_name(raw: &str, mode: Bare) -> Name {
    let (raw, tagged) = take_edition_tag(raw);
    let raw = raw.replace(['–', '—'], "-");
    let toks = tokenize(&raw);
    let mut name = Name { group: raw.trim_start().starts_with('['), ..Name::default() };
    if let [only] = toks.as_slice()
        && matches!(only.lower.as_str(), "specials" | "special")
    {
        name.season = Some(0);
        return name;
    }
    let start = toks.iter().position(|t| !t.bracket).unwrap_or(toks.len());
    let (edition, used) = find_editions(&toks, start);
    name.edition = tagged.or_else(|| edition.map(str::to_string));

    // The title ends at the first episode marker or release tag.
    let mut end = toks.len();
    let mut i = start;
    while i < toks.len() {
        let web = toks.get(i).is_some_and(|t| t.lower == "web")
            && toks.get(i + 1).is_some_and(|t| matches!(t.lower.as_str(), "dl" | "rip" | "dlrip"));
        if let Some(m) = episode_marker(&toks, i, mode, start) {
            (name.season, name.episode, name.end) = (m.season, m.episode, m.end);
            end = i;
            break;
        }
        // "H.264" and "H.265" split into two tokens.
        let dotted = toks.get(i).is_some_and(|t| t.lower == "h")
            && toks.get(i + 1).is_some_and(|t| matches!(t.lower.split('-').next(), Some("264" | "265")));
        if web || dotted || toks.get(i).is_some_and(|t| is_tag(&t.lower)) {
            end = i;
            break;
        }
        i += 1;
    }

    // The year is the last bracketed one, else the last bare one that has a title before it
    // ("1917.2019", "Wonder Woman 1984 (2020)").
    let before = toks.get(..end).unwrap_or(&[]);
    let has_title_before = |i: usize| {
        toks.get(start..i)
            .is_some_and(|ts| ts.iter().enumerate().any(|(k, t)| !t.bracket && t.text != "-" && !used[start + k]))
    };
    let year_at = before.iter().rposition(|t| t.bracket && year_of(t).is_some()).or_else(|| {
        (start..before.len())
            .rev()
            .find(|&i| year_of(&before[i]).is_some() && !before[i].bracket && has_title_before(i))
    });
    if let Some(i) = year_at {
        name.year = before.get(i).and_then(year_of);
        end = i;
    }

    let words: Vec<&str> = toks
        .iter()
        .enumerate()
        .take(end)
        .skip(start)
        .filter(|(i, t)| !t.bracket && !used[*i])
        .map(|(_, t)| t.text.as_str())
        .collect();
    let words = words.iter().skip_while(|w| **w == "-").copied().collect::<Vec<_>>();
    let keep = words.iter().rposition(|w| *w != "-").map_or(0, |p| p + 1);
    name.title = words.get(..keep).unwrap_or(&[]).join(" ");
    name
}

fn strip_extension(file: &str) -> &str {
    match file.rsplit_once('.') {
        Some((stem, ext)) if VIDEO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) => stem,
        _ => file,
    }
}

fn same(a: &str, b: &str) -> bool {
    let a = normalize(a);
    !a.is_empty() && a == normalize(b)
}

/// Parses a file path (any mix of `/` and `\` separators).
pub fn parse(path: &str) -> Parsed {
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|c| !c.is_empty() && *c != ".").collect();
    let Some((file, dirs)) = parts.split_last() else { return Parsed::default() };
    let file = strip_extension(file);
    let folder = |back: usize| dirs.len().checked_sub(back).and_then(|i| dirs.get(i)).map(|d| parse_name(d, Bare::No));
    let (p1, p2) = (folder(1), folder(2));
    let in_season = p1.as_ref().is_some_and(|p| p.season.is_some() && p.episode.is_none());

    let mut f = parse_name(file, Bare::No);
    if f.episode.is_none() {
        let bare = if in_season {
            Some(Bare::Season)
        } else if f.group {
            Some(Bare::Group)
        } else {
            None
        };
        if let Some(bare) = bare {
            let b = parse_name(file, bare);
            if b.episode.is_some() {
                f = b;
            }
        }
    }

    let tv = f.episode.is_some();
    let folders: Vec<&Name> = if tv { p1.iter().chain(p2.iter()).collect() } else { p1.iter().collect() };
    for p in folders {
        if f.title.is_empty() && !p.title.is_empty() {
            f.title = p.title.clone();
        }
        if f.year.is_none() && p.year.is_some() && (p.title.is_empty() || same(&p.title, &f.title)) {
            f.year = p.year;
        }
        if !tv && f.edition.is_none() && same(&p.title, &f.title) {
            f.edition = p.edition.clone();
        }
    }
    let season = f.season.or_else(|| p1.as_ref().filter(|_| tv).and_then(|p| p.season));
    Parsed { title: f.title, year: f.year, season, episode: f.episode, episode_end: f.end, edition: f.edition }
}
