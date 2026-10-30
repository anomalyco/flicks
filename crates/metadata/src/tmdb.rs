//! TMDB v3 search. The network sits behind one fetch function so tests replay recorded responses.

use std::time::Duration;

use serde_json::Value;

use crate::{Error, Parsed, normalize};

const API: &str = "https://api.themoviedb.org/3";
/// Responses are small JSON or JPEG posters; anything bigger is not TMDB.
const MAX_BODY: u64 = 8 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    Movie,
    Tv,
}

/// The TMDB entry a name matched. `poster` and `backdrop` are TMDB image paths (`/abc.jpg`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub id: u64,
    pub media: Media,
    pub title: String,
    pub year: Option<u16>,
    pub poster: Option<String>,
    pub backdrop: Option<String>,
}

type Fetch = Box<dyn Fn(&str) -> Result<Vec<u8>, Error> + Send + Sync>;

/// A TMDB client. Matching a show matches the series; episodes keep their parsed season and number.
pub struct Tmdb {
    key: String,
    pub(crate) fetch: Fetch,
}

impl Tmdb {
    /// A client for `FLICKS_TMDB_KEY` (a v3 API key), or `None` when it is unset or empty.
    pub fn from_env() -> Option<Self> {
        let key = std::env::var("FLICKS_TMDB_KEY").ok()?;
        let key = key.trim();
        if key.is_empty() {
            return None;
        }
        let agent: ureq::Agent =
            ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).build().into();
        let secret = key.to_string();
        Some(Self::new(key, move |url| live(&agent, &secret, url)))
    }

    /// A client that gets URLs through `fetch`.
    pub fn new(key: &str, fetch: impl Fn(&str) -> Result<Vec<u8>, Error> + Send + Sync + 'static) -> Self {
        Self { key: key.to_string(), fetch: Box::new(fetch) }
    }

    /// The best TMDB entry for a parsed name, or `None` when nothing has the same title (and a
    /// year within one of the parsed one). A show when the name has an episode, else a movie.
    pub fn find(&self, p: &Parsed) -> Result<Option<Match>, Error> {
        if normalize(&p.title).is_empty() {
            return Ok(None);
        }
        let media = if p.is_episode() { Media::Tv } else { Media::Movie };
        if p.year.is_some()
            && let Some(m) = self.search(p, media, true)?
        {
            return Ok(Some(m));
        }
        self.search(p, media, false)
    }

    fn search(&self, p: &Parsed, media: Media, with_year: bool) -> Result<Option<Match>, Error> {
        let (path, year_param) = match media {
            Media::Movie => ("search/movie", "year"),
            Media::Tv => ("search/tv", "first_air_date_year"),
        };
        let mut url = format!("{API}/{path}?api_key={}&query={}", self.key, encode(&p.title));
        if let (true, Some(y)) = (with_year, p.year) {
            url.push_str(&format!("&{year_param}={y}"));
        }
        let body = (self.fetch)(&url)?;
        let json: Value = serde_json::from_slice(&body).map_err(|e| Error::Json(e.to_string()))?;
        let results = json.get("results").and_then(Value::as_array).ok_or_else(|| Error::Json("no results".into()))?;

        let want = normalize(&p.title);
        let mut near = None;
        for r in results.iter().take(20) {
            let Some(m) = candidate(r, media, &want) else { continue };
            match (p.year, m.year) {
                (Some(a), Some(b)) if a == b => return Ok(Some(m)),
                (Some(a), Some(b)) if a.abs_diff(b) <= 1 => {
                    near.get_or_insert(m);
                }
                (Some(_), _) => {}
                (None, _) => return Ok(Some(m)),
            }
        }
        Ok(near)
    }
}

/// The result as a `Match` when its title or original title is the wanted one.
fn candidate(r: &Value, media: Media, want: &str) -> Option<Match> {
    let (title_key, original_key, date_key) = match media {
        Media::Movie => ("title", "original_title", "release_date"),
        Media::Tv => ("name", "original_name", "first_air_date"),
    };
    let text = |k: &str| r.get(k).and_then(Value::as_str);
    let title = text(title_key).or_else(|| text(original_key))?;
    let same = |t: Option<&str>| t.is_some_and(|t| normalize(t) == want);
    if !same(Some(title)) && !same(text(original_key)) {
        return None;
    }
    let path = |k: &str| text(k).filter(|p| !p.is_empty()).map(str::to_string);
    Some(Match {
        id: r.get("id").and_then(Value::as_u64)?,
        media,
        title: title.to_string(),
        year: text(date_key).and_then(|d| d.get(..4)).and_then(|y| y.parse().ok()),
        poster: path("poster_path"),
        backdrop: path("backdrop_path"),
    })
}

/// Percent-encodes everything but unreserved characters.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn live(agent: &ureq::Agent, key: &str, url: &str) -> Result<Vec<u8>, Error> {
    // The URL carries the API key, so errors here never repeat it.
    let mut response = agent.get(url).call().map_err(|e| Error::Http(redact(&e.to_string(), key, url)))?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_vec()
        .map_err(|e| Error::Http(redact(&e.to_string(), key, url)))
}

/// The message without the request URL or the key itself, wherever either appears.
fn redact(msg: &str, key: &str, url: &str) -> String {
    let msg = msg.replace(url, "<url>");
    if key.is_empty() { msg } else { msg.replace(key, "<key>") }
}
