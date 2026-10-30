//! Client commands (`flicks connect`, `ls`, `get`): this machine as a device of other servers,
//! through `flicks_client`, keeping `client.key` and `servers.json` in the Flicks data directory.

use std::io::Write;
use std::path::PathBuf;

use flicks_client::{Client, MAX_READ, Server};

use crate::Error;

/// What this machine calls itself when it pairs.
const DEVICE_NAME: &str = "Flicks CLI";

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Pair this machine with a server, from the link `flicks pair` printed there.
    Connect { link: String },
    /// List the catalog of every server this machine is paired with.
    Ls,
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
    let mut client = Client::open(&crate::data_dir()?).map_err(client_error)?;
    match command {
        Command::Connect { link } => {
            let server = client.pair(&link, DEVICE_NAME).await.map_err(client_error)?;
            println!("Paired with {} as {} ({}).", server.name, server.user, role(&server));
        }
        Command::Ls => {
            if client.servers.is_empty() {
                println!("No servers yet. Pair with one with `flicks connect <link>`.");
            }
            for server in client.servers.clone() {
                ls(&mut client, &server).await;
            }
        }
        Command::Get { id, range, out, server } => {
            let (start, end) = range
                .split_once('-')
                .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)))
                .filter(|(a, b)| a <= b)
                .ok_or(Error::Usage("--range is START-END, inclusive byte offsets"))?;
            let key = pick(&client.servers, server.as_deref())?.key.clone();
            let connection = client.connect(&key).await.map_err(client_error)?;
            let mut out: Box<dyn Write> = match out {
                Some(path) => Box::new(std::fs::File::create(path)?),
                None => Box::new(std::io::stdout().lock()),
            };
            let mut at = start;
            // One request per MAX_READ bytes; stops early at the end of the file.
            while at <= end {
                let last = end.min(at.saturating_add(MAX_READ - 1));
                let bytes = connection.read(id, at, last).await.map_err(client_error)?;
                out.write_all(&bytes)?;
                if bytes.is_empty() || last == u64::MAX || (bytes.len() as u64) < last - at + 1 {
                    break;
                }
                at = last + 1;
            }
            out.flush()?;
        }
    }
    Ok(())
}

async fn ls(client: &mut Client, server: &Server) {
    let header = format!("{} ({}, {})", server.name, server.user, role(server));
    let entries = match client.connect(&server.key).await {
        Ok(connection) => connection.items().await.map(|items| (connection.addr, items)),
        Err(e) => Err(e),
    };
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

fn client_error(error: flicks_client::Error) -> Error {
    Error::Io(std::io::Error::other(error))
}
