//! What files looked like before taskpod first wrote them, so `--undo` can
//! put them back.
//!
//! The journal lives on the machine being configured: files written as the
//! user in `~/.local/state/taskpod/journal`, files written as root in
//! `/var/lib/taskpod/journal`, readable by root only, since they may hold
//! anything. Each entry is mode 600: a line of JSON, then the old content.
//! Only the first write is recorded, so undo goes back to before taskpod,
//! not to the previous run.

use super::target::{self, Current};
use crate::vars::Ctx;
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum Previous {
    /// There was no file.
    Missing,
    File {
        content: Vec<u8>,
        mode: u32,
        owner: String,
    },
}

/// What the journal knows about a file.
#[derive(Debug, PartialEq)]
pub enum Entry {
    None,
    /// There's an entry, but reading it needs root (in a check).
    Unreadable,
    Some(Previous),
}

fn dir(ctx: &Ctx, root: bool) -> Result<PathBuf> {
    if let Some(dir) = &ctx.journal {
        return Ok(dir.join(if root { "root" } else { "user" }));
    }
    if root {
        return Ok(PathBuf::from("/var/lib/taskpod/journal"));
    }
    let home = ctx
        .home()
        .ok_or_else(|| anyhow!("no home directory for the undo journal"))?;
    Ok(PathBuf::from(home).join(".local/state/taskpod/journal"))
}

/// The entry's file: the path with everything but letters, digits, `.`, `-`
/// and `_` written as `%XX`, so names stay readable and unique.
fn entry(ctx: &Ctx, path: &Path, root: bool) -> Result<PathBuf> {
    let mut name = String::new();
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_') {
            name.push(byte as char);
        } else {
            name.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(dir(ctx, root)?.join(name))
}

/// Whether there's an entry for `path`.
pub fn exists(ctx: &Ctx, path: &Path, root: bool) -> Result<bool> {
    Ok(target::stat(ctx, &entry(ctx, path, root)?).is_some())
}

/// The entry for `path`. With `sudo`, a root entry is read as root.
pub fn read(ctx: &Ctx, task: &str, path: &Path, root: bool, sudo: bool) -> Result<Entry> {
    let file = entry(ctx, path, root)?;
    let bytes = match target::read(ctx, task, &file, sudo)? {
        Current::Content(bytes) => bytes,
        Current::Missing => return Ok(Entry::None),
        Current::Unreadable => return Ok(Entry::Unreadable),
    };
    let split = bytes
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(bytes.len());
    let header: Value = serde_json::from_slice(&bytes[..split])
        .with_context(|| format!("the journal entry {} is damaged", file.display()))?;
    if header["missing"] == true {
        return Ok(Entry::Some(Previous::Missing));
    }
    let mode = header["mode"]
        .as_u64()
        .and_then(|m| u32::try_from(m).ok())
        .ok_or_else(|| anyhow!("the journal entry {} has no mode", file.display()))?;
    Ok(Entry::Some(Previous::File {
        content: bytes.get(split + 1..).unwrap_or_default().to_vec(),
        mode,
        owner: header["owner"].as_str().unwrap_or_default().to_string(),
    }))
}

/// Record what `path` was before taskpod writes it, unless it's recorded already.
pub fn record(ctx: &Ctx, task: &str, path: &Path, root: bool, previous: &Previous) -> Result<()> {
    if exists(ctx, path, root)? {
        return Ok(());
    }
    let shown = path.display().to_string();
    let mut bytes = match previous {
        Previous::Missing => json!({ "path": shown, "missing": true }),
        Previous::File { mode, owner, .. } => {
            json!({ "path": shown, "mode": mode, "owner": owner })
        }
    }
    .to_string()
    .into_bytes();
    bytes.push(b'\n');
    if let Previous::File { content, .. } = previous {
        bytes.extend_from_slice(content);
    }
    // Written as root (by sudo's `install`), root owns it.
    let file = entry(ctx, path, root)?;
    target::write(ctx, task, &file, &bytes, 0o600, None, root)
        .with_context(|| format!("failed to record {shown} in the undo journal"))
}

/// Forget `path`, once it's undone.
pub fn remove(ctx: &Ctx, task: &str, path: &Path, root: bool) -> Result<()> {
    target::remove(ctx, task, &entry(ctx, path, root)?, root)
}

/// What taskpod did to a file with `line` steps, kept next to the file
/// entries, one per file.
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

/// The line entry's file. A file entry's name never has `%` followed by
/// something other than two hex digits, so `%lines` can't clash with one.
fn lines_entry(ctx: &Ctx, path: &Path, root: bool) -> Result<PathBuf> {
    let file = entry(ctx, path, root)?;
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    Ok(file.with_file_name(format!("{name}%lines")))
}

/// The line entry for `path`: `None` if it can't be read (a root entry, in a
/// check). With `sudo`, a root entry is read as root.
pub fn read_lines(
    ctx: &Ctx,
    task: &str,
    path: &Path,
    root: bool,
    sudo: bool,
) -> Result<Option<Lines>> {
    let file = lines_entry(ctx, path, root)?;
    let bytes = match target::read(ctx, task, &file, sudo)? {
        Current::Content(bytes) => bytes,
        Current::Missing => return Ok(Some(Lines::default())),
        Current::Unreadable => return Ok(None),
    };
    let damaged = || anyhow!("the journal entry {} is damaged", file.display());
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| damaged())?;
    let text = |v: &Value| v.as_str().map(String::from);
    let lines = value["lines"]
        .as_array()
        .ok_or_else(damaged)?
        .iter()
        .map(|l| {
            Some(LineRecord {
                key: text(&l["key"])?,
                text: text(&l["text"])?,
                replaced: text(&l["replaced"]),
            })
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(damaged)?;
    Ok(Some(Lines {
        created: value["created"] == true,
        base: text(&value["base"]),
        lines,
    }))
}

/// Save the line entry for `path`, or remove it once it has no lines.
pub fn write_lines(ctx: &Ctx, task: &str, path: &Path, root: bool, lines: &Lines) -> Result<()> {
    let file = lines_entry(ctx, path, root)?;
    if lines.lines.is_empty() {
        if target::stat(ctx, &file).is_none() {
            return Ok(());
        }
        return target::remove(ctx, task, &file, root);
    }
    let records: Vec<Value> = lines
        .lines
        .iter()
        .map(|l| json!({ "key": l.key, "text": l.text, "replaced": l.replaced }))
        .collect();
    let value = json!({
        "path": path.display().to_string(),
        "created": lines.created,
        "base": lines.base,
        "lines": records,
    });
    let shown = path.display();
    target::write(
        ctx,
        task,
        &file,
        value.to_string().as_bytes(),
        0o600,
        None,
        root,
    )
    .with_context(|| format!("failed to record {shown}'s lines in the undo journal"))
}
