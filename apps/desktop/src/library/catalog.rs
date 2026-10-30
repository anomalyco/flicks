//! The catalog the screens show: every item from every saved server, grouped into titles (a
//! movie, or a show with its episodes) and laid out as Home's rows.

use std::collections::HashMap;
use std::sync::Arc;

use flicks_client::{Client, Connection, Entry, Item, Progress};
use hyper::Method;
use serde::Deserialize;

/// What one server answered.
pub struct ServerCatalog {
    /// The server key.
    pub server: String,
    pub libraries: Vec<(String, Vec<Entry>)>,
    pub home: Home,
}

/// `GET /home`.
#[derive(Default, Deserialize)]
pub struct Home {
    pub continue_watching: Vec<Entry>,
    pub recently_added: Vec<Entry>,
}

#[derive(Deserialize)]
struct Library {
    id: i64,
    name: String,
}

/// Everything [`load`] read, with the connections that answered (for artwork).
#[derive(Default)]
pub struct Loaded {
    pub servers: Vec<ServerCatalog>,
    /// One line per server that couldn't be read.
    pub errors: Vec<String>,
    pub connections: Vec<(String, Arc<Connection>)>,
}

/// Reads the catalog of every saved server. This is the one place the app reads catalog data;
/// the merged multi-server library (FLK-40) replaces it.
pub async fn load(client: &mut Client) -> Loaded {
    let mut loaded = Loaded::default();
    let servers: Vec<(String, String)> = client.servers.iter().map(|s| (s.key.clone(), s.name.clone())).collect();
    for (key, name) in servers {
        let read = async {
            let connection = client.connect(&key).await?;
            let catalog = read_server(&connection, &key).await?;
            Ok::<_, flicks_client::Error>((connection, catalog))
        };
        match read.await {
            Ok((connection, catalog)) => {
                loaded.servers.push(catalog);
                loaded.connections.push((key, Arc::new(connection)));
            }
            Err(error) => loaded.errors.push(format!("{name}: {error}")),
        }
    }
    loaded
}

async fn read_server(connection: &Connection, key: &str) -> Result<ServerCatalog, flicks_client::Error> {
    const PAGE: usize = 200;
    let home: Home = connection.call(Method::GET, "/home", None::<&()>).await?;
    let mut libraries = Vec::new();
    let list: Vec<Library> = connection.call(Method::GET, "/libraries", None::<&()>).await?;
    for library in list {
        let mut entries = Vec::new();
        // At most 10,000 pages (two million items).
        for page in 0..10_000 {
            let path = format!("/items?library={}&limit={PAGE}&offset={}", library.id, page * PAGE);
            let got: Vec<Entry> = connection.call(Method::GET, &path, None::<&()>).await?;
            let done = got.len() < PAGE;
            entries.extend(got);
            if done {
                break;
            }
        }
        libraries.push((library.name, entries));
    }
    Ok(ServerCatalog { server: key.to_string(), libraries, home })
}

/// One file on one server.
pub struct Media {
    pub server: String,
    pub item: Item,
    pub progress: Option<Progress>,
}

impl Media {
    pub fn is_episode(&self) -> bool {
        self.item.season.is_some() || self.item.episode.is_some() || self.item.tmdb_media.as_deref() == Some("tv")
    }

    /// "S1 · E2", or "S1 · E2–E3" for a multi-episode file.
    pub fn episode_label(&self) -> Option<String> {
        let episode = self.item.episode?;
        let episodes = match self.item.episode_end {
            Some(end) if end > episode => format!("E{episode}–E{end}"),
            _ => format!("E{episode}"),
        };
        Some(match self.item.season {
            Some(season) => format!("S{season} · {episodes}"),
            None => episodes,
        })
    }

    /// How far in, 0 to 1, when there's progress worth showing.
    pub fn watched(&self) -> Option<f32> {
        let progress = self.progress.as_ref().filter(|p| !p.played && p.position > 0)?;
        let duration = self.item.duration_ms.filter(|d| *d > 0)?;
        Some((progress.position as f64 / duration as f64).clamp(0.0, 1.0) as f32)
    }
}

/// A movie, or a show and its episodes.
pub struct Title {
    pub key: String,
    pub name: String,
    pub year: Option<u16>,
    pub show: bool,
    /// Indices into [`Catalog::media`], in season and episode order.
    pub media: Vec<usize>,
}

pub struct Row {
    pub name: String,
    /// Indices into [`Catalog::media`].
    pub media: Vec<usize>,
    /// The tiles stand for whole titles (a library), not single episodes.
    pub titles: bool,
}

#[derive(Default)]
pub struct Catalog {
    pub media: Vec<Media>,
    pub titles: Vec<Title>,
    /// The title of each media.
    pub title_of: Vec<usize>,
    /// Home: Continue Watching, Recently Added, then one row per library.
    pub rows: Vec<Row>,
}

impl Catalog {
    pub fn build(servers: Vec<ServerCatalog>) -> Catalog {
        let mut catalog = Catalog::default();
        let mut by_id: HashMap<(String, i64), usize> = HashMap::new();
        let mut add = |catalog: &mut Catalog, server: &str, entry: Entry| {
            let id = (server.to_string(), entry.item.id);
            if let Some(&at) = by_id.get(&id) {
                return at;
            }
            let at = catalog.media.len();
            catalog.media.push(Media { server: server.to_string(), item: entry.item, progress: entry.progress });
            by_id.insert(id, at);
            at
        };
        let mut libraries = Vec::new();
        let (mut watching, mut added) = (Vec::new(), Vec::new());
        for server in servers {
            for (name, entries) in server.libraries {
                let media: Vec<usize> = entries.into_iter().map(|e| add(&mut catalog, &server.server, e)).collect();
                libraries.push((name, media));
            }
            watching.extend(server.home.continue_watching.into_iter().map(|e| add(&mut catalog, &server.server, e)));
            added.extend(server.home.recently_added.into_iter().map(|e| add(&mut catalog, &server.server, e)));
        }

        let mut title_at: HashMap<String, usize> = HashMap::new();
        for at in 0..catalog.media.len() {
            let media = &catalog.media[at];
            let show = media.is_episode();
            let key = match (show, media.item.tmdb_id) {
                (true, Some(tmdb)) => format!("{}/tv/{tmdb}", media.server),
                (true, None) => format!("{}/tv/{}", media.server, media.item.title.to_lowercase()),
                (false, _) => format!("{}/{}", media.server, media.item.id),
            };
            let title = *title_at.entry(key.clone()).or_insert_with(|| {
                let (name, year) = (media.item.title.clone(), media.item.year);
                catalog.titles.push(Title { key, name, year, show, media: Vec::new() });
                catalog.titles.len() - 1
            });
            catalog.titles[title].media.push(at);
            catalog.title_of.push(title);
        }
        let media = &catalog.media;
        for title in &mut catalog.titles {
            title.media.sort_by_key(|&m| (media[m].item.season, media[m].item.episode, media[m].item.id));
        }

        let newest_progress = |&m: &usize| std::cmp::Reverse(media[m].progress.as_ref().map_or(0, |p| p.updated_at));
        watching.sort_by_key(newest_progress);
        added.sort_by_key(|&m| std::cmp::Reverse(media[m].item.added_at));
        for (name, list) in [("Continue Watching", watching), ("Recently Added", added)] {
            if !list.is_empty() {
                catalog.rows.push(Row { name: name.to_string(), media: list, titles: false });
            }
        }
        for (name, list) in libraries {
            // One tile per title, by name.
            let mut titles: Vec<usize> = list.iter().map(|&m| catalog.title_of[m]).collect();
            titles.sort_by(|&a, &b| {
                let (a, b) = (&catalog.titles[a], &catalog.titles[b]);
                a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.year.cmp(&b.year))
            });
            titles.dedup();
            let media: Vec<usize> = titles.iter().filter_map(|&t| catalog.titles[t].media.first().copied()).collect();
            if !media.is_empty() {
                catalog.rows.push(Row { name, media, titles: true });
            }
        }
        catalog
    }

    /// The title a media belongs to.
    pub fn title_for(&self, media: usize) -> Option<&Title> {
        self.title_of.get(media).and_then(|&t| self.titles.get(t))
    }

    pub fn title(&self, key: &str) -> Option<&Title> {
        self.titles.iter().find(|t| t.key == key)
    }

    /// Media whose title contains every word of `query`, ignoring case, by title and episode.
    pub fn search(&self, query: &str) -> Vec<usize> {
        const MAX: usize = 200;
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        if words.is_empty() {
            return Vec::new();
        }
        let mut found: Vec<usize> = (0..self.media.len())
            .filter(|&m| {
                let title = self.media[m].item.title.to_lowercase();
                words.iter().all(|w| title.contains(w.as_str()))
            })
            .collect();
        found.sort_by_key(|&m| {
            let item = &self.media[m].item;
            (item.title.to_lowercase(), item.season, item.episode, item.id)
        });
        found.truncate(MAX);
        found
    }
}
