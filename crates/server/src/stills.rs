//! Local fallback artwork. A separate, serial worker follows catalog publication; each decode
//! runs in a disposable low-priority process, so a stuck or crashing codec cannot stop serving.

mod decode;

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, SyncSender, TryRecvError};
use std::time::{Duration, Instant};

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

const DEADLINE: Duration = Duration::from_secs(5);

/// Local names deliberately cannot alias TMDB's size-prefixed cache entries.
pub(crate) fn valid_name(name: &str) -> bool {
    name.strip_prefix("still-v2-")
        .or_else(|| name.strip_prefix("still-"))
        .and_then(|s| s.strip_suffix(".jpg"))
        .is_some_and(|id| id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub(crate) fn start(dir: &Path) -> io::Result<SyncSender<()>> {
    let dir = dir.to_path_buf();
    let exe = std::env::current_exe()?;
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new().name("flicks-stills".into()).spawn(move || {
        let run = || -> Result<(), crate::Error> {
            let conn = Connection::open(dir.join(crate::DB_FILE))?;
            conn.busy_timeout(Duration::from_secs(5))?;
            let artwork = dir.join("artwork");
            fs::create_dir_all(&artwork)?;
            let mut pending = false;
            loop {
                if !pending && rx.recv().is_err() {
                    break;
                }
                pending = false;
                let ids = {
                    let mut stmt = conn.prepare(
                        "SELECT f.id FROM library_files f JOIN folders ON folders.path=f.root
                        WHERE f.video_codec IS NOT NULL AND (f.still IS NULL OR f.still LIKE 'still-%') ORDER BY f.id",
                    )?;
                    stmt.query_map([], |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?
                };
                for id in ids {
                    match rx.try_recv() {
                        Err(TryRecvError::Disconnected) => return Ok(()),
                        Ok(()) => pending = true,
                        Err(TryRecvError::Empty) => {}
                    }
                    let Some(item) = flicks_library::item(&conn, id).map_err(io::Error::other)? else { continue };
                    if item.still.as_deref().is_some_and(|name| !valid_name(name)) {
                        continue;
                    }
                    let name = format!("still-v2-{}.jpg", item.oshash);
                    if !valid_name(&name) {
                        continue;
                    }
                    let output = artwork.join(&name);
                    let skipped = output.with_extension("skip");
                    if !output.is_file() && !skipped.is_file() {
                        let keys = flicks_library::keyframes(&conn, id).map_err(io::Error::other)?;
                        let duration = item.duration_ms.unwrap_or(0);
                        let times: Vec<_> = [20, 25, 15, 30]
                            .into_iter()
                            .filter_map(|percent| {
                                let target = duration.saturating_mul(percent) / 100;
                                keys.iter().rev().find(|k| u64::from(k.time_ms) <= target).map(|k| k.time_ms)
                            })
                            .collect();
                        let job =
                            Job { root: item.root, path: item.path, size: item.size, times, output: output.clone() };
                        let begin = Instant::now();
                        let result = generate(&exe, &job);
                        eprintln!(
                            "still {}: {} ms ({})",
                            item.id,
                            begin.elapsed().as_millis(),
                            if result.is_ok() { "saved" } else { "skipped" }
                        );
                        if result.is_err() {
                            // Negative cache: don't repeatedly spend the budget on an unchanged bad file.
                            fs::write(&skipped, [])?;
                        }
                    }
                    if output.is_file() {
                        conn.execute(
                            "UPDATE library_files SET still=?1,
                            backdrop=CASE WHEN backdrop IS NULL OR backdrop LIKE 'still-%' THEN ?1 ELSE backdrop END
                            WHERE id=?2 AND oshash=?3 AND (still IS NULL OR still LIKE 'still-%')
                            AND EXISTS(SELECT 1 FROM folders WHERE folders.path=library_files.root)",
                            params![name, id, item.oshash],
                        )?;
                    }
                }
                clean(&conn, &artwork)?;
            }
            Ok(())
        };
        if let Err(error) = run() {
            eprintln!("stills worker: {error}");
        }
    })?;
    Ok(tx)
}

fn clean(conn: &Connection, dir: &Path) -> Result<(), crate::Error> {
    let mut stmt =
        conn.prepare("SELECT oshash, still, backdrop FROM library_files f JOIN folders ON folders.path=f.root")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?))
    })?;
    let (mut live, mut images) = (HashSet::new(), HashSet::new());
    for row in rows {
        let (hash, still, backdrop) = row?;
        live.insert(hash);
        images.extend(still);
        images.extend(backdrop);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((stem, extension)) = name.rsplit_once('.') else { continue };
        if !matches!(extension, "jpg" | "skip" | "part") {
            continue;
        }
        let Some(hash) = stem.strip_prefix("still-v2-").or_else(|| stem.strip_prefix("still-")) else { continue };
        if valid_name(&format!("{stem}.jpg"))
            && (!live.contains(hash) || (extension == "jpg" && !images.contains(&name)))
        {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Job {
    root: String,
    path: String,
    size: u64,
    times: Vec<u32>,
    output: PathBuf,
}

fn generate(exe: &Path, job: &Job) -> io::Result<()> {
    if job.times.is_empty() {
        return Err(io::Error::other("no indexed video"));
    }
    let input = serde_json::to_vec(job).map_err(io::Error::other)?;
    // Fits in an empty pipe even if exec fails; writing cannot bypass the wait deadline.
    if input.len() > 4096 {
        return Err(io::Error::other("still job too large"));
    }
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("nice");
        command.args(["-n", "19"]).arg(exe);
        command
    };
    #[cfg(not(unix))]
    let mut command = Command::new(exe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x00004000); // BELOW_NORMAL_PRIORITY_CLASS
    }
    let mut child = command
        .arg("--still-worker")
        .env_remove("FLICKS_TMDB_KEY")
        .env("RAYON_NUM_THREADS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let result = (|| {
        let mut stdin = child.stdin.take().ok_or_else(|| io::Error::other("worker stdin"))?;
        stdin.write_all(&input)?;
        drop(stdin);
        let begin = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                return if status.success() { Ok(()) } else { Err(io::Error::other("still decode refused")) };
            }
            if begin.elapsed() >= DEADLINE {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "still deadline"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = fs::remove_file(job.output.with_extension("part"));
    result
}

/// Private binary entry point; stdin is a bounded job from the catalog worker, not a route.
pub fn worker() -> io::Result<()> {
    let job: Job = serde_json::from_reader(io::stdin().take(64 * 1024)).map_err(io::Error::other)?;
    if job.times.len() > 4 {
        return Err(io::Error::other("too many candidates"));
    }
    let path = flicks_library::resolve(Path::new(&job.root), Path::new(&job.path))
        .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "media outside library"))?;
    let file = fs::File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() != job.size || meta.len() > 128 * 1024 * 1024 * 1024 {
        return Err(io::Error::other("media changed or exceeds size budget"));
    }
    let rgb = decode::frame(file, &job.times)?;
    let temp = job.output.with_extension("part");
    let mut jpeg = fs::File::create(&temp)?;
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85)
        .encode(rgb.as_raw(), rgb.width(), rgb.height(), image::ExtendedColorType::Rgb8)
        .map_err(io::Error::other)?;
    drop(jpeg);
    fs::rename(temp, job.output)
}
