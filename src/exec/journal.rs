//! What files, lines and settings looked like before taskpod first changed
//! them, so `--undo` can put them back.
//!
//! The journal lives on the machine being configured, as two SQLite
//! databases with the same tables: changes made as the user in
//! `~/.local/state/taskpod/journal.db`, changes made as root (with
//! `sudo` or an `owner`) in `/var/lib/taskpod/journal.db`, mode 600 and
//! owned by root, since it may hold anything. Settings always go in the
//! user's.
//!
//! taskpod reads a database's file through [`target`] (as root, or over SSH,
//! like any other file), opens it in memory with the bundled SQLite, and
//! after each change writes the whole file back the same way: written beside
//! it, then renamed over it, so it's never half written. Nothing else is
//! left on the machine. Two taskpod runs on one machine at once can lose
//! each other's records: the last to write wins.
//!
//! Only the first change is recorded, so undo goes back to before taskpod,
//! not to the previous run.

use super::target::{self, Current};
use crate::vars::Ctx;
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, MAIN_DB, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq)]
pub enum Previous {
    /// There was no file.
    Missing,
    /// A file, or a directory (with no content).
    File {
        content: Vec<u8>,
        mode: u32,
        owner: String,
        /// Its modification time (nanoseconds since the epoch), put back
        /// with its content. Filled in by the journal.
        mtime: Option<i64>,
    },
}

/// What the journal knows about a file.
#[derive(Debug, PartialEq)]
pub enum Entry {
    None,
    /// The journal it would be in needs root to read (in a check without `--root`).
    Unreadable,
    Some(Previous),
}

/// The databases of one run, read on first use.
#[derive(Default)]
pub struct Journal {
    user: Mutex<Option<Connection>>,
    root: Mutex<Option<Connection>>,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Journal")
    }
}

/// The tables. `user_version` says which layout a database has.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS files (
    path TEXT PRIMARY KEY,
    -- 'missing' (taskpod created it), 'file' or 'dir'
    kind TEXT NOT NULL CHECK (kind IN ('missing', 'file', 'dir')),
    -- The old content; NULL for missing files and directories.
    content BLOB,
    sha256 TEXT,
    size INTEGER,
    mode INTEGER,
    -- user:group, as names (ids where there were none)
    owner TEXT,
    uid INTEGER,
    gid INTEGER,
    -- Nanoseconds since the epoch: modification, change, creation (if known).
    mtime INTEGER,
    ctime INTEGER,
    btime INTEGER,
    task TEXT NOT NULL,
    recorded_at INTEGER NOT NULL,
    taskpod_version TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS line_files (
    path TEXT PRIMARY KEY,
    -- Whether taskpod created the file, and from which base content.
    created INTEGER NOT NULL,
    base TEXT,
    task TEXT NOT NULL,
    recorded_at INTEGER NOT NULL,
    taskpod_version TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS lines (
    path TEXT NOT NULL REFERENCES line_files (path) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    -- The step's `match` prefix, or its text.
    key TEXT NOT NULL,
    -- The line as taskpod wrote it, and the one it replaced, if any.
    text TEXT NOT NULL,
    replaced TEXT,
    PRIMARY KEY (path, key)
);
CREATE TABLE IF NOT EXISTS settings (
    -- The setting's `get` command.
    get TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    task TEXT NOT NULL,
    recorded_at INTEGER NOT NULL,
    taskpod_version TEXT NOT NULL
);
"#;
const VERSION: i64 = 1;

/// Where a database lives.
fn location(ctx: &Ctx, root: bool) -> Result<PathBuf> {
    if let Some(dir) = &ctx.journal {
        return Ok(dir.join(if root { "root.db" } else { "user.db" }));
    }
    if root {
        return Ok(PathBuf::from("/var/lib/taskpod/journal.db"));
    }
    let home = ctx
        .home()
        .ok_or_else(|| anyhow!("no home directory for the undo journal"))?;
    Ok(PathBuf::from(home).join(".local/state/taskpod/journal.db"))
}

/// A database's file as an in-memory connection, with the tables made.
fn open(bytes: Option<Vec<u8>>, shown: &Path) -> Result<Connection> {
    let damaged = || format!("the undo journal {} is damaged", shown.display());
    let mut conn = Connection::open_in_memory()?;
    if let Some(bytes) = bytes.filter(|b| !b.is_empty()) {
        conn.deserialize_read_exact(MAIN_DB, &bytes[..], bytes.len(), false)
            .with_context(damaged)?;
    }
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .with_context(damaged)?;
    if version > VERSION {
        bail!(
            "the undo journal {} was written by a newer taskpod",
            shown.display()
        );
    }
    conn.execute_batch(SCHEMA).with_context(damaged)?;
    conn.execute_batch(&format!(
        "PRAGMA user_version = {VERSION}; PRAGMA foreign_keys = ON;"
    ))?;
    Ok(conn)
}

/// Run `f` on a database: the root one with `root`, read as root with
/// `sudo`. `None` if it needs root to read and `sudo` isn't given. With
/// `write`, the database is written back if `f` changed it.
fn with<T>(
    ctx: &Ctx,
    task: &str,
    root: bool,
    sudo: bool,
    write: bool,
    f: impl FnOnce(&Connection) -> Result<T>,
) -> Result<Option<T>> {
    let journal = &ctx.journals;
    let mut slot = if root { &journal.root } else { &journal.user }
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let path = location(ctx, root)?;
    if slot.is_none() {
        let bytes = match target::read(ctx, task, &path, sudo)? {
            Current::Content(bytes) => Some(bytes),
            Current::Missing => None,
            Current::Unreadable if write => {
                bail!("can't read the undo journal {}", path.display())
            }
            Current::Unreadable => return Ok(None),
        };
        *slot = Some(open(bytes, &path)?);
    }
    let conn = slot.as_mut().expect("opened above");
    let before = conn.total_changes();
    let value = f(conn)?;
    if !write || conn.total_changes() == before {
        return Ok(Some(value));
    }
    let saved = conn
        .serialize(MAIN_DB)
        .map(|data| data.to_vec())
        .map_err(anyhow::Error::from)
        .and_then(|bytes| {
            if root {
                target::write(ctx, task, &path, &bytes, 0o600, None, true)
            } else {
                target::replace(ctx, &path, &bytes, 0o600)
            }
        });
    if let Err(e) = saved {
        // What's in memory isn't what's on the machine: read it again next time.
        *slot = None;
        return Err(e.context(format!(
            "failed to write the undo journal {}",
            path.display()
        )));
    }
    Ok(Some(value))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

const TASKPOD_VERSION: &str = env!("CARGO_PKG_VERSION");

fn key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The entry for `path`. With `sudo`, the root journal is read as root.
pub fn read(ctx: &Ctx, task: &str, path: &Path, root: bool, sudo: bool) -> Result<Entry> {
    let row = with(ctx, task, root, sudo, false, |conn| {
        let row = conn
            .query_row(
                "SELECT kind, content, mode, owner, mtime FROM files WHERE path = ?1",
                [key(path)],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<Vec<u8>>>(1)?,
                        r.get::<_, Option<u32>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                    ))
                },
            )
            .optional()?;
        Ok(row)
    })?;
    let Some(row) = row else {
        return Ok(Entry::Unreadable);
    };
    let Some((kind, content, mode, owner, mtime)) = row else {
        return Ok(Entry::None);
    };
    if kind == "missing" {
        return Ok(Entry::Some(Previous::Missing));
    }
    let mode = mode.ok_or_else(|| {
        anyhow!(
            "the undo journal's entry for {} has no mode",
            path.display()
        )
    })?;
    Ok(Entry::Some(Previous::File {
        content: content.unwrap_or_default(),
        mode,
        owner: owner.unwrap_or_default(),
        // A directory's changes with what's in it.
        mtime: if kind == "file" { mtime } else { None },
    }))
}

/// Record what `path` was before taskpod writes it, unless it's recorded
/// already, with everything `stat` says about it.
pub fn record(ctx: &Ctx, task: &str, path: &Path, root: bool, previous: &Previous) -> Result<()> {
    with(ctx, task, root, root, true, |conn| {
        let known: Option<i64> = conn
            .query_row("SELECT 1 FROM files WHERE path = ?1", [key(path)], |r| {
                r.get(0)
            })
            .optional()?;
        if known.is_some() {
            return Ok(());
        }
        let (kind, content, mode, owner, details) = match previous {
            Previous::Missing => ("missing", None, None, None, None),
            Previous::File {
                content,
                mode,
                owner,
                ..
            } => {
                let details = target::details(ctx, task, path, root)?;
                let dir = details.as_ref().is_some_and(|d| d.dir);
                let content = (!dir).then_some(content.as_slice());
                let kind = if dir { "dir" } else { "file" };
                (kind, content, Some(*mode), Some(owner.as_str()), details)
            }
        };
        let details = details.unwrap_or_default();
        let has = kind != "missing";
        conn.execute(
            "INSERT INTO files (path, kind, content, sha256, size, mode, owner, uid, gid,
                 mtime, ctime, btime, task, recorded_at, taskpod_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                key(path),
                kind,
                content,
                content.map(super::step::sha256_hex),
                content.map(|c| c.len() as i64),
                mode,
                owner,
                has.then_some(details.uid),
                has.then_some(details.gid),
                details.mtime,
                details.ctime,
                details.btime,
                task,
                now(),
                TASKPOD_VERSION,
            ],
        )?;
        Ok(())
    })
    .with_context(|| format!("failed to record {} in the undo journal", path.display()))?;
    Ok(())
}

/// Forget `path`, once it's undone.
pub fn remove(ctx: &Ctx, task: &str, path: &Path, root: bool) -> Result<()> {
    with(ctx, task, root, root, true, |conn| {
        conn.execute("DELETE FROM files WHERE path = ?1", [key(path)])?;
        Ok(())
    })?;
    Ok(())
}

/// What taskpod did to a file with `line` steps, one entry per file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Lines {
    /// Taskpod created the file (from its `base`, if it had one).
    pub created: bool,
    /// The base's content when taskpod created the file.
    pub base: Option<String>,
    pub lines: Vec<LineRecord>,
}

/// One line taskpod wrote.
#[derive(Debug, Clone, PartialEq)]
pub struct LineRecord {
    /// Which step it belongs to: the step's `match` prefix, or its text.
    pub key: String,
    /// The line as taskpod wrote it.
    pub text: String,
    /// The line it replaced, if it replaced one.
    pub replaced: Option<String>,
}

impl Lines {
    pub fn get(&self, key: &str) -> Option<&LineRecord> {
        self.lines.iter().find(|l| l.key == key)
    }
}

/// The line entry for `path`: `None` if it can't be read (the root journal,
/// in a check without `--root`). With `sudo`, the root journal is read as root.
pub fn read_lines(
    ctx: &Ctx,
    task: &str,
    path: &Path,
    root: bool,
    sudo: bool,
) -> Result<Option<Lines>> {
    with(ctx, task, root, sudo, false, |conn| {
        let file: Option<(bool, Option<String>)> = conn
            .query_row(
                "SELECT created, base FROM line_files WHERE path = ?1",
                [key(path)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((created, base)) = file else {
            return Ok(Lines::default());
        };
        let mut query = conn
            .prepare("SELECT key, text, replaced FROM lines WHERE path = ?1 ORDER BY position")?;
        let lines = query
            .query_map([key(path)], |r| {
                Ok(LineRecord {
                    key: r.get(0)?,
                    text: r.get(1)?,
                    replaced: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Lines {
            created,
            base,
            lines,
        })
    })
}

/// Save the line entry for `path`, or remove it once it has no lines.
pub fn write_lines(ctx: &Ctx, task: &str, path: &Path, root: bool, lines: &Lines) -> Result<()> {
    with(ctx, task, root, root, true, |conn| {
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM line_files WHERE path = ?1", [key(path)])?;
        if !lines.lines.is_empty() {
            tx.execute(
                "INSERT INTO line_files (path, created, base, task, recorded_at, taskpod_version)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    key(path),
                    lines.created,
                    lines.base,
                    task,
                    now(),
                    TASKPOD_VERSION
                ],
            )?;
            for (position, line) in lines.lines.iter().enumerate() {
                tx.execute(
                    "INSERT INTO lines (path, position, key, text, replaced)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        key(path),
                        position as i64,
                        line.key,
                        line.text,
                        line.replaced
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    })
    .with_context(|| {
        format!(
            "failed to record {}'s lines in the undo journal",
            path.display()
        )
    })?;
    Ok(())
}

/// The value a setting had before taskpod first set it, if recorded. Kept
/// in the user's journal: it's the output of a command the user can run.
pub fn read_setting(ctx: &Ctx, task: &str, get: &str) -> Result<Option<String>> {
    let value = with(ctx, task, false, false, false, |conn| {
        Ok(conn
            .query_row("SELECT value FROM settings WHERE get = ?1", [get], |r| {
                r.get(0)
            })
            .optional()?)
    })?;
    value.ok_or_else(|| anyhow!("can't read the undo journal"))
}

/// Record a setting's value before taskpod sets it, unless it's recorded already.
pub fn record_setting(ctx: &Ctx, task: &str, get: &str, value: &str) -> Result<()> {
    with(ctx, task, false, false, true, |conn| {
        conn.execute(
            "INSERT OR IGNORE INTO settings (get, value, task, recorded_at, taskpod_version)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![get, value, task, now(), TASKPOD_VERSION],
        )?;
        Ok(())
    })
    .with_context(|| format!("failed to record `{get}` in the undo journal"))?;
    Ok(())
}

/// Forget a setting, once it's undone.
pub fn remove_setting(ctx: &Ctx, task: &str, get: &str) -> Result<()> {
    with(ctx, task, false, false, true, |conn| {
        conn.execute("DELETE FROM settings WHERE get = ?1", [get])?;
        Ok(())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn databases_survive_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path().to_path_buf());
        let path = Path::new("/etc/a b.conf");
        let previous = Previous::File {
            content: b"old\n".to_vec(),
            mode: 0o640,
            owner: "me:me".into(),
            mtime: None,
        };
        record(&ctx, "t", path, false, &previous).unwrap();
        record_setting(&ctx, "t", "get x", "1").unwrap();
        let lines = Lines {
            created: true,
            base: Some("base\n".into()),
            lines: vec![
                LineRecord {
                    key: "b".into(),
                    text: "b=1".into(),
                    replaced: None,
                },
                LineRecord {
                    key: "a".into(),
                    text: "a=1".into(),
                    replaced: Some("a=0".into()),
                },
            ],
        };
        write_lines(&ctx, "t", path, false, &lines).unwrap();
        // Only the database is left, with nothing beside it.
        let names: Vec<String> = std::fs::read_dir(dir.path().join(".taskpod-journal"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["user.db"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let db = dir.path().join(".taskpod-journal/user.db");
            let mode = std::fs::metadata(db).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // A new run reads it back from the file.
        let ctx = Ctx::new(dir.path().to_path_buf());
        assert_eq!(
            read(&ctx, "t", path, false, false).unwrap(),
            Entry::Some(previous.clone())
        );
        // The first record stays.
        let later = Previous::File {
            content: b"newer\n".to_vec(),
            mode: 0o600,
            owner: "me:me".into(),
            mtime: None,
        };
        record(&ctx, "t", path, false, &later).unwrap();
        assert_eq!(
            read(&ctx, "t", path, false, false).unwrap(),
            Entry::Some(previous)
        );
        assert_eq!(
            read_lines(&ctx, "t", path, false, false).unwrap(),
            Some(lines)
        );
        assert_eq!(
            read_setting(&ctx, "t", "get x").unwrap().as_deref(),
            Some("1")
        );
        record_setting(&ctx, "t", "get x", "2").unwrap();
        assert_eq!(
            read_setting(&ctx, "t", "get x").unwrap().as_deref(),
            Some("1")
        );

        remove(&ctx, "t", path, false).unwrap();
        remove_setting(&ctx, "t", "get x").unwrap();
        write_lines(&ctx, "t", path, false, &Lines::default()).unwrap();
        let ctx = Ctx::new(dir.path().to_path_buf());
        assert_eq!(read(&ctx, "t", path, false, false).unwrap(), Entry::None);
        assert_eq!(read_setting(&ctx, "t", "get x").unwrap(), None);
        assert_eq!(
            read_lines(&ctx, "t", path, false, false).unwrap(),
            Some(Lines::default())
        );
    }

    #[test]
    fn a_damaged_journal_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path().to_path_buf());
        std::fs::create_dir_all(dir.path().join(".taskpod-journal")).unwrap();
        std::fs::write(dir.path().join(".taskpod-journal/user.db"), "not sqlite").unwrap();
        let err = read(&ctx, "t", Path::new("/x"), false, false).unwrap_err();
        assert!(format!("{err:#}").contains("is damaged"), "{err:#}");
    }
}
