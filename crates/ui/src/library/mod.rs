//! The library screens: Home (a featured title above rows of tiles), a title's page and search.

pub mod catalog;
pub mod fixture;

use rune::{
    Color, Cursor, Cx, El, ScrollPos, ScrollSpec, Shape, SurfaceFit, TextStyle, Transition, col, el, row, spacer, text,
    text_input,
};

use crate::{Flicks, Screen, Status, artwork_key, hairline, message, style};
use catalog::{Catalog, episode_label, source, watched};
use flicks_client::merge::Title;

pub fn home(s: &Flicks, cx: &mut Cx<Flicks>) -> El {
    match &s.status {
        Status::Loading => return message("Loading your library…", ""),
        Status::Failed(error) => return message("Something went wrong", error),
        Status::NoServers => return message("No servers yet", "Add one to see its movies and shows."),
        Status::Ready if s.catalog.rows.is_empty() => {
            return message("Nothing here yet", "Add a folder of movies or shows on your server.");
        }
        Status::Ready => {}
    }
    let mut rows = col().w_full().gap(28.0).pt(40.0);
    for (i, r) in s.catalog.rows.iter().enumerate() {
        rows = rows.child(shelf(s, cx, i, &r.name, &r.titles, r.groups));
    }
    scrolling(col().w_full().pb(56.0).children(hero(s)).child(rows))
}

/// A screen's content in a vertical scroll view.
fn scrolling(content: El) -> El {
    col().w_full().h_full().scroll(ScrollSpec { stick_bottom: false }).child(content)
}

/// What Home features: the newest title in progress, else the newest added.
pub(crate) fn featured(catalog: &Catalog) -> Option<usize> {
    let row = |name: &str| catalog.rows.iter().find(|r| r.name == name).and_then(|r| r.titles.first().copied());
    row("Continue Watching").or_else(|| row("Recently Added"))
}

/// The featured title: its backdrop as a band across the window, then its name, facts and
/// buttons on the page below.
fn hero(s: &Flicks) -> Option<El> {
    let t = featured(&s.catalog)?;
    let (title, group) = (s.catalog.titles.get(t)?, s.catalog.group_for(t)?);
    let offline = s.offline(t);
    let progress = watched(title);

    let mut facts = Vec::new();
    facts.extend(episode_label(&title.item));
    facts.extend(group.year.map(|y| y.to_string()));
    facts.extend(title.item.duration_ms.map(duration));
    facts.extend(quality(&title.item));
    let mut info = col()
        .key(format!("hero-info-{t}"))
        .transition(Transition::opacity())
        .max_w(620.0)
        .gap(14.0)
        .child(text(group.name.clone()).key("hero-title").font(style::DISPLAY).color(style::text()).max_lines(2))
        .child(text(facts.join("  ·  ")).font(style::META).color(style::muted()));
    if let (Some(done), Some(ms)) = (progress, title.item.duration_ms) {
        let left = ((1.0 - done as f64) * ms as f64) as u64;
        info = info.child(
            row()
                .gap(12.0)
                .center_y()
                .child(progress_bar(done).w(180.0))
                .child(text(format!("{} left", duration(left))).font(style::SMALL).color(style::muted())),
        );
    }
    let key = group.key.clone();
    let more = row()
        .key("more-info")
        .h(46.0)
        .px(22.0)
        .center_y()
        .radius(10.0)
        .fill(style::glass())
        .hover(Color::WHITE.alpha(0.12))
        .press(Color::WHITE.alpha(0.2))
        .cursor(Cursor::Pointer)
        .focusable(true)
        .focus_ring(2.0, 3.0, style::focus())
        .label("More Info")
        .child(text("More Info").font(style::BUTTON).color(style::text()))
        .on_press(move |s: &mut Flicks| s.go(Screen::Detail(key.clone())));
    let mut buttons = row().gap(12.0).mt(10.0);
    if offline {
        buttons = buttons.child(text("Offline: its server didn't answer.").font(style::BODY).color(style::muted()));
    } else {
        buttons = buttons.child(play_button(if progress.is_some() { "Resume" } else { "Play" }, t));
    }
    info = info.child(buttons.child(more));
    Some(below_band(band(s, title, "hero-band", format!("hero-art-{t}")), info).key("hero"))
}

/// A title's backdrop as a full-width band, keyed `key` (the top bar watches it), or none when
/// the title has no artwork. Until the image arrives the band keeps its place, empty.
fn band(s: &Flicks, title: &Title, key: &str, art: String) -> Option<El> {
    let item = &source(title)?.item;
    item.backdrop.as_ref().or(item.poster.as_ref())?;
    let image = artwork(s, title, true).map(|image| {
        el().key(art).transition(Transition::opacity()).w_full().h_full().pixels_fit(image.pixels(), SurfaceFit::Fill)
    });
    Some(el().key(key).w_full().h(style::BAND_H).children(image))
}

/// The band, then `info` on the page under it (or under the top bar, with no band).
fn below_band(band: Option<El>, info: El) -> El {
    let top = if band.is_some() { 28.0 } else { style::BAR_H + 28.0 };
    col().w_full().children(band).child(col().w_full().px(style::GUTTER).pt(top).child(info))
}

/// A row of tiles that scrolls sideways, with, for a mouse, arrows that page it while the row is
/// hovered.
fn shelf(s: &Flicks, cx: &mut Cx<Flicks>, i: usize, name: &str, titles: &[usize], whole_groups: bool) -> El {
    let wide = name == "Continue Watching";
    let tiles: Vec<El> = titles.iter().map(|&t| tile(s, t, &format!("r{i}"), whole_groups, wide)).collect();
    let (w, h) = if wide { (style::STILL_W, style::STILL_H) } else { (style::POSTER_W, style::POSTER_H) };
    let key = format!("row-{i}");
    let scroll = format!("row-{i}-scroll");
    // Room above and below the tiles for a lifted tile.
    let (above, below) = (16.0, 8.0);
    let row_x = s.row_x.clone();
    let at = key.clone();
    let strip = row()
        .key(scroll.clone())
        .w_full()
        .scroll_x()
        .on_scroll_offset(move |p: ScrollPos| {
            row_x.borrow_mut().insert(at.clone(), p.offset[0]);
        })
        .child(row().gap(style::TILE_GAP).px(style::GUTTER).pt(above).pb(below).focus_group().children(tiles));
    let width = cx.layout_size(&scroll).map_or(0.0, |[w, _]| w);
    let content = titles.len() as f32 * (w + style::TILE_GAP) - style::TILE_GAP + 2.0 * style::GUTTER;
    let mut shelf = el().w_full().child(strip);
    if content > width + 1.0 {
        let top = above + h / 2.0 - 22.0;
        let page = (width - 2.0 * style::GUTTER).max(w);
        shelf = shelf.child(arrow(&key, &scroll, -page, top)).child(arrow(&key, &scroll, page, top));
    }
    col()
        .key(key)
        .w_full()
        .hover_group()
        .child(text(name.to_string()).font(style::HEADING).color(style::text()).px(style::GUTTER))
        .child(shelf)
}

/// A row's arrow: pages it left (`by` < 0) or right.
fn arrow(row_key: &str, scroll: &str, by: f32, top: f32) -> El {
    let left = by < 0.0;
    let points =
        if left { vec![[7.0, 1.0], [1.0, 8.0], [7.0, 15.0]] } else { vec![[1.0, 1.0], [7.0, 8.0], [1.0, 15.0]] };
    let chevron = el().size(9.0, 16.0).shape(Shape::new().stroke(points, 2.4, Color::WHITE).view_box(8.0, 16.0));
    let (row_key, scroll) = (row_key.to_string(), scroll.to_string());
    let edge = Some(10.0);
    el().key(format!("{row_key}-{}", if left { "back" } else { "next" }))
        .pin(if left { edge } else { None }, Some(top), if left { None } else { edge }, None)
        .size(44.0, 44.0)
        .radius(22.0)
        .center()
        .fill(style::bg().alpha(0.72))
        .border(1.0, Color::WHITE.alpha(0.12))
        .hover(Color::WHITE.alpha(0.14))
        .group_opacity(0.0, 1.0)
        .cursor(Cursor::Pointer)
        .label(if left { "Back" } else { "More" })
        .child(chevron)
        .on_press_tx(move |s: &mut Flicks, tx| {
            let x = s.row_x.borrow().get(&row_key).copied().unwrap_or(0.0);
            tx.scroll_to(scroll.clone(), (x + by).max(0.0), 0.0);
        })
}

/// The decoded artwork for a title's poster (or backdrop), once it has arrived.
fn artwork<'a>(s: &'a Flicks, title: &Title, backdrop: bool) -> Option<&'a rune::Image> {
    let source = source(title)?;
    let name = if backdrop {
        source.item.backdrop.as_ref().or(source.item.poster.as_ref())
    } else {
        source.item.poster.as_ref()
    };
    s.artwork.get(&artwork_key(&source.server, name?))
}

/// Per-item artwork stays distinct from the series hero.
fn still<'a>(s: &'a Flicks, title: &Title) -> Option<&'a rune::Image> {
    let source = source(title)?;
    let name = source.item.still.as_ref().or(source.item.backdrop.as_ref()).or(source.item.poster.as_ref())?;
    s.artwork.get(&artwork_key(&source.server, name))
}

/// A poster (or, in Continue Watching, a 16:9 still), keyed `title-<title>-<place>`, with the
/// name and one line under it: the episode, the episode count or the year. A tile in a TV Shows
/// or Movies row stands for its whole group. Hovered, it lifts. A title none of whose servers
/// answered is dimmed.
fn tile(s: &Flicks, t: usize, place: &str, whole_group: bool, wide: bool) -> El {
    let (Some(title), Some(group)) = (s.catalog.titles.get(t), s.catalog.group_for(t)) else { return el() };
    let offline = if whole_group { s.refreshed && !s.catalog.group_available(group) } else { s.offline(t) };
    let progress = watched(title);
    let sub = match (whole_group, episode_label(&title.item)) {
        _ if offline => "Offline".to_string(),
        (false, Some(episode)) => match (progress, title.item.duration_ms) {
            (Some(done), Some(ms)) if wide => {
                format!("{episode}  ·  {} left", duration(((1.0 - done as f64) * ms as f64) as u64))
            }
            _ => episode,
        },
        _ if group.is_show() => episodes(group.titles.len()),
        _ => group.year.map(|y| y.to_string()).unwrap_or_default(),
    };
    let (w, h) = if wide { (style::STILL_W, style::STILL_H) } else { (style::POSTER_W, style::POSTER_H) };
    let image = if wide { still(s, title) } else { artwork(s, title, false) };
    let art = match image {
        Some(image) => el().pixels_fit(image.pixels(), SurfaceFit::Fill),
        None => blank(),
    };

    // A title can show in several rows, so its key names the row too.
    let key = format!("title-{t}-{place}");
    let lifted = s.lifted.as_deref() == Some(key.as_str());
    let hover = key.clone();
    let open = group.key.clone();
    let mut art = art
        .key(key)
        .size(w, h)
        .radius(style::ART_RADIUS)
        .clip_rounded(style::ART_RADIUS)
        .scale(if lifted { style::LIFT } else { 1.0 })
        .scale_motion(style::LIFT_SPRING)
        .label(format!("{}, {sub}", group.name))
        .cursor(Cursor::Pointer)
        .focusable(true)
        .focus_ring(3.0, 4.0, style::focus())
        .press_scale(0.97)
        .on_hover_change(move |s: &mut Flicks, on, _| {
            if on {
                s.lifted = Some(hover.clone());
            } else if s.lifted.as_deref() == Some(hover.as_str()) {
                s.lifted = None;
            }
        })
        .on_press(move |s: &mut Flicks| s.go(Screen::Detail(open.clone())));
    if lifted {
        art = art.raise();
    }
    // Under the image, clear of it lifted: how far in, then the name and the line.
    let mut tile = col().opacity(if offline { 0.4 } else { 1.0 }).w(w).child(art);
    if let Some(done) = progress {
        tile = tile.child(progress_bar(done).mt(12.0));
    }
    tile.child(text(group.name.clone()).font(style::NAME).color(style::text()).truncate().mt(12.0))
        .child(text(sub).font(style::SMALL).color(style::muted()).truncate().mt(4.0))
}

fn episodes(n: usize) -> String {
    if n == 1 { "1 episode".into() } else { format!("{n} episodes") }
}

/// How far in a title is: a thin flat bar.
fn progress_bar(watched: f32) -> El {
    el().w_full()
        .h(3.0)
        .radius(1.5)
        .fill(style::faint())
        .child(el().w_frac(watched).h_full().radius(1.5).fill(style::accent()))
}

/// What stands in for a title's artwork: a flat dark surface.
fn blank() -> El {
    el().fill(style::raised())
}

pub fn detail(s: &Flicks, key: &catalog::GroupKey) -> El {
    let Some(group) = s.catalog.group(key) else {
        return message("Not in your library", "It may have been removed from its server.");
    };
    let titles: Vec<(usize, &Title)> =
        group.titles.iter().filter_map(|&t| s.catalog.titles.get(t).map(|title| (t, title))).collect();
    let Some(&(first, lead)) = titles.first() else { return message("Not in your library", "") };
    // Resume the episode in progress, else start at the first that can play.
    let (next, next_title) = titles
        .iter()
        .copied()
        .find(|(_, title)| watched(title).is_some())
        .or_else(|| titles.iter().copied().find(|(t, _)| !s.offline(*t)))
        .unwrap_or((first, lead));

    let mut facts = Vec::new();
    facts.extend(group.year.map(|y| y.to_string()));
    if group.is_show() {
        facts.push(episodes(group.titles.len()));
    } else if let Some(ms) = lead.item.duration_ms {
        facts.push(duration(ms));
    }
    facts.extend(quality(&lead.item));

    let offline = s.offline(next);
    let play_label = if watched(next_title).is_some() { "Resume" } else { "Play" };
    let play_label = match (group.is_show(), episode_label(&next_title.item)) {
        (true, Some(episode)) => format!("{play_label}  {episode}"),
        _ => play_label.to_string(),
    };
    let mut info = col()
        .key("detail-info")
        .max_w(680.0)
        .gap(14.0)
        .child(text(group.name.clone()).key("detail-title").font(style::DISPLAY).color(style::text()).wrap())
        .child(text(facts.join("  ·  ")).font(style::META).color(style::muted()));
    info = if offline {
        info.child(
            text("Offline: its server didn't answer.").key("offline").font(style::BODY).color(style::muted()).mt(8.0),
        )
    } else {
        info.child(play_button(&play_label, next).mt(10.0))
    };

    let mut page = col().w_full().pb(56.0).child(below_band(band(s, lead, "detail-band", "detail-art".into()), info));
    if group.is_show() {
        let mut seasons: Vec<Option<u16>> = titles.iter().map(|(_, t)| t.item.season).collect();
        seasons.dedup();
        let season = s.season.filter(|n| seasons.contains(&Some(*n))).or(next_title.item.season);
        let mut list = col().key("episodes").w_full().px(style::GUTTER).pt(40.0);
        let heading = match seasons.as_slice() {
            [only] => text(only.map_or("Episodes".to_string(), |n| format!("Season {n}")))
                .font(style::HEADING)
                .color(style::text())
                .mb(6.0),
            _ => row()
                .gap(6.0)
                .ml(-16.0)
                .mb(6.0)
                .focus_group()
                .children(seasons.iter().map(|&n| season_tab(n, n == season))),
        };
        list = list.child(heading);
        for (i, &(t, title)) in
            titles.iter().filter(|(_, title)| seasons.len() < 2 || title.item.season == season).enumerate()
        {
            if i > 0 {
                list = list.child(hairline());
            }
            list = list.child(episode_row(s, title, t, s.offline(t)));
        }
        page = page.child(list);
    }
    scrolling(page)
}

fn season_tab(season: Option<u16>, on: bool) -> El {
    let label = season.map_or("Episodes".to_string(), |n| format!("Season {n}"));
    text(label.clone())
        .key(format!("season-{}", season.unwrap_or(0)))
        .font(style::NAV)
        .color(if on { Color::BLACK } else { style::text() })
        .fill(if on { Color::WHITE } else { style::glass() })
        .py(8.0)
        .px(16.0)
        .radius(18.0)
        .hover(style::faint())
        .cursor(Cursor::Pointer)
        .focusable(true)
        .label(label)
        .on_press(move |s: &mut Flicks| s.season = season)
}

fn play_button(label: &str, media: usize) -> El {
    row()
        .key("play")
        .h(46.0)
        .px(26.0)
        .gap(10.0)
        .center_y()
        .self_start()
        .radius(10.0)
        .fill(style::text())
        .hover(Color::BLACK.alpha(0.08))
        .press(Color::BLACK.alpha(0.15))
        .cursor(Cursor::Pointer)
        .focusable(true)
        .focus_ring(2.0, 3.0, style::focus())
        .label(label.to_string())
        .child(play_icon(style::bg(), 14.0))
        .child(text(label.to_string()).font(style::BUTTON).color(style::bg()))
        .on_press(move |s: &mut Flicks| s.open_player(media))
}

fn play_icon(color: Color, size: f32) -> El {
    let triangle = rune::Shape::new().fill(vec![[0.0, 0.0], [10.0, 6.0], [0.0, 12.0]], color).view_box(10.0, 12.0);
    el().size(size * 10.0 / 12.0, size).shape(triangle)
}

/// An episode: a thumbnail, its name and facts, and how far in it is.
fn episode_row(s: &Flicks, title: &Title, t: usize, offline: bool) -> El {
    let item = &title.item;
    let name = match (item.episode, item.episode_end) {
        (Some(first), Some(last)) if last > first => format!("Episodes {first}–{last}"),
        (Some(episode), _) => format!("Episode {episode}"),
        _ => item.title.clone(),
    };
    let mut facts = Vec::new();
    if offline {
        facts.push("Offline".to_string());
    }
    facts.extend(episode_label(item));
    facts.extend(item.duration_ms.map(duration));
    facts.extend(quality(item));
    let (w, h) = (176.0, 99.0);
    let mut thumb = match still(s, title) {
        Some(image) => el().pixels_fit(image.pixels(), SurfaceFit::Fill),
        None => blank(),
    };
    thumb = thumb.size(w, h).radius(style::ART_RADIUS).clip_rounded(style::ART_RADIUS).center();
    if !offline {
        // A play glyph shows on the thumbnail while the row is hovered.
        thumb = thumb.child(
            el().size(40.0, 40.0)
                .radius(20.0)
                .center()
                .fill(Color::BLACK.alpha(0.45))
                .group_opacity(0.0, 1.0)
                .child(play_icon(Color::WHITE, 14.0).ml(3.0)),
        );
    }
    let mut middle = col()
        .grow(1.0)
        .gap(6.0)
        .child(text(name).font(TextStyle::new(16.0, 650.0)).color(style::text()))
        .child(text(facts.join("  ·  ")).font(style::SMALL).color(style::muted()));
    if let Some(watched) = watched(title) {
        middle = middle.child(progress_bar(watched).w(160.0).mt(4.0));
    }
    let row = row()
        .key(format!("episode-{t}"))
        .opacity(if offline { 0.4 } else { 1.0 })
        .w_full()
        .py(14.0)
        .gap(20.0)
        .center_y()
        .hover_group()
        .child(thumb)
        .child(middle)
        .child(spacer());
    if offline {
        return row;
    }
    row.cursor(Cursor::Pointer)
        .focusable(true)
        .focus_ring(2.0, 0.0, style::focus())
        .on_press(move |s: &mut Flicks| s.open_player(t))
}

fn duration(ms: u64) -> String {
    let minutes = ms / 60_000;
    if minutes >= 60 { format!("{}h {}m", minutes / 60, minutes % 60) } else { format!("{minutes}m") }
}

/// "4K", "HDR" and the like.
fn quality(item: &flicks_client::Item) -> Vec<String> {
    let mut facts = Vec::new();
    match item.height {
        Some(h) if h >= 2000 => facts.push("4K".to_string()),
        Some(h) if h >= 1000 => facts.push("HD".to_string()),
        _ => {}
    }
    if item.hdr.is_some() {
        facts.push("HDR".into());
    }
    facts
}

pub fn search(s: &Flicks) -> El {
    let field = text_input(&s.query, TextStyle::new(28.0, 700.0).lh(36.0).tracking(-0.5))
        .key("search-field")
        .single_line()
        .w_full()
        .h(68.0)
        .px(24.0)
        .py(16.0)
        .radius(14.0)
        .fill(style::raised())
        .border(1.0, style::faint())
        .color(style::text())
        .placeholder("Movies and shows", style::muted().alpha(0.6))
        .on_edit(|s: &mut Flicks| &mut s.query)
        .on_input_change(|s: &mut Flicks, _| s.search_changed());
    let results = if s.query.text.trim().is_empty() {
        text("Type to search every server's movies and shows.").font(style::BODY).color(style::muted())
    } else if s.results.is_empty() {
        text(format!("Nothing matches \u{201c}{}\u{201d}.", s.query.text.trim()))
            .font(style::BODY)
            .color(style::muted())
    } else {
        // Each result springs in as it starts to match, and fades out as it stops.
        let tiles = s.results.iter().map(|&t| {
            tile(s, t, "q", false, false)
                .key(format!("result-{t}"))
                .transition(Transition::opacity().combined(Transition::scale(0.9)))
        });
        row().key("results").w_full().gaps(28.0, style::TILE_GAP).flex_wrap().children(tiles)
    };
    let content = col()
        .w_full()
        .px(style::GUTTER)
        .pt(20.0)
        .pb(56.0)
        .gap(32.0)
        .child(field)
        .child(results.key("search-results").transition(Transition::opacity()));
    scrolling(content)
}
