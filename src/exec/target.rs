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
            let output = crate::tty::output(
                std::process::Command::new("sudo")
                    .arg("cat")
                    .arg("--")
                    .arg(path),
            )
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

/// The files under a directory (through symlinks), as paths relative to it
/// joined with `/`, sorted; empty if there's no directory. With `sudo`, a
/// directory the user can't list is listed as root.
pub fn list(
    ctx: &Ctx,
    task: &str,
    dir: &Path,
    subdirs: &[String],
    sudo: bool,
) -> Result<Vec<String>> {
    // Only the files right in each subdirectory: not what's in others, like
    // an app's data next to its config.
    let script = "[ -d \"$1\" ] || exit 0\ncd -- \"$1\" && shift\nfor d; do\n  if [ -d \"$d\" ]; then find -L \"$d\" -mindepth 1 -maxdepth 1 -type f || exit 1; fi\ndone";
    let subdirs: Vec<&str> = subdirs
        .iter()
        .map(|d| if d.is_empty() { "." } else { d.as_str() })
        .collect();
    let run = |root: bool| -> Result<std::process::Output> {
        let dir = arg(dir);
        match &ctx.remote {
            Some(remote) => {
                let sh = if root { "sudo sh" } else { "sh" };
                let mut script = format!("{sh} -c {} sh {}", quote(script), quote(&dir));
                for d in &subdirs {
                    script.push(' ');
                    script.push_str(&quote(d));
                }
                remote.output(&script, &[])
            }
            None => {
                let mut command = if root {
                    let mut c = std::process::Command::new("sudo");
                    c.arg("sh");
                    c
                } else {
                    std::process::Command::new("sh")
                };
                command.args(["-c", script, "sh"]).arg(dir).args(&subdirs);
                crate::tty::output(&mut command).context("failed to list files")
            }
        }
    };
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
const STAT: &str = r#"if stat -c %a / >/dev/null 2>&1; then
  stat -L -c '%a %U:%G' -- "$1"
else
  stat -L -f '%Mp%Lp %Su:%Sg' -- "$1"
fi"#;

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

/// Permission bits and `user:group` of a file, if it exists.
pub fn stat(ctx: &Ctx, path: &Path) -> Option<(u32, String)> {
    let output = match &ctx.remote {
        Some(remote) => {
            let script = format!("set -- {}\n{STAT}", quote(&arg(path)));
            remote.output(&script, &[]).ok()?
        }
        #[cfg(unix)]
        None => return stat_local(path),
        #[cfg(not(unix))]
        None => {
            if !path.exists() {
                return None;
            }
            crate::tty::output(
                std::process::Command::new("sh")
                    .args(["-c", STAT, "sh"])
                    .arg(arg(path)),
            )
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

/// [`stat`] on this machine, without starting `stat` for every file: the
/// same permission bits and `user:group` (ids for unknown ones), through symlinks.
#[cfg(unix)]
fn stat_local(path: &Path) -> Option<(u32, String)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    let user = id_name(meta.uid(), libc::getpwuid_r, |p| p.pw_name);
    let group = id_name(meta.gid(), libc::getgrgid_r, |g| g.gr_name);
    Some((meta.mode() & 0o7777, format!("{user}:{group}")))
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

/// `$1`'s state, through symlinks like `stat`.
const DIR_STATE: &str = r#"if [ ! -e "$1" ]; then echo missing
elif [ ! -d "$1" ]; then echo not-dir
elif [ ! -r "$1" ] || [ ! -x "$1" ]; then echo unlistable
elif [ -n "$(ls -A -- "$1")" ]; then echo not-empty
else echo empty; fi"#;

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
                super::stat_local(path),
                Some(expected),
                "{}",
                path.display()
            );
        }
        assert_eq!(super::stat_local(&dir.path().join("missing")), None);
    }
}
