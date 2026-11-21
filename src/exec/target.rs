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

/// Whether an error says there's nothing at the path: it, or a directory
/// on the way to it, is missing. Anything else (permissions, like a
/// directory on the way that the user can't enter) means there may be.
fn gone(e: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
}

/// A shell function for a path the scripts below didn't find: `absent "$1"`
/// returns 2 when nothing is there, or 3 when a directory on the way can't
/// be searched (`/var/lib/x` of mode 750), so there may be. It looks for the
/// nearest directory above it that can be seen.
macro_rules! absent {
    () => {
        r#"absent() {
  p=$1
  while d=$(dirname -- "$p"); do
    if [ -d "$d" ]; then
      if [ -x "$d" ]; then return 2; fi
      return 3
    fi
    if [ "$d" = "$p" ]; then return 2; fi
    p=$d
  done
  return 2
}
"#
    };
}

/// Prints `$1`, or exits with 2 if there's nothing there, 3 if it can't be read.
const READ: &str = concat!(
    absent!(),
    r#"if [ -f "$1" ]; then cat -- "$1" 2>/dev/null || exit 3
elif [ -e "$1" ] || [ -L "$1" ]; then exit 3
else absent "$1"; fi"#
);

/// Read a file. With `sudo`, a file the user can't read, or can't see (in
/// a directory only root can enter), is read as root.
pub fn read(ctx: &Ctx, task: &str, path: &Path, sudo: bool) -> Result<Current> {
    let current = match &ctx.remote {
        Some(_) => read_script(ctx, path, false)?,
        None => match std::fs::read(path) {
            Ok(bytes) => Current::Content(bytes),
            Err(e) if gone(&e) => Current::Missing,
            Err(_) => Current::Unreadable,
        },
    };
    if current != Current::Unreadable || !sudo || path.is_dir() && ctx.remote.is_none() {
        return Ok(current);
    }
    sudo::ensure(task, ctx)?;
    read_script(ctx, path, true)
}

/// [`READ`] on the machine being configured, as root with `sudo`.
fn read_script(ctx: &Ctx, path: &Path, sudo: bool) -> Result<Current> {
    let output = script_output(ctx, READ, &[arg(path)], sudo)?;
    Ok(match output.status.code() {
        Some(0) => Current::Content(output.stdout),
        Some(2) => Current::Missing,
        Some(3) => Current::Unreadable,
        _ => bail!(
            "couldn't read {}{}: {}",
            path.display(),
            match &ctx.remote {
                Some(remote) => format!(" on {}", remote.host),
                None => String::new(),
            },
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    })
}

/// Lists the files right in each directory after `$1` (relative to it),
/// through symlinks. Exits with 1 when it can't, and with nothing when
/// there's no directory `$1`.
const LIST: &str = concat!(
    absent!(),
    r#"if [ ! -d "$1" ]; then absent "$1"; [ $? = 2 ] && exit 0; exit 1; fi
cd -- "$1" && shift
for d; do
  if [ -d "$d" ]; then find -L "$d" -mindepth 1 -maxdepth 1 -type f || exit 1; fi
done"#
);

/// The files under a directory (through symlinks), as paths relative to it
/// joined with `/`, sorted; empty if there's no directory. With `sudo`, a
/// directory the user can't list (or see) is listed as root.
pub fn list(
    ctx: &Ctx,
    task: &str,
    dir: &Path,
    subdirs: &[String],
    sudo: bool,
) -> Result<Vec<String>> {
    // Only the files right in each subdirectory: not what's in others, like
    // an app's data next to its config.
    let mut args = vec![arg(dir)];
    args.extend(
        subdirs
            .iter()
            .map(|d| if d.is_empty() { "." } else { d.as_str() }.to_string()),
    );
    let run = |root: bool| script_output(ctx, LIST, &args, root).context("failed to list files");
    let mut output = run(false)?;
    if !output.status.success() && sudo {
        sudo::ensure(task, ctx)?;
        output = run(true)?;
    }
    if !output.status.success() {
        bail!("couldn't list the files in {}", dir.display());
    }
    let mut names: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.strip_prefix("./").unwrap_or(l))
        .map(String::from)
        .collect();
    names.sort();
    Ok(names)
}

/// Prints the octal mode and `user:group` of `$1`, with GNU (Linux, busybox)
/// or BSD (macOS) `stat`. Through symlinks, since writes go through them too.
/// Exits with 2 if there's nothing there, 3 if it can't be seen.
const STAT: &str = concat!(
    absent!(),
    r#"if [ ! -e "$1" ]; then absent "$1"; exit; fi
if stat -c %a / >/dev/null 2>&1; then
  stat -L -c '%a %U:%G' -- "$1"
else
  stat -L -f '%Mp%Lp %Su:%Sg' -- "$1"
fi"#
);

/// Run as root: writes stdin to `$1` with mode `$2`, passing the remaining
/// arguments (`-o user -g group`) to `install`, after creating parent
/// directories. BSD `install` has no `-D` and can't read `/dev/stdin`, so the
/// content goes through a private temporary file first. `install` replaces
/// a symlink rather than writing through it, so a symlink is resolved first,
/// like other writes and `stat` do.
///
/// `install` unlinks an existing destination before creating it again, and
/// a program watching the directory (opensnitchd's rules) can react to the
/// file going missing. So it installs to a dotted temporary file, with no
/// extension a watcher would load, in the target's own directory, and
/// renames that over the target: the target is never missing.
const INSTALL: &str = r#"p=$1 mode=$2; shift 2
if [ -L "$p" ]; then p=$(readlink -f -- "$p") || exit 1; fi
if [ -d "$p" ]; then echo "$p is a directory" >&2; exit 1; fi
d=$(dirname -- "$p")
mkdir -p -- "$d" || exit 1
t= n=
trap 'rm -f ${t:+"$t"} ${n:+"$n"}' EXIT
t=$(umask 077 && mktemp "${TMPDIR:-/tmp}/taskpod.XXXXXX") || exit 1
cat > "$t" || exit 1
n=$(umask 077 && mktemp "$d/.taskpod.XXXXXX") || exit 1
install -m "$mode" "$@" "$t" "$n" || exit 1
mv -f "$n" "$p" || exit 1
n="#;

/// A path as a command argument: one starting with `-` gets a `./`, since
/// not every tool (BSD `chmod`) takes `--`.
fn arg(path: &Path) -> String {
    let path = path.to_string_lossy();
    if path.starts_with('-') {
        format!("./{path}")
    } else {
        path.into_owned()
    }
}

/// `install` arguments that set the owner.
fn owner_args(owner: Option<&str>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(owner) = owner {
        let (user, group) = owner.split_once(':').unwrap_or((owner, ""));
        args.extend(["-o".into(), user.into()]);
        if !group.is_empty() {
            args.extend(["-g".into(), group.into()]);
        }
    }
    args
}

/// What [`stat`] found.
enum Stat<T = (u32, String)> {
    Found(T),
    Missing,
    /// Not seen: in a directory the user can't enter.
    Unseen,
}

/// Permission bits and `user:group` of a file, if it exists. With `sudo`,
/// one the user can't see (in a directory only root can enter) is looked at
/// as root.
pub fn stat(ctx: &Ctx, task: &str, path: &Path, sudo: bool) -> Result<Option<(u32, String)>> {
    let seen = match &ctx.remote {
        #[cfg(unix)]
        None => match stat_local(path) {
            Ok(found) => Stat::Found(found),
            Err(e) if gone(&e) => Stat::Missing,
            Err(_) => Stat::Unseen,
        },
        _ => stat_script(ctx, path, false)?,
    };
    Ok(match seen {
        Stat::Found(found) => Some(found),
        Stat::Unseen if sudo => {
            sudo::ensure(task, ctx)?;
            match stat_script(ctx, path, true)? {
                Stat::Found(found) => Some(found),
                _ => None,
            }
        }
        Stat::Missing | Stat::Unseen => None,
    })
}

/// [`STAT`] on the machine being configured, as root with `sudo`.
fn stat_script(ctx: &Ctx, path: &Path, sudo: bool) -> Result<Stat> {
    let output = script_output(ctx, STAT, &[arg(path)], sudo)?;
    if output.status.code() == Some(3) {
        return Ok(Stat::Unseen);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let found = output
        .status
        .success()
        .then(|| text.trim().split_once(' '))
        .flatten()
        .and_then(|(mode, owner)| Some((u32::from_str_radix(mode, 8).ok()?, owner.to_string())));
    Ok(found.map_or(Stat::Missing, Stat::Found))
}

/// [`stat`] on this machine, without starting `stat` for every file: the
/// same permission bits and `user:group` (ids for unknown ones), through symlinks.
#[cfg(unix)]
fn stat_local(path: &Path) -> std::io::Result<(u32, String)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path)?;
    let user = id_name(meta.uid(), libc::getpwuid_r, |p| p.pw_name);
    let group = id_name(meta.gid(), libc::getgrgid_r, |g| g.gr_name);
    Ok((meta.mode() & 0o7777, format!("{user}:{group}")))
}

/// The name of a user or group id, with `getpwuid_r` or `getgrgid_r`; the
/// id itself if it has none.
#[cfg(unix)]
fn id_name<T>(
    id: u32,
    get: unsafe extern "C" fn(
        u32,
        *mut T,
        *mut libc::c_char,
        libc::size_t,
        *mut *mut T,
    ) -> libc::c_int,
    name: fn(&T) -> *const libc::c_char,
) -> String {
    let mut buf: Vec<libc::c_char> = vec![0; 1024];
    loop {
        // SAFETY: `passwd` and `group` are plain data, filled in by `get`
        // with strings in `buf`, which outlives every use of them here.
        unsafe {
            let mut entry: T = std::mem::zeroed();
            let mut found = std::ptr::null_mut();
            let rc = get(id, &mut entry, buf.as_mut_ptr(), buf.len(), &mut found);
            if rc == libc::ERANGE && buf.len() < 1 << 20 {
                let len = buf.len() * 2;
                buf.resize(len, 0);
                continue;
            }
            if rc != 0 || found.is_null() {
                return id.to_string();
            }
            return std::ffi::CStr::from_ptr(name(&entry))
                .to_string_lossy()
                .into_owned();
        }
    }
}

/// Everything else `stat` knows about a file, for the undo journal.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Details {
    pub dir: bool,
    pub size: u64,
    pub uid: u32,
    pub gid: u32,
    /// Nanoseconds since the epoch; `btime` (creation) isn't always known.
    pub mtime: Option<i64>,
    pub ctime: Option<i64>,
    pub btime: Option<i64>,
}

/// Prints `d` or `f`, then the size, uid, gid and the modification, change
/// and creation times (seconds, with a fraction where `stat` gives one) of
/// `$1`, through symlinks, with GNU, busybox or BSD `stat`. Exits with 2 if
/// there's nothing there, 3 if it can't be seen.
const DETAILS: &str = concat!(
    absent!(),
    r#"if [ ! -e "$1" ]; then absent "$1"; exit; fi
if [ -d "$1" ]; then printf 'd '; else printf 'f '; fi
if stat -c %a / >/dev/null 2>&1; then
  stat -L -c '%s %u %g %.9Y %.9Z %.9W' -- "$1" 2>/dev/null ||
    stat -L -c '%s %u %g %Y %Z 0' -- "$1"
else
  stat -L -f '%z %u %g %Fm %Fc %FB' -- "$1"
fi"#
);

/// `seconds[.fraction]` as nanoseconds; `None` for 0 or `-` (unknown).
fn parse_time(text: &str) -> Option<i64> {
    let (secs, frac) = text.split_once('.').unwrap_or((text, ""));
    let secs: i64 = secs.parse().ok()?;
    let digits: String = frac.chars().take(9).collect();
    let nanos: i64 = if digits.is_empty() {
        0
    } else {
        format!("{digits:0<9}").parse().ok()?
    };
    let time = secs.checked_mul(1_000_000_000)?.checked_add(nanos)?;
    (time != 0).then_some(time)
}

fn parse_details(text: &str) -> Option<Details> {
    let fields: Vec<&str> = text.split_whitespace().collect();
    let [kind, size, uid, gid, mtime, ctime, btime] = fields[..] else {
        return None;
    };
    Some(Details {
        dir: kind == "d",
        size: size.parse().ok()?,
        uid: uid.parse().ok()?,
        gid: gid.parse().ok()?,
        mtime: parse_time(mtime),
        ctime: parse_time(ctime),
        btime: parse_time(btime),
    })
}

/// [`Details`] of a file, if it exists. With `sudo`, one the user can't see
/// is looked at as root.
pub fn details(ctx: &Ctx, task: &str, path: &Path, sudo: bool) -> Result<Option<Details>> {
    let run = |root: bool| -> Result<Stat<Details>> {
        let output = script_output(ctx, DETAILS, &[arg(path)], root)?;
        if output.status.code() == Some(3) {
            return Ok(Stat::Unseen);
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let found = output.status.success().then(|| parse_details(&text));
        Ok(found.flatten().map_or(Stat::Missing, Stat::Found))
    };
    let seen = match &ctx.remote {
        #[cfg(unix)]
        None => match details_local(path) {
            Ok(found) => Stat::Found(found),
            Err(e) if gone(&e) => Stat::Missing,
            Err(_) => Stat::Unseen,
        },
        _ => run(false)?,
    };
    Ok(match seen {
        Stat::Found(found) => Some(found),
        Stat::Unseen if sudo => {
            sudo::ensure(task, ctx)?;
            match run(true)? {
                Stat::Found(found) => Some(found),
                _ => None,
            }
        }
        Stat::Missing | Stat::Unseen => None,
    })
}

/// [`details`] on this machine, without starting `stat`.
#[cfg(unix)]
fn details_local(path: &Path) -> std::io::Result<Details> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path)?;
    let time = |secs: i64, nanos: i64| {
        let time = secs.checked_mul(1_000_000_000)?.checked_add(nanos)?;
        (time != 0).then_some(time)
    };
    let btime = meta.created().ok().and_then(|t| {
        let since = t.duration_since(std::time::UNIX_EPOCH).ok()?;
        i64::try_from(since.as_nanos()).ok()
    });
    Ok(Details {
        dir: meta.is_dir(),
        size: meta.size(),
        uid: meta.uid(),
        gid: meta.gid(),
        mtime: time(meta.mtime(), meta.mtime_nsec()),
        ctime: time(meta.ctime(), meta.ctime_nsec()),
        btime,
    })
}

/// Set a file's modification time (nanoseconds since the epoch), as root with `sudo`.
pub fn set_mtime(ctx: &Ctx, task: &str, path: &Path, mtime: i64, sudo: bool) -> Result<()> {
    #[cfg(unix)]
    if ctx.remote.is_none() && !sudo {
        let time = std::time::UNIX_EPOCH
            + std::time::Duration::new(
                u64::try_from(mtime.div_euclid(1_000_000_000)).unwrap_or(0),
                u32::try_from(mtime.rem_euclid(1_000_000_000)).unwrap_or(0),
            );
        return std::fs::File::options()
            .write(true)
            .open(path)
            .and_then(|f| f.set_modified(time))
            .with_context(|| format!("failed to set the modification time of {}", path.display()));
    }
    if sudo {
        sudo::ensure(task, ctx)?;
    }
    // GNU and busybox `touch` take `@seconds`. BSD's takes a date: with `-d`
    // it keeps the fraction (macOS, FreeBSD), with `-t` only whole seconds.
    let script = r#"touch -m -d "@$2" -- "$1" 2>/dev/null ||
  touch -m -d "$(date -u -r "${2%.*}" +%Y-%m-%dT%H:%M:%S).${2#*.}Z" -- "$1" 2>/dev/null ||
  touch -m -t "$(date -r "${2%.*}" +%Y%m%d%H%M.%S)" -- "$1""#;
    let secs = mtime.div_euclid(1_000_000_000);
    let nanos = mtime.rem_euclid(1_000_000_000);
    let args = [arg(path), format!("{secs}.{nanos:09}")];
    let output = script_output(ctx, script, &args, sudo)?;
    if !output.status.success() {
        bail!(
            "setting the modification time of {} exited with {}: {}",
            path.display(),
            describe(output.status),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Replace a file the user owns with `content` and `mode`, never leaving it
/// half written: it's written beside the file, then renamed over it.
pub fn replace(ctx: &Ctx, path: &Path, content: &[u8], mode: u32) -> Result<()> {
    if let Some(remote) = &ctx.remote {
        let script = format!("sh -c {} sh {} {mode:o}", quote(INSTALL), quote(&arg(path)));
        let output = remote.output(&script, content)?;
        if !output.status.success() {
            bail!(
                "exited with {}: {}",
                describe(output.status),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        return Ok(());
    }
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = parent.join(format!(".{name}.taskpod-{}", std::process::id()));
    let written = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        options.open(&temp)?.write_all(content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode))?;
        }
        std::fs::rename(&temp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    Ok(written?)
}

/// Whether there's something at `path`. With `sudo`, a path the user can't
/// see (in a directory only root can enter) is looked for as root too.
pub fn exists(ctx: &Ctx, task: &str, path: &Path, sudo: bool) -> Result<bool> {
    let check = |root: bool| -> Result<bool> {
        Ok(script_output(ctx, r#"[ -e "$1" ]"#, &[arg(path)], root)?
            .status
            .success())
    };
    let seen = match &ctx.remote {
        Some(_) => check(false)?,
        None => path.exists(),
    };
    if seen || !sudo {
        return Ok(seen);
    }
    sudo::ensure(task, ctx)?;
    check(true)
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
    let p = quote(&arg(path));
    let script = if sudo || owner.is_some() {
        let args: Vec<String> = owner_args(owner).iter().map(|a| quote(a)).collect();
        format!(
            "sudo sh -c {} sh {p} {mode:o} {}",
            quote(INSTALL),
            args.join(" ")
        )
    } else {
        // Like a local write: through symlinks, and never readable with
        // looser permissions than `mode`, even for a moment.
        format!(
            "mkdir -p -- \"$(dirname -- {p})\" || exit 1\n\
             if [ -e {p} ]; then chmod {mode:o} {p} || exit 1; fi\n\
             (umask 077 && cat > {p}) || exit 1\n\
             chmod {mode:o} {p}\n"
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

/// Write a file as root with `install`, which also sets the owner.
fn write_with_sudo(path: &Path, content: &[u8], mode: u32, owner: Option<&str>) -> Result<()> {
    // Local arguments aren't parsed by a shell, so they're passed unquoted.
    let (mut child, job) = crate::tty::Job::spawn(
        std::process::Command::new("sudo")
            .args(["sh", "-c", INSTALL, "sh"])
            .arg(arg(path))
            .arg(format!("{mode:o}"))
            .args(owner_args(owner))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped()),
    )
    .context("failed to run sudo")?;
    // If sudo or the script exits early, the write fails with a broken pipe;
    // its exit status and stderr say why, so those are reported first.
    let written = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("no stdin for sudo"))?
        .write_all(content);
    let output = child.wait_with_output()?;
    if let Some(process) = job.finish() {
        bail!("{}", crate::tty::wanted(&process));
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "sudo install exited with {}: {}",
            describe(output.status),
            stderr.trim()
        );
    }
    written.context("failed to send the content to sudo")?;
    Ok(())
}

/// Delete a file if it's there, as root with `sudo`.
pub fn remove(ctx: &Ctx, task: &str, path: &Path, sudo: bool) -> Result<()> {
    if sudo {
        sudo::ensure(task, ctx)?;
    }
    let output = match &ctx.remote {
        Some(remote) => {
            let sudo = if sudo { "sudo " } else { "" };
            remote.output(&format!("{sudo}rm -f -- {}", quote(&arg(path))), &[])?
        }
        None if sudo => crate::tty::output(
            std::process::Command::new("sudo")
                .args(["rm", "-f", "--"])
                .arg(path),
        )
        .context("failed to run sudo")?,
        None => {
            return match std::fs::remove_file(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                _ => Ok(()),
            };
        }
    };
    if !output.status.success() {
        bail!(
            "rm exited with {}: {}",
            describe(output.status),
            String::from_utf8_lossy(&output.stderr).trim()
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

/// What's at a path that should be a directory.
#[derive(Debug, PartialEq)]
pub enum DirState {
    Missing,
    /// Something that isn't a directory.
    NotDir,
    Empty,
    NotEmpty,
    /// A directory whose contents can't be listed (as this user).
    Unlistable,
}

/// `$1`'s state, through symlinks like `stat`. One in a directory that
/// can't be searched is unlistable.
const DIR_STATE: &str = concat!(
    absent!(),
    r#"if [ ! -e "$1" ]; then
  absent "$1"
  if [ $? = 3 ]; then echo unlistable; else echo missing; fi
elif [ ! -d "$1" ]; then echo not-dir
elif [ ! -r "$1" ] || [ ! -x "$1" ]; then echo unlistable
elif [ -n "$(ls -A -- "$1")" ]; then echo not-empty
else echo empty; fi"#
);

/// Run a script with arguments on the machine being configured, as root
/// with `sudo`.
fn script_output(
    ctx: &Ctx,
    script: &str,
    args: &[String],
    sudo: bool,
) -> Result<std::process::Output> {
    match &ctx.remote {
        Some(remote) => {
            let sh = if sudo { "sudo sh" } else { "sh" };
            let mut command = format!("{sh} -c {} sh", quote(script));
            for a in args {
                command.push(' ');
                command.push_str(&quote(a));
            }
            remote.output(&command, &[])
        }
        None => {
            let mut command = if sudo {
                let mut c = std::process::Command::new("sudo");
                c.arg("sh");
                c
            } else {
                std::process::Command::new("sh")
            };
            command.args(["-c", script, "sh"]).args(args);
            crate::tty::output(&mut command).context("failed to run sh")
        }
    }
}

/// Whether `path` is a directory, and whether it's empty. With `sudo`, one
/// the user can't list is looked at as root.
pub fn dir_state(ctx: &Ctx, task: &str, path: &Path, sudo: bool) -> Result<DirState> {
    let state = |root: bool| -> Result<DirState> {
        let output = script_output(ctx, DIR_STATE, &[arg(path)], root)?;
        Ok(match String::from_utf8_lossy(&output.stdout).trim() {
            "missing" => DirState::Missing,
            "not-dir" => DirState::NotDir,
            "empty" => DirState::Empty,
            "not-empty" => DirState::NotEmpty,
            _ => DirState::Unlistable,
        })
    };
    match state(false)? {
        DirState::Unlistable if sudo => {
            sudo::ensure(task, ctx)?;
            state(true)
        }
        other => Ok(other),
    }
}

/// Create a directory (and its parents) if it's missing, and set its mode,
/// and owner if given, which needs root.
pub fn make_dir(
    ctx: &Ctx,
    task: &str,
    path: &Path,
    mode: u32,
    owner: Option<&str>,
    sudo: bool,
) -> Result<()> {
    let root = sudo || owner.is_some();
    if root {
        sudo::ensure(task, ctx)?;
    }
    let script = r#"mkdir -p -- "$1" && chmod "$2" "$1" && { [ -z "$3" ] || chown "$3" "$1"; }"#;
    let args = [
        arg(path),
        format!("{mode:o}"),
        owner.unwrap_or_default().to_string(),
    ];
    let output = script_output(ctx, script, &args, root)?;
    if !output.status.success() {
        bail!(
            "creating it exited with {}: {}",
            describe(output.status),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Remove an empty directory.
pub fn remove_dir(ctx: &Ctx, task: &str, path: &Path, sudo: bool) -> Result<()> {
    if sudo {
        sudo::ensure(task, ctx)?;
    }
    let output = script_output(ctx, r#"rmdir -- "$1""#, &[arg(path)], sudo)?;
    if !output.status.success() {
        bail!(
            "rmdir exited with {}: {}",
            describe(output.status),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::{INSTALL, STAT};
    use std::io::Write;
    use std::path::Path;
    use std::process::{Command, Stdio};

    /// Wraps the real `name` in `bin`, running `before` first.
    fn wrap(bin: &Path, name: &str, before: &str) {
        let real = Command::new("sh")
            .args(["-c", &format!("command -v {name}")])
            .output()
            .unwrap();
        let real = String::from_utf8(real.stdout).unwrap();
        crate::test_util::write_script(
            &bin.join(name),
            &format!("#!/bin/sh\n{before}\nexec '{}' \"$@\"\n", real.trim()),
        );
    }

    /// Runs `INSTALL` (as the user) with the wrappers in `bin` first on `PATH`.
    fn install(dir: &Path, path: &Path, content: &str) -> std::process::Output {
        let tmp = dir.join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let path_var = format!(
            "{}:{}",
            dir.join("bin").display(),
            std::env::var("PATH").unwrap()
        );
        let mut child = Command::new("sh")
            .args(["-c", INSTALL, "sh"])
            .arg(path)
            .arg("640")
            .env("PATH", path_var)
            .env("TMPDIR", &tmp)
            .env("LOG", dir.join("log"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(content.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn root_writes_replace_the_target_without_it_going_missing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // `install` records its destination; `mv` records its arguments and
        // whether the target was still there when it was replaced.
        wrap(
            &bin,
            "install",
            r#"for a; do d=$a; done; echo "install $d" >> "$LOG""#,
        );
        wrap(
            &bin,
            "mv",
            r#"[ -e "$3" ] && s=present || s=missing; echo "mv $2 $3 $s" >> "$LOG""#,
        );
        let rules = dir.path().join("rules");
        std::fs::create_dir_all(&rules).unwrap();
        let target = rules.join("allow.json");
        std::fs::write(&target, "old\n").unwrap();

        let output = install(dir.path(), &target, "new\n");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);

        let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "{log}");
        // Installed to a dotted temporary file next to the target...
        let temp = lines[0].strip_prefix("install ").unwrap();
        let temp = Path::new(temp);
        assert_eq!(temp.parent(), Some(rules.as_path()), "{log}");
        let name = temp.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with(".taskpod."), "{log}");
        // ...then renamed over it, which was there until then.
        assert_eq!(
            lines[1],
            format!("mv {} {} present", temp.display(), target.display())
        );
        // Nothing is left behind.
        assert_eq!(names(&rules), ["allow.json"]);
        assert!(names(&dir.path().join("tmp")).is_empty());
    }

    #[test]
    fn failed_root_writes_clean_up() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        wrap(&bin, "mv", "exit 1");
        let rules = dir.path().join("rules");
        std::fs::create_dir_all(&rules).unwrap();
        let target = rules.join("allow.json");
        std::fs::write(&target, "old\n").unwrap();

        let output = install(dir.path(), &target, "new\n");
        assert!(!output.status.success());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
        assert_eq!(names(&rules), ["allow.json"]);
        assert!(names(&dir.path().join("tmp")).is_empty());

        // A new file in a new directory works too.
        std::fs::remove_file(bin.join("mv")).unwrap();
        let new = dir.path().join("new/dir/file.conf");
        let output = install(dir.path(), &new, "x\n");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "x\n");
        assert_eq!(names(&dir.path().join("new/dir")), ["file.conf"]);
    }

    #[test]
    fn files_the_user_cant_see_arent_missing() {
        use super::{Current, DirState, READ};
        use std::os::unix::fs::PermissionsExt;
        // Root can enter any directory.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::vars::Ctx::new(dir.path().to_path_buf());
        let locked = dir.path().join("locked");
        std::fs::create_dir_all(locked.join("sub")).unwrap();
        std::fs::write(locked.join("f"), "x").unwrap();
        std::fs::write(dir.path().join("plain"), "x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        // The scripts remote machines (and root reads) use, and this machine's own reads.
        let script = |p: &Path| {
            let output = Command::new("sh")
                .args(["-c", READ, "sh"])
                .arg(p)
                .output()
                .unwrap();
            match output.status.code() {
                Some(0) => Current::Content(output.stdout),
                Some(2) => Current::Missing,
                Some(3) => Current::Unreadable,
                code => panic!("{code:?}"),
            }
        };
        let missing = [
            dir.path().join("none"),
            dir.path().join("no/such/dir/f"),
            dir.path().join("plain/under-a-file"),
        ];
        for path in &missing {
            assert_eq!(
                super::read(&ctx, "t", path, false).unwrap(),
                Current::Missing
            );
            assert_eq!(script(path), Current::Missing, "{}", path.display());
            assert_eq!(super::stat(&ctx, "t", path, false).unwrap(), None);
        }
        for path in [
            locked.join("f"),
            locked.join("none"),
            locked.join("sub/deeper/f"),
        ] {
            assert_eq!(
                super::read(&ctx, "t", &path, false).unwrap(),
                Current::Unreadable
            );
            assert_eq!(script(&path), Current::Unreadable, "{}", path.display());
            assert!(super::stat_local(&path).is_err_and(|e| !super::gone(&e)));
            assert_eq!(
                super::dir_state(&ctx, "t", &path, false).unwrap(),
                DirState::Unlistable
            );
            // Listing it needs root: it's not an empty listing.
            assert!(super::list(&ctx, "t", &path, &[String::new()], false).is_err());
        }
        assert_eq!(
            super::dir_state(&ctx, "t", &dir.path().join("none"), false).unwrap(),
            DirState::Missing
        );
        assert!(
            super::list(&ctx, "t", &dir.path().join("none"), &[String::new()], false)
                .unwrap()
                .is_empty()
        );
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// Whether the stat command's details can be `local`'s. Not every stat
    /// gives exact nanoseconds: Ubuntu 26.04's (uutils) was seen a tenth of a
    /// microsecond off, with creation times in whole seconds. The journal only
    /// keeps these times, and undo sets the modification time back, so that
    /// much is harmless.
    fn stat_matches(script: &super::Details, local: &super::Details) -> bool {
        fn time(script: Option<i64>, local: Option<i64>) -> bool {
            match (script, local) {
                (Some(s), Some(l)) => {
                    (s - l).abs() < 1_000
                        || (s % 1_000_000_000 == 0 && s == l - l.rem_euclid(1_000_000_000))
                }
                (s, l) => s == l,
            }
        }
        (script.dir, script.size, script.uid, script.gid)
            == (local.dir, local.size, local.uid, local.gid)
            && time(script.mtime, local.mtime)
            && time(script.ctime, local.ctime)
            && time(script.btime, local.btime)
    }

    #[test]
    fn local_details_match_the_stat_command() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, "content").unwrap();
        for path in [file.as_path(), dir.path()] {
            let output = Command::new("sh")
                .args(["-c", super::DETAILS, "sh"])
                .arg(path)
                .output()
                .unwrap();
            let text = String::from_utf8(output.stdout).unwrap();
            let script = super::parse_details(&text).unwrap();
            let local = super::details_local(path).unwrap();
            assert!(
                stat_matches(&script, &local),
                "{text}\n{script:?}\n{local:?}"
            );
        }
        // Ubuntu 26.04's stat (uutils): the fraction off by about 0.1 µs, and
        // the creation time in whole seconds.
        let uutils = super::parse_details(
            "f 7 1001 1001 1795287538.977342600 1795287538.977342600 1795287538",
        )
        .unwrap();
        let exact = super::Details {
            dir: false,
            size: 7,
            uid: 1001,
            gid: 1001,
            mtime: Some(1_795_287_538_977_342_713),
            ctime: Some(1_795_287_538_977_342_713),
            btime: Some(1_795_287_538_977_342_713),
        };
        assert!(stat_matches(&uutils, &exact));
        // A second off, or a time missing, is still a mismatch.
        let later = super::Details {
            mtime: Some(1_795_287_539_977_342_713),
            ..exact.clone()
        };
        assert!(!stat_matches(&uutils, &later));
        let unknown = super::Details {
            btime: None,
            ..exact.clone()
        };
        assert!(!stat_matches(&uutils, &unknown));
        assert_eq!(super::parse_time("12.5"), Some(12_500_000_000));
        assert_eq!(super::parse_time("0"), None);
        assert_eq!(super::parse_time("-"), None);
    }

    #[test]
    fn local_stat_matches_the_stat_command() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, "x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o4751)).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        for path in [file.as_path(), link.as_path(), dir.path(), Path::new("/")] {
            let output = Command::new("sh")
                .args(["-c", STAT, "sh"])
                .arg(path)
                .output()
                .unwrap();
            let text = String::from_utf8(output.stdout).unwrap();
            let (mode, owner) = text.trim().split_once(' ').unwrap();
            let expected = (u32::from_str_radix(mode, 8).unwrap(), owner.to_string());
            assert_eq!(
                super::stat_local(path).ok(),
                Some(expected),
                "{}",
                path.display()
            );
        }
        assert!(super::stat_local(&dir.path().join("missing")).is_err());
    }
}
