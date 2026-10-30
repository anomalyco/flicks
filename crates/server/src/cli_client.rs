//! Client commands (`flicks connect`, `ls`, `get`, `download`, `downloads`): this machine as a device of other servers,
//! through `flicks_client`, keeping `client.key`, `servers.json` and `catalog.json` in the Flicks
//! data directory.

use std::io::Write;
use std::path::PathBuf;

use flicks_client::download::{Download, Downloads, State};
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
    /// Download a file (id from `flicks ls`) to this machine, or continue an earlier attempt from
    /// the bytes already on disk.
    Download {
        id: i64,
        /// The server's name or key (needed when several are paired; see `flicks ls`).
        #[arg(long)]
        server: Option<String>,
    },
    /// List downloads and their state.
    Downloads {
        #[command(subcommand)]
        command: Option<DownloadsCommand>,
    },
}

#[derive(Debug, clap::Subcommand)]
pub enum DownloadsCommand {
    /// Cancel and delete a download.
    Rm {
        id: i64,
        /// The server's name or key (needed when several have that id).
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
        Command::Download { id, server } => download(&mut client, id, server.as_deref()).await?,
        Command::Downloads { command: None } => list_downloads(&Downloads::open(&crate::data_dir()?)?),
        Command::Downloads { command: Some(DownloadsCommand::Rm { id, server }) } => {
            let downloads = Downloads::open(&crate::data_dir()?)?;
            let mut found = downloads.list();
            found.retain(|d| d.item.id == id && server.as_ref().is_none_or(|s| d.server_name == *s || d.server == *s));
            let [found] = found.as_slice() else {
                return Err(Error::Usage("no download has that id, or several do (say which with --server)"));
            };
            downloads.remove(&found.server, id).await?;
            println!("Deleted {}.", describe(&found.item));
        }
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct Detail {
    item: Item,
}

/// Downloads file `id` and waits for it, showing progress. Ctrl-C leaves the partial file, which
/// running this again continues.
async fn download(client: &mut Client, id: i64, server: Option<&str>) -> Result<(), Error> {
    let server = pick(&client.servers, server)?.clone();
    let connection = client.connect(&server.key).await?;
    let detail: Detail = connection.call(Method::GET, &format!("/items/{id}"), None::<&()>).await?;
    let source = Source {
        server: server.key.clone(),
        server_name: server.name.clone(),
        item: detail.item,
        progress: None,
        available: true,
        latency: None,
    };
    let downloads = Downloads::open(&crate::data_dir()?)?;
    let started = std::time::Instant::now();
    downloads.start(&connection, &source)?;
    loop {
        let Some(now) = downloads.get(&server.key, id) else { return Err(Error::Usage("the download disappeared")) };
        match now.state {
            State::Complete => {
                let path = downloads.local_file(&server.key, id);
                println!(
                    "\r{} downloaded in {:.1?}: {}",
                    describe(&now.item),
                    started.elapsed(),
                    path.map_or_else(String::new, |p| p.display().to_string())
                );
                return Ok(());
            }
            State::Failed(why) => return Err(std::io::Error::other(why).into()),
            State::Paused => return Err(Error::Usage("the download stopped")),
            State::Downloading => {
                print!("\r{}  {}   ", describe(&now.item), progress(&now));
                std::io::stdout().flush()?;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

fn list_downloads(downloads: &Downloads) {
    let list = downloads.list();
    if list.is_empty() {
        println!("No downloads. Start one with `flicks download <id>` (ids from `flicks ls`).");
    }
    for d in list {
        let state = match &d.state {
            State::Downloading => format!("downloading {}", progress(&d)),
            State::Paused => format!("paused {}", progress(&d)),
            State::Complete => "complete".to_string(),
            State::Failed(why) => format!("failed ({why}) {}", progress(&d)),
        };
        println!("{:>6}  {}  {}  {state}", d.item.id, d.server_name, describe(&d.item));
    }
}

/// `12.3 / 456.7 MB (3%)`.
fn progress(d: &Download) -> String {
    let percent = (d.done * 100).checked_div(d.item.size).unwrap_or(100);
    format!("{:.1} / {:.1} MB ({percent}%)", d.done as f64 / 1e6, d.item.size as f64 / 1e6)
}

/// `Title (2020) S01E02`.
fn describe(item: &Item) -> String {
    let mut name = item.title.clone();
    if let Some(year) = item.year {
        name.push_str(&format!(" ({year})"));
    }
    if let (Some(season), Some(episode)) = (item.season, item.episode) {
        name.push_str(&format!(" S{season:02}E{episode:02}"));
    }
    name
}

/// Writes bytes `start..=end` of file `id` to `out`, one request per [`MAX_READ`] bytes. The range
/// is clamped to the file's size, which `/items/{id}` gives first, so a range running past the end
/// stops there without asking for bytes that aren't there.
pub async fn get(connection: &Connection, id: i64, start: u64, end: u64, out: &mut dyn Write) -> Result<(), Error> {
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
                let title = describe(&item);
                let minutes = item.duration_ms.map(|ms| format!("{}m", ms / 60_000)).unwrap_or_default();
                println!("{:>6}  {title}  {}  {minutes}  {} bytes", item.id, item.container, item.size);
            }
        }
    }
}

async fn ls_merged(mut library: Library) -> Result<(), Error> {
    library.refresh().await?;
    // Progress made offline on downloaded titles goes to the servers that answered.
    Downloads::open(&crate::data_dir()?)?.sync(&mut library).await?;
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
        let mut name = describe(item);
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
