//! What the screens show, from the merged library (`flicks_client::merge`): its titles (one
//! movie or episode each, across servers), grouped into movies and shows, and laid out as Home's
//! rows.

use std::collections::HashMap;

use flicks_client::Item;
use flicks_client::merge::{self, Key, Source, Title};

/// What a library tile and a title's page stand for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum GroupKey {
    /// A show's episodes: by TMDB id when matched, else by name.
    Show(String),
    Movie(Key),
}

/// A movie, or a show and its episodes.
pub struct Group {
    pub key: GroupKey,
    pub name: String,
    pub year: Option<u16>,
    /// Indices into [`Catalog::titles`], in season and episode order.
    pub titles: Vec<usize>,
}

impl Group {
    pub fn is_show(&self) -> bool {
        matches!(self.key, GroupKey::Show(_))
    }
}

pub struct Row {
    pub name: String,
    /// Indices into [`Catalog::titles`].
    pub titles: Vec<usize>,
    /// The tiles stand for whole groups (a movie or show), not single episodes.
    pub groups: bool,
}

#[derive(Default)]
pub struct Catalog {
    pub titles: Vec<Title>,
    pub groups: Vec<Group>,
    /// The group of each title.
    pub group_of: Vec<usize>,
    /// Home: Continue Watching, Recently Added, TV Shows, Movies.
    pub rows: Vec<Row>,
}

/// How many titles Recently Added shows.
const RECENT: usize = 30;

/// The most tiles in one Home row, until Rune virtualizes horizontal scrolling.
const ROW: usize = 60;

impl Catalog {
    pub fn build(titles: Vec<Title>) -> Catalog {
        let mut catalog = Catalog { titles, ..Catalog::default() };
        let mut group_at: HashMap<GroupKey, usize> = HashMap::new();
        for (at, title) in catalog.titles.iter().enumerate() {
            let key = group_key(title);
            let group = *group_at.entry(key.clone()).or_insert_with(|| {
                catalog.groups.push(Group {
                    key,
                    name: title.item.title.clone(),
                    year: title.item.year,
                    titles: Vec::new(),
                });
                catalog.groups.len() - 1
            });
            if let Some(g) = catalog.groups.get_mut(group) {
                g.titles.push(at);
            }
            catalog.group_of.push(group);
        }
        let titles = &catalog.titles;
        let item = |t: usize| titles.get(t).map(|t| &t.item);
        for group in &mut catalog.groups {
            group.titles.sort_by_key(|&t| item(t).map(|i| (i.season, i.episode, i.id)));
        }

        let mut watching: Vec<usize> =
            (0..titles.len()).filter(|&t| titles.get(t).and_then(watched).is_some()).collect();
        watching
            .sort_by_key(|&t| std::cmp::Reverse(titles.get(t).and_then(|t| t.progress).map_or(0, |p| p.updated_at)));
        let mut added: Vec<usize> = (0..titles.len()).collect();
        added.sort_by_key(|&t| std::cmp::Reverse(item(t).map_or(0, |i| i.added_at)));
        added.truncate(RECENT);
        let mut rows = vec![
            Row { name: "Continue Watching".into(), titles: watching, groups: false },
            Row { name: "Recently Added".into(), titles: added, groups: false },
        ];
        for (name, shows) in [("TV Shows", true), ("Movies", false)] {
            let mut groups: Vec<&Group> = catalog.groups.iter().filter(|g| g.is_show() == shows).collect();
            groups.sort_by_cached_key(|g| (g.name.to_lowercase(), g.year));
            let titles = groups.iter().filter_map(|g| g.titles.first().copied()).collect();
            rows.push(Row { name: name.into(), titles, groups: true });
        }
        rows.retain(|r| !r.titles.is_empty());
        for row in &mut rows {
            row.titles.truncate(ROW);
        }
        catalog.rows = rows;
        catalog
    }

    pub fn group(&self, key: &GroupKey) -> Option<&Group> {
        self.groups.iter().find(|g| g.key == *key)
    }

    /// The group a title belongs to.
    pub fn group_for(&self, title: usize) -> Option<&Group> {
        self.group_of.get(title).and_then(|&g| self.groups.get(g))
    }

    /// Whether any title of the group is on a server that answered.
    pub fn group_available(&self, group: &Group) -> bool {
        group.titles.iter().any(|&t| self.titles.get(t).is_some_and(Title::available))
    }

    /// Titles whose name contains every word of `query`, ignoring case, by name and episode.
    pub fn search(&self, query: &str) -> Vec<usize> {
        const MAX: usize = 200;
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        if words.is_empty() {
            return Vec::new();
        }
        let mut found: Vec<usize> = (0..self.titles.len())
            .filter(|&t| {
                let name = self.titles.get(t).map(|t| t.item.title.to_lowercase()).unwrap_or_default();
                words.iter().all(|w| name.contains(w.as_str()))
            })
            .collect();
        found.sort_by_cached_key(|&t| {
            self.titles.get(t).map(|t| (t.item.title.to_lowercase(), t.item.season, t.item.episode, t.item.id))
        });
        found.truncate(MAX);
        found
    }
}

fn group_key(title: &Title) -> GroupKey {
    let item = &title.item;
    let show = item.season.is_some() || item.episode.is_some() || item.tmdb_media.as_deref() == Some("tv");
    match (show, item.tmdb_id) {
        (true, Some(id)) => GroupKey::Show(format!("tmdb/{id}")),
        (true, None) => {
            GroupKey::Show(item.title.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect())
        }
        (false, _) => GroupKey::Movie(title.key.clone()),
    }
}

/// The source to play, else the first (whose artwork names are still the ones to show).
pub fn source(title: &Title) -> Option<&Source> {
    merge::best_source(title).or(title.sources.first())
}

/// "S1 · E2", or "S1 · E2–E3" for a multi-episode file.
pub fn episode_label(item: &Item) -> Option<String> {
    let episode = item.episode?;
    let episodes = match item.episode_end {
        Some(end) if end > episode => format!("E{episode}–E{end}"),
        _ => format!("E{episode}"),
    };
    Some(match item.season {
        Some(season) => format!("S{season} · {episodes}"),
        None => episodes,
    })
}

/// How far in, 0 to 1, when there's progress worth showing.
pub fn watched(title: &Title) -> Option<f32> {
    let progress = title.progress.filter(|p| !p.played && p.position > 0)?;
    let duration = title.item.duration_ms.filter(|d| *d > 0)?;
    Some((progress.position as f64 / duration as f64).clamp(0.0, 1.0) as f32)
}
