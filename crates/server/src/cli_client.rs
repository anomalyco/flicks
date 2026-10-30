//! Client commands (`flicks connect`, `ls`, `get`): this machine as a device of other servers,
//! through `flicks_client`, keeping `client.key`, `servers.json` and `catalog.json` in the Flicks
//! data directory.

use std::io::Write;
use std::path::PathBuf;

use flicks_client::merge::{self, Library, Source};
use flicks_client::{Client, Connection, Item, MAX_READ, Server};
use hyper::Method;

use crate::Error;

/// What this machine calls itself when it pairs.
const DEVICE_NAME: &str = "Flicks CLI";

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Pair this machine with a server, from the link `flicks pair` printed there.
    Connect { link: String },
    /// List the library merged from every server this machine is paired with: one line per
    /// title, with its sources (`*` marks the one to play).
    Ls {
        /// List each server's catalog on its own instead.
        #[arg(long)]
        per_server: bool,
    },
    /// Read a byte range of a file (ids from `flicks ls`) to a file or standard output.
    Get {
        id: i64,
        /// Inclusive byte offsets.
        #[arg(long, value_name = "START-END")]
        range: String,
        /// Where to write the bytes (default: standard output).
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// The server's name or key (needed when several are paired; see `flicks ls`).
        #[arg(long)]
        server: Option<String>,
    },
}

pub async fn run(command: Command) -> Result<(), Error> {
    let mut client = Client::open(&crate::data_dir()?)?;
    match command {
        Command::Connect { link } => {
            let server = client.pair(&link, DEVICE_NAME).await?;
            println!("Paired with {} as {} ({}).", server.name, server.user, role(&server));
        }
        Command::Ls { per_server } => {
            if client.servers.is_empty() {
                println!("No servers yet. Pair with one with `flicks connect <link>`.");
            } else if per_server {
                for server in client.servers.clone() {
                    ls(&mut client, &server).await;
                }
            } else {
                ls_merged(Library::open(client)).await?;
            }
        }
        Command::Get { id, range, out, server } => {
            let (start, end) = range
                .split_once('-')
                .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)))
                .filter(|(a, b)| a <= b)
                .ok_or(Error::Usage("--range is START-END, inclusive byte offsets"))?;
            let key = pick(&client.servers, server.as_deref())?.key.clone();
            let connection = client.connect(&key).await?;
            let mut out: Box<dyn Write> = match out {
                Some(path) => Box::new(std::fs::File::create(path)?),
                None => Box::new(std::io::stdout().lock()),
            };
            get(&connection, id, start, end, &mut out).await?;
        }
    }
    Ok(())
}

/// Writes bytes `start..=end` of file `id` to `out`, one request per [`MAX_READ`] bytes. The range
/// is clamped to the file's size, which `/items/{id}` gives first, so a range running past the end
/// stops there without asking for bytes that aren't there.
pub async fn get(connection: &Connection, id: i64, start: u64, end: u64, out: &mut dyn Write) -> Result<(), Error> {
    #[derive(serde::Deserialize)]
    struct Detail {
        item: Item,
    }
    let detail: Detail = connection.call(Method::GET, &format!("/items/{id}"), None::<&()>).await?;
    let Some(end) = detail.item.size.checked_sub(1).map(|last| end.min(last)).filter(|end| start <= *end) else {
        return Err(Error::Usage("--range starts past the end of the file"));
    };
    let mut at = start;
    while at <= end {
        let last = end.min(at.saturating_add(MAX_READ - 1));
        let bytes = connection.read(id, at, last).await?;
        out.write_all(&bytes)?;
        // Short: the file shrank since it was scanned.
        if (bytes.len() as u64) < last - at + 1 {
            break;
        }
        // `end` is below the file size, so this can't overflow.
        at = last + 1;
    }
    out.flush()?;
    Ok(())
}

async fn ls(client: &mut Client, server: &Server) {
    let entries = match client.connect(&server.key).await {
        Ok(connection) => connection.items().await.map(|items| (connection.addr, items)),
        Err(e) => Err(e),
    };
    // Connecting saves the name the server gives now, if it was renamed.
    let server = client.servers.iter().find(|s| s.key == server.key).unwrap_or(server);
    let header = format!("{} ({}, {})", server.name, server.user, role(server));
    match entries {
        Err(e) => println!("{header}: {e}"),
        Ok((addr, entries)) => {
            println!("{header} at {addr}: {} items", entries.len());
            for entry in entries {
                let item = entry.item;
                let mut title = item.title;
                if let Some(year) = item.year {
                    title.push_str(&format!(" ({year})"));
                }
                if let (Some(season), Some(episode)) = (item.season, item.episode) {
                    title.push_str(&format!(" S{season:02}E{episode:02}"));
                }
                let minutes = item.duration_ms.map(|ms| format!("{}m", ms / 60_000)).unwrap_or_default();
                println!("{:>6}  {title}  {}  {minutes}  {} bytes", item.id, item.container, item.size);
            }
        }
    }
}

async fn ls_merged(mut library: Library) -> Result<(), Error> {
    library.refresh().await?;
    for server in &library.client.servers {
        let header = format!("{} ({}, {})", server.name, server.user, role(server));
        match library.connection(&server.key) {
            Some(connection) => println!("{header} at {}", connection.addr),
            None => println!("{header}: offline, its titles are from the last catalog"),
        }
    }
    println!("{} titles", library.titles().len());
    for title in library.titles() {
        let item = &title.item;
        let mut name = item.title.clone();
        if let Some(year) = item.year {
            name.push_str(&format!(" ({year})"));
        }
        if let (Some(season), Some(episode)) = (item.season, item.episode) {
            name.push_str(&format!(" S{season:02}E{episode:02}"));
        }
        if let Some(minutes) = item.duration_ms.map(|ms| ms / 60_000) {
            name.push_str(&format!("  {minutes}m"));
        }
        let best = merge::best_source(title);
        let sources: Vec<String> =
            title.sources.iter().map(|s| source(s, best.is_some_and(|b| std::ptr::eq(b, s)))).collect();
        println!("{name}  {}", sources.join("  |  "));
    }
    Ok(())
}

/// `* Ryan's Mac #12 mp4 h264 1920x1080 1.2 GB`, `(offline)` when its server didn't answer.
fn source(source: &Source, best: bool) -> String {
    let item = &source.item;
    let mut line = format!("{}{} #{} {}", if best { "* " } else { "" }, source.server_name, item.id, item.container);
    if let Some(codec) = &item.video_codec {
        line.push_str(&format!(" {codec}"));
    }
    if let (Some(width), Some(height)) = (item.width, item.height) {
        line.push_str(&format!(" {width}x{height}"));
    }
    let size = if item.size >= 1_000_000_000 {
        format!("{:.1} GB", item.size as f64 / 1e9)
    } else {
        format!("{:.1} MB", item.size as f64 / 1e6)
    };
    line.push_str(&format!(" {size}"));
    if !source.available {
        line.push_str(" (offline)");
    }
    line
}

/// The server `--server` names, or the only one.
fn pick<'a>(servers: &'a [Server], name: Option<&str>) -> Result<&'a Server, Error> {
    match (name, servers) {
        (None, [server]) => Ok(server),
        (None, []) => Err(Error::Usage("no servers yet; pair with one with `flicks connect <link>`")),
        (None, _) => Err(Error::Usage("several servers are paired; say which with --server <name>")),
        (Some(name), _) => servers
            .iter()
            .find(|s| s.name == name || s.key == name)
            .ok_or(Error::Usage("no paired server has that name or key (see `flicks ls`)")),
    }
}

fn role(server: &Server) -> &'static str {
    if server.admin { "admin" } else { "viewer" }
}
