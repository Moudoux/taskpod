//! Files on the machine being configured: this one, or a remote one over SSH.

use super::sudo;
use crate::remote::{Remote, quote};
use crate::vars::Ctx;
use anyhow::{Context, Result, anyhow, bail};
use std::io::Write;
use std::path::Path;
use std::process::{ExitStatus, Stdio};

/// What's at a path.
#[derive(Debug, PartialEq)]
pub enum Current {
    Missing,
    /// It exists, but can't be read (permissions, or it's a directory).
    Unreadable,
    Content(Vec<u8>),
}

/// Read a file. With `sudo`, a file the user can't read is read as root.
pub fn read(ctx: &Ctx, task: &str, path: &Path, sudo: bool) -> Result<Current> {
    let current = match &ctx.remote {
        Some(remote) => read_remote(remote, path, false)?,
        None => match std::fs::read(path) {
            Ok(bytes) => Current::Content(bytes),
            Err(_) if path.exists() => Current::Unreadable,
            Err(_) => Current::Missing,
        },
    };
    if current != Current::Unreadable || !sudo || path.is_dir() && ctx.remote.is_none() {
        return Ok(current);
    }
    sudo::ensure(task, ctx)?;
    match &ctx.remote {
        Some(remote) => read_remote(remote, path, true),
        None => {
            let output = std::process::Command::new("sudo")
                .arg("cat")
                .arg("--")
                .arg(path)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .context("failed to run sudo")?;
            Ok(if output.status.success() {
                Current::Content(output.stdout)
            } else {
                Current::Unreadable
            })
        }
    }
}

fn read_remote(remote: &Remote, path: &Path, sudo: bool) -> Result<Current> {
    let p = quote(&path.to_string_lossy());
    let cat = if sudo { "sudo cat" } else { "cat" };
    let script = format!(
        "if [ -f {p} ]; then {cat} -- {p} 2>/dev/null || exit 3; \
         elif [ -e {p} ] || [ -L {p} ]; then exit 3; else exit 2; fi"
    );
    let output = remote.output(&script, &[])?;
    Ok(match output.status.code() {
        Some(0) => Current::Content(output.stdout),
        Some(2) => Current::Missing,
        Some(3) => Current::Unreadable,
        _ => bail!(
            "couldn't read {} on {}: {}",
            path.display(),
            remote.host,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    })
}

/// Permission bits and `user:group` of a file, if it exists.
pub fn stat(ctx: &Ctx, path: &Path) -> Option<(u32, String)> {
    let output = match &ctx.remote {
        Some(remote) => {
            let script = format!("stat -c '%a %U:%G' -- {}", quote(&path.to_string_lossy()));
            remote.output(&script, &[]).ok()?
        }
        None => {
            if !path.exists() {
                return None;
            }
            std::process::Command::new("stat")
                .args(["-c", "%a %U:%G"])
                .arg(path)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()?
        }
    };
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (mode, owner) = text.trim().split_once(' ')?;
    Some((u32::from_str_radix(mode, 8).ok()?, owner.to_string()))
}

/// Write a file with `mode`, creating parent directories. With `sudo` or
/// an owner it's written as root, which also sets the owner.
pub fn write(
    ctx: &Ctx,
    task: &str,
    path: &Path,
    content: &[u8],
    mode: u32,
    owner: Option<&str>,
    sudo: bool,
) -> Result<()> {
    if sudo || owner.is_some() {
        sudo::ensure(task, ctx)?;
    }
    match &ctx.remote {
        Some(remote) => write_remote(remote, path, content, mode, owner, sudo),
        None if sudo || owner.is_some() => write_with_sudo(path, content, mode, owner),
        None => write_local(path, content, mode),
    }
}

fn write_remote(
    remote: &Remote,
    path: &Path,
    content: &[u8],
    mode: u32,
    owner: Option<&str>,
    sudo: bool,
) -> Result<()> {
    let p = quote(&path.to_string_lossy());
    let script = if sudo || owner.is_some() {
        format!(
            "sudo {} /dev/stdin {p}",
            install_args(mode, owner).join(" ")
        )
    } else {
        // Like a local write: through symlinks, and never readable with
        // looser permissions than `mode`, even for a moment.
        format!(
            "mkdir -p -- \"$(dirname -- {p})\" || exit 1\n\
             if [ -e {p} ]; then chmod {mode:o} -- {p} || exit 1; fi\n\
             (umask 077 && cat > {p}) || exit 1\n\
             chmod {mode:o} -- {p}\n"
        )
    };
    let output = remote.output(&script, content)?;
    if !output.status.success() {
        bail!(
            "exited with {}: {}",
            describe(output.status),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// `install` arguments that create parent directories and set the mode and owner.
fn install_args(mode: u32, owner: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "install".into(),
        "-D".into(),
        "-m".into(),
        format!("{mode:o}"),
    ];
    if let Some(owner) = owner {
        let (user, group) = owner.split_once(':').unwrap_or((owner, ""));
        args.extend(["-o".into(), quote(user)]);
        if !group.is_empty() {
            args.extend(["-g".into(), quote(group)]);
        }
    }
    args
}

fn write_local(path: &Path, content: &[u8], mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        // Set the mode before writing, so content like secrets is never
        // readable with looser permissions, even for a moment.
        let permissions = || std::fs::Permissions::from_mode(mode);
        if path.exists() {
            std::fs::set_permissions(path, permissions())?;
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(path)?
            .write_all(content)?;
        // The umask may have removed bits from a new file.
        std::fs::set_permissions(path, permissions())?;
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        std::fs::write(path, content)?;
    }
    Ok(())
}

/// Write a file as root with `sudo install`, which also creates parent
/// directories and sets the owner.
fn write_with_sudo(path: &Path, content: &[u8], mode: u32, owner: Option<&str>) -> Result<()> {
    let mut command = std::process::Command::new("sudo");
    // Local arguments aren't parsed by a shell, so they're passed unquoted.
    command.args(["install", "-D", "-m", &format!("{mode:o}")]);
    if let Some(owner) = owner {
        let (user, group) = owner.split_once(':').unwrap_or((owner, ""));
        command.args(["-o", user]);
        if !group.is_empty() {
            command.args(["-g", group]);
        }
    }
    let mut child = command
        .arg("/dev/stdin")
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run sudo")?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("no stdin for sudo"))?
        .write_all(content)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "sudo install exited with {}: {}",
            describe(output.status),
            stderr.trim()
        );
    }
    Ok(())
}

pub fn describe(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("status {code}"),
        None => status.to_string(),
    }
}
