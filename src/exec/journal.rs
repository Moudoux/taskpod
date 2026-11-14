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
