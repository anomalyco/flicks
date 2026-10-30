//! The SQLite database: one connection, owned by one thread, which every read and write goes
//! through, so there is a single writer. Migrations run on open, tracked by `user_version`.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::{Error, keys, unix_now};

/// Each entry moves the schema up one version. Never edit one that has shipped; add another.
const MIGRATIONS: &[&str] = &["
    CREATE TABLE users (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL UNIQUE,
        admin INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE devices (
        id INTEGER PRIMARY KEY,
        key BLOB NOT NULL UNIQUE,
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        name TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE folders (
        id INTEGER PRIMARY KEY,
        path TEXT NOT NULL UNIQUE,
        created_at INTEGER NOT NULL
    );
"];

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// How many jobs may wait for the database thread; past that, callers wait for room.
const QUEUE: usize = 256;

/// A handle to the database thread.
///
/// Other crates may keep their own tables in this database through [`Db::call`], but must never
/// read or write `PRAGMA user_version`: it tracks this crate's migrations alone.
#[derive(Clone)]
pub struct Db {
    jobs: mpsc::Sender<Job>,
}

impl Db {
    /// Opens (or creates) the database, migrates it, and starts its thread.
    pub fn open(path: &Path) -> Result<Db, Error> {
        #[cfg(windows)]
        {
            // Secure an existing database and sidecars before SQLite reads or writes them.
            let file = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(path)?;
            flicks_platform::permissions::owner_only(path)?;
            drop(file);
            for suffix in ["-wal", "-shm", "-journal"] {
                let mut sidecar = path.as_os_str().to_os_string();
                sidecar.push(suffix);
                let sidecar = Path::new(&sidecar);
                if sidecar.try_exists()? {
                    flicks_platform::permissions::owner_only(sidecar)?;
                }
            }
        }
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        migrate(&mut conn)?;
        let (jobs, mut rx) = mpsc::channel::<Job>(QUEUE);
        std::thread::Builder::new().name("flicks-db".into()).spawn(move || {
            while let Some(job) = rx.blocking_recv() {
                job(&mut conn);
            }
        })?;
        Ok(Db { jobs })
    }

    /// Runs `f` on the database thread.
    pub async fn call<T, F>(&self, f: F) -> Result<T, Error>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, Error> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.jobs
            .send(Box::new(move |conn| {
                let _ = tx.send(f(conn));
            }))
            .await
            .map_err(|_| Error::DbClosed)?;
        rx.await.map_err(|_| Error::DbClosed)?
    }
}

fn migrate(conn: &mut Connection) -> Result<(), Error> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let known = i64::try_from(MIGRATIONS.len()).unwrap_or(i64::MAX);
    if version > known {
        return Err(Error::NewerDb(version));
    }
    for (done, sql) in (1..).zip(MIGRATIONS).skip(usize::try_from(version).unwrap_or(0)) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", done)?;
        tx.commit()?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub admin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: i64,
    pub user: String,
    pub name: String,
    /// The device's public key, base64url.
    pub key: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folder {
    pub id: i64,
    pub path: String,
}

/// Who a signing device belongs to.
#[derive(Debug, Clone)]
pub struct DeviceOwner {
    pub device: i64,
    pub user: i64,
    pub admin: bool,
}

fn now() -> i64 {
    i64::try_from(unix_now()).unwrap_or(i64::MAX)
}

/// Adds a user; `None` if the name is taken.
pub fn add_user(conn: &Connection, name: &str, admin: bool) -> Result<Option<User>, Error> {
    let added = conn.execute(
        "INSERT INTO users (name, admin, created_at) VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
        params![name, admin, now()],
    )?;
    if added == 0 {
        return Ok(None);
    }
    Ok(Some(User { id: conn.last_insert_rowid(), name: name.to_string(), admin }))
}

/// Finds a user by name, creating it if missing; `admin` makes it an admin.
pub fn ensure_user(conn: &Connection, name: &str, admin: bool) -> Result<User, Error> {
    conn.execute(
        "INSERT INTO users (name, admin, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (name) DO UPDATE SET admin = admin OR excluded.admin",
        params![name, admin, now()],
    )?;
    Ok(conn.query_row("SELECT id, name, admin FROM users WHERE name = ?1", [name], |row| {
        Ok(User { id: row.get(0)?, name: row.get(1)?, admin: row.get(2)? })
    })?)
}

pub fn users(conn: &Connection) -> Result<Vec<User>, Error> {
    let mut stmt = conn.prepare("SELECT id, name, admin FROM users ORDER BY name")?;
    let rows = stmt.query_map([], |row| Ok(User { id: row.get(0)?, name: row.get(1)?, admin: row.get(2)? }))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Adds a device under a user. A key that is already paired moves to this user.
pub fn add_device(conn: &Connection, user: i64, key: &[u8], name: &str) -> Result<i64, Error> {
    Ok(conn.query_row(
        "INSERT INTO devices (key, user_id, name, created_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (key) DO UPDATE SET user_id = excluded.user_id, name = excluded.name
         RETURNING id",
        params![key, user, name, now()],
        |row| row.get(0),
    )?)
}

/// The device with this public key, if it is paired.
pub fn device_owner(conn: &Connection, key: &[u8]) -> Result<Option<DeviceOwner>, Error> {
    Ok(conn
        .query_row(
            "SELECT devices.id, users.id, users.admin FROM devices
             JOIN users ON users.id = devices.user_id WHERE devices.key = ?1",
            [key],
            |row| Ok(DeviceOwner { device: row.get(0)?, user: row.get(1)?, admin: row.get(2)? }),
        )
        .optional()?)
}

pub fn devices(conn: &Connection) -> Result<Vec<Device>, Error> {
    let mut stmt = conn.prepare(
        "SELECT devices.id, users.name, devices.name, devices.key, devices.created_at
         FROM devices JOIN users ON users.id = devices.user_id ORDER BY devices.id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Device {
            id: row.get(0)?,
            user: row.get(1)?,
            name: row.get(2)?,
            key: keys::encode(&row.get::<_, Vec<u8>>(3)?),
            created_at: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Removes a device; false if there was none with this id.
pub fn remove_device(conn: &Connection, id: i64) -> Result<bool, Error> {
    Ok(conn.execute("DELETE FROM devices WHERE id = ?1", [id])? > 0)
}

/// Adds a folder; `None` if it is already there.
pub fn add_folder(conn: &Connection, path: &str) -> Result<Option<Folder>, Error> {
    let added = conn.execute(
        "INSERT INTO folders (path, created_at) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
        params![path, now()],
    )?;
    if added == 0 {
        return Ok(None);
    }
    Ok(Some(Folder { id: conn.last_insert_rowid(), path: path.to_string() }))
}

pub fn folders(conn: &Connection) -> Result<Vec<Folder>, Error> {
    let mut stmt = conn.prepare("SELECT id, path FROM folders ORDER BY path")?;
    let rows = stmt.query_map([], |row| Ok(Folder { id: row.get(0)?, path: row.get(1)? }))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Removes a folder; false if there was none with this id.
pub fn remove_folder(conn: &Connection, id: i64) -> Result<bool, Error> {
    Ok(conn.execute("DELETE FROM folders WHERE id = ?1", [id])? > 0)
}

/// Numbers of users, devices and folders, for `flicks status`.
pub fn counts(conn: &Connection) -> Result<[i64; 3], Error> {
    Ok(conn.query_row(
        "SELECT (SELECT count(*) FROM users), (SELECT count(*) FROM devices),
                (SELECT count(*) FROM folders)",
        [],
        |row| Ok([row.get(0)?, row.get(1)?, row.get(2)?]),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_once_and_refuses_newer_schemas() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();
        assert!(add_user(&conn, "ana", false).unwrap().is_some());
        assert!(add_user(&conn, "ana", true).unwrap().is_none());
        conn.pragma_update(None, "user_version", 99).unwrap();
        assert!(matches!(migrate(&mut conn), Err(Error::NewerDb(99))));
    }
}
