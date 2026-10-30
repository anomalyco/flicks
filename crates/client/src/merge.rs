//! One library from every saved server (docs/architecture.md, "App"): files are the same title
//! when TMDB says so (a movie's id; a series id with season and episode), or, unmatched, by parsed
//! title, year, season and episode. Each title lists its sources; the best one to play is on a
//! server that answered, then the highest quality, then the lowest latency. A server that doesn't
//! answer keeps its titles from the catalog it gave last time (`catalog.json`), marked
//! unavailable. Progress is newest-wins across servers, written back to the ones behind.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use hyper::Method;

use crate::{Client, Connection, Entry, Error, Item, Progress, Server};

/// The last catalog each server gave, in the data directory.
pub const CATALOG_FILE: &str = "catalog.json";

/// What makes files on different servers the same title.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    /// Matched on TMDB: `media` is `movie` or `tv`; a series' episodes have a season and episode.
    Tmdb { media: Option<String>, id: u64, season: Option<u16>, episode: Option<u16> },
    /// Unmatched: the parsed title (lowercase letters and digits only), year, season and episode.
    Name { title: String, year: Option<u16>, season: Option<u16>, episode: Option<u16> },
}

impl Key {
    fn tmdb(item: &Item) -> Option<Key> {
        let (media, season, episode) = (item.tmdb_media.clone(), item.season, item.episode);
        item.tmdb_id.map(|id| Key::Tmdb { media, id, season, episode })
    }

    fn name(item: &Item) -> Key {
        let title = item.title.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect();
        Key::Name { title, year: item.year, season: item.season, episode: item.episode }
    }
}

/// One file of a title, on one server.
#[derive(Debug, Clone)]
pub struct Source {
    /// The server's key ([`Server::key`]).
    pub server: String,
    pub server_name: String,
    /// The file there: `item.id` for `/play` and `/files`, its container, codec, size and
    /// resolution.
    pub item: Item,
    pub progress: Option<Progress>,
    /// The server answered on the last refresh; otherwise this is from its last catalog.
    pub available: bool,
    /// How long reaching the server took on the last refresh (the `/info` probe).
    pub latency: Option<Duration>,
}

/// One title, merged across servers.
#[derive(Debug, Clone)]
pub struct Title {
    pub key: Key,
    /// The facts to show: the best source's, or the first's when none is available.
    pub item: Item,
    /// In saved-server order.
    pub sources: Vec<Source>,
    /// The newest progress of any source.
    pub progress: Option<Progress>,
}

impl Title {
    pub fn available(&self) -> bool {
        self.sources.iter().any(|s| s.available)
    }
}

/// The source to play: on a server that answered, the most pixels, then the largest file (the
/// higher bitrate), then the lowest latency. `None` while every server with it is offline.
pub fn best_source(title: &Title) -> Option<&Source> {
    let quality =
        |s: &Source| (u64::from(s.item.width.unwrap_or(0)) * u64::from(s.item.height.unwrap_or(0)), s.item.size);
    title
        .sources
        .iter()
        .filter(|s| s.available)
        .max_by(|a, b| quality(a).cmp(&quality(b)).then(b.latency.cmp(&a.latency)))
}

/// Every saved server's catalog, merged.
pub struct Library {
    pub client: Client,
    titles: Vec<Title>,
    /// The servers that answered on the last refresh, by key.
    connections: HashMap<String, Connection>,
}

impl Library {
    /// The library as last cached, every source unavailable until [`Library::refresh`].
    pub fn open(client: Client) -> Library {
        let titles = merge(&client.servers, &read_cache(&client), &HashMap::new());
        Library { client, titles, connections: HashMap::new() }
    }

    /// The merged titles, sorted by title, year, season and episode.
    pub fn titles(&self) -> &[Title] {
        &self.titles
    }

    /// A connection to the server with this key, if it answered on the last refresh.
    pub fn connection(&self, server: &str) -> Option<&Connection> {
        self.connections.get(server)
    }

    /// Fetches every saved server's catalog (one at a time), caches what answered, merges, and
    /// writes the newest progress back to sources that are behind. A server that doesn't answer
    /// keeps its titles from its last catalog, unavailable.
    pub async fn refresh(&mut self) -> Result<&[Title], Error> {
        let mut catalogs = read_cache(&self.client);
        catalogs.retain(|key, _| self.client.servers.iter().any(|s| s.key == *key));
        self.connections.clear();
        let mut latencies = HashMap::new();
        let keys: Vec<String> = self.client.servers.iter().map(|s| s.key.clone()).collect();
        for key in keys {
            let started = Instant::now();
            let Ok(connection) = self.client.connect(&key).await else { continue };
            let latency = started.elapsed();
            let Ok(entries) = connection.items().await else { continue };
            catalogs.insert(key.clone(), entries);
            latencies.insert(key.clone(), latency);
            self.connections.insert(key, connection);
        }
        let bytes = serde_json::to_vec(&catalogs).map_err(|_| Error::BadResponse)?;
        self.client.write(CATALOG_FILE, &bytes)?;
        self.titles = merge(&self.client.servers, &catalogs, &latencies);

        for title in &mut self.titles {
            let Some(newest) = title.progress else { continue };
            for source in &mut title.sources {
                let behind = source.progress.is_none_or(|p| p.updated_at < newest.updated_at);
                let Some(connection) = self.connections.get(&source.server).filter(|_| behind) else { continue };
                // One that fails is retried on the next refresh.
                if let Ok(stored) = put_progress(connection, source.item.id, &newest).await {
                    source.progress = stored;
                }
            }
        }
        Ok(&self.titles)
    }

    /// Saves `progress` to every available source of the title with this key. Fails only if none
    /// took it; a source that was offline catches up on a later refresh.
    pub async fn set_progress(&mut self, key: &Key, progress: Progress) -> Result<(), Error> {
        let title = self.titles.iter_mut().find(|t| t.key == *key).ok_or(Error::Status(404, "no such title".into()))?;
        let mut result = Err(Error::Status(503, "every server with that title is offline".into()));
        for source in &mut title.sources {
            let Some(connection) = self.connections.get(&source.server) else { continue };
            match put_progress(connection, source.item.id, &progress).await {
                Ok(stored) => {
                    source.progress = stored;
                    result = Ok(());
                }
                Err(e) if result.is_err() => result = Err(e),
                Err(_) => {}
            }
        }
        title.progress = newest(&title.sources);
        result
    }
}

async fn put_progress(connection: &Connection, file: i64, progress: &Progress) -> Result<Option<Progress>, Error> {
    connection.call(Method::PUT, &format!("/progress/{file}"), Some(progress)).await
}

fn newest(sources: &[Source]) -> Option<Progress> {
    sources.iter().filter_map(|s| s.progress).max_by_key(|p| p.updated_at)
}

/// The cached catalogs by server key; a missing or unreadable cache is empty.
fn read_cache(client: &Client) -> BTreeMap<String, Vec<Entry>> {
    std::fs::read(client.dir.join(CATALOG_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Groups every server's entries into titles. Servers in `latencies` answered; the rest are
/// unavailable. Matched files go first, so an unmatched copy of a matched title (a server without
/// a TMDB key) joins it by name.
fn merge(
    servers: &[Server],
    catalogs: &BTreeMap<String, Vec<Entry>>,
    latencies: &HashMap<String, Duration>,
) -> Vec<Title> {
    let mut sources: Vec<Source> = servers
        .iter()
        .flat_map(|server| {
            let latency = latencies.get(&server.key).copied();
            catalogs.get(&server.key).into_iter().flatten().map(move |entry| Source {
                server: server.key.clone(),
                server_name: server.name.clone(),
                item: entry.item.clone(),
                progress: entry.progress,
                available: latency.is_some(),
                latency,
            })
        })
        .collect();
    sources.sort_by_key(|s| s.item.tmdb_id.is_none());

    let mut titles: Vec<Title> = Vec::new();
    let mut by_key: HashMap<Key, usize> = HashMap::new();
    for source in sources {
        let name = Key::name(&source.item);
        let key = Key::tmdb(&source.item).unwrap_or_else(|| name.clone());
        let at = match by_key.get(&key) {
            Some(at) => *at,
            None => {
                let item = source.item.clone();
                titles.push(Title { key: key.clone(), item, sources: Vec::new(), progress: None });
                titles.len() - 1
            }
        };
        by_key.entry(key).or_insert(at);
        by_key.entry(name).or_insert(at);
        if let Some(title) = titles.get_mut(at) {
            title.sources.push(source);
        }
    }
    for title in &mut titles {
        title.sources.sort_by_key(|s| servers.iter().position(|server| server.key == s.server));
        title.progress = newest(&title.sources);
        if let Some(best) = best_source(title) {
            title.item = best.item.clone();
        }
    }
    titles.sort_by_cached_key(|t| (t.item.title.to_lowercase(), t.item.year, t.item.season, t.item.episode));
    titles
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64, title: &str, tmdb: Option<u64>, episode: Option<u16>) -> Entry {
        let item = Item {
            id,
            title: title.into(),
            year: Some(2026),
            season: episode.map(|_| 1),
            episode,
            episode_end: None,
            edition: None,
            tmdb_id: tmdb,
            tmdb_media: tmdb.map(|_| if episode.is_some() { "tv" } else { "movie" }.into()),
            poster: None,
            backdrop: None,
            added_at: 0,
            size: 1,
            oshash: String::new(),
            container: "mkv".into(),
            duration_ms: None,
            video_codec: None,
            width: None,
            height: None,
            hdr: None,
        };
        Entry { item, progress: None }
    }

    fn server(key: &str) -> Server {
        Server { key: key.into(), name: key.into(), addrs: Vec::new(), user: "ana".into(), admin: false }
    }

    #[test]
    fn matches_by_tmdb_then_by_name() {
        let servers = [server("a"), server("b")];
        let catalogs = BTreeMap::from([
            // A has a TMDB key: titles as TMDB names them.
            ("a".to_string(), vec![entry(1, "Vision Quest", Some(7), Some(2)), entry(2, "Arrival", Some(9), None)]),
            // B has none: an episode under its parsed name joins A's by name; a differently named
            // file with the same TMDB id joins by id; another episode stays apart.
            (
                "b".to_string(),
                vec![
                    entry(3, "VisionQuest", None, Some(2)),
                    entry(4, "Arrival", Some(9), None),
                    entry(5, "VisionQuest", None, Some(6)),
                ],
            ),
        ]);
        let titles = merge(&servers, &catalogs, &HashMap::from([("b".to_string(), Duration::ZERO)]));
        let ids: Vec<Vec<i64>> = titles.iter().map(|t| t.sources.iter().map(|s| s.item.id).collect()).collect();
        assert_eq!(ids, [vec![2, 4], vec![1, 3], vec![5]]);
        // Only B answered, so B's copy is the one to play and shown.
        assert_eq!(titles[1].item.id, 3);
        assert_eq!(best_source(&titles[1]).map(|s| s.server.as_str()), Some("b"));
    }
}
