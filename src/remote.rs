//! Running on another machine over SSH, without installing taskpod there.
//!
//! Parsing, templating, prompts and secrets stay local; commands, checks and
//! file reads and writes run remotely. One master connection is opened per
//! run (authenticating once), and every command is multiplexed over it.
//!
//! Each remote command is a POSIX shell script sent over stdin: the remote
//! login shell only ever sees a fixed bootstrap that reads a length-prefixed
//! script and `eval`s it, so it works whatever the login shell is, and
//! scripts (and secrets in them) never appear in a command line. Anything
//! after the script on stdin, such as file content, is left for the script.

use crate::ui::{self, ui};
use anyhow::{Context, Result, bail};
use console::style;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::Mutex;
use std::time::Duration;

/// The remote command: read a line with the script's length, then that many
/// bytes, and run them. No `'` or `\` inside, so any login shell passes it on.
const BOOTSTRAP: &str = r#"sh -c 'IFS= read -r n && eval "$(dd bs=1 count=$n 2>/dev/null)"'"#;

pub struct Remote {
    /// `user@host`, or anything else `ssh` understands, like a `Host` alias.
    pub host: String,
    /// The remote user's home directory and name.
    pub home: String,
    pub user: String,
    ssh: String,
    /// Directory holding the master connection's socket.
    control_dir: PathBuf,
    master: Mutex<Option<(Child, ChildStdin)>>,
    sudo: Mutex<Sudo>,
}

#[derive(Clone, PartialEq)]
enum Sudo {
    Unknown,
    Passwordless,
    Password(String),
    Failed,
}

impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remote").field("host", &self.host).finish()
    }
}

impl Remote {
    /// Connect to `host`, asking for credentials if `ssh` needs them.
    pub fn connect(host: &str) -> Result<Remote> {
        Self::connect_with(host, "ssh")
    }

    /// Connect using another `ssh` program, for tests.
    pub fn connect_with(host: &str, ssh: &str) -> Result<Remote> {
        if cfg!(not(unix)) {
            bail!("--host needs a Unix system to run on");
        }
        if host.starts_with('-') {
            bail!("invalid host '{host}'");
        }
        static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("taskpod-ssh-{}-{n}", std::process::id());
        let control_dir = std::env::temp_dir().join(name);
        create_private_dir(&control_dir)
            .with_context(|| format!("failed to create {}", control_dir.display()))?;
        let mut remote = Remote {
            host: host.to_string(),
            home: String::new(),
            user: String::new(),
            ssh: ssh.to_string(),
            control_dir,
            master: Mutex::new(None),
            sudo: Mutex::new(Sudo::Unknown),
        };
        remote.start_master()?;
        let output = remote.output("printf '%s\\n%s\\n' \"$HOME\" \"$(id -un)\"", &[])?;
        if !output.status.success() {
            bail!(
                "couldn't run a command on {host}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut lines = text.lines();
        remote.home = lines.next().unwrap_or_default().to_string();
        remote.user = lines.next().unwrap_or_default().to_string();
        if !remote.home.starts_with('/') {
            bail!("{host} has no usable home directory ({:?})", remote.home);
        }
        Ok(remote)
    }

    fn socket(&self) -> PathBuf {
        self.control_dir.join("socket")
    }

    fn base(&self) -> Command {
        let mut command = Command::new(&self.ssh);
        command
            .arg("-o")
            .arg(format!("ControlPath={}", self.socket().display()));
        command
    }

    /// Start the master connection. Its remote side waits for stdin to
    /// close, so the connection ends with taskpod, even if taskpod is killed.
    fn start_master(&mut self) -> Result<()> {
        ui().line(format!(
            "{} {}",
            style("⇄").cyan(),
            style(format!("connecting to {}", self.host)).dim()
        ));
        let mut child = self
            .base()
            .args(["-M", "-o", "ControlPersist=no", "-o", "LogLevel=ERROR"])
            .arg("--")
            .arg(&self.host)
            .arg("cat > /dev/null")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to start {}", self.ssh))?;
        let stdin = child.stdin.take().expect("stdin is piped");
        // ssh asks for passwords and host keys on the terminal itself.
        loop {
            if let Some(status) = child.try_wait()? {
                bail!("ssh to {} failed ({status})", self.host);
            }
            let ready = self
                .base()
                .args(["-O", "check", "--"])
                .arg(&self.host)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if ready {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        *self.master.lock().unwrap_or_else(|e| e.into_inner()) = Some((child, stdin));
        Ok(())
    }

    /// An `ssh` command running `remote` over the master connection.
    fn client(&self, tty: bool, remote: &str) -> Command {
        let mut command = self.base();
        command
            .arg(if tty { "-t" } else { "-T" })
            .args(["-o", "ControlMaster=no", "-o", "BatchMode=yes"])
            .arg("--")
            .arg(&self.host)
            .arg(remote);
        command
    }

    /// A command running `script` remotely, and the bytes to write to its
    /// stdin: the framed script, then `input`.
    pub fn command(&self, script: &str, input: &[u8]) -> (Command, Vec<u8>) {
        let script = self.with_prelude(script);
        let mut stdin = format!("{}\n{script}", script.len()).into_bytes();
        stdin.extend_from_slice(input);
        (self.client(false, BOOTSTRAP), stdin)
    }

    /// A command running `script` remotely with a terminal, for interactive
    /// tasks. The script is uploaded to a private temporary file first, since
    /// stdin belongs to the terminal; it deletes itself when it starts.
    pub fn interactive_command(&self, script: &str) -> Result<Command> {
        let script = format!("rm -f \"$0\"\n{}", self.with_prelude(script));
        let upload = "umask 077 && f=$(mktemp) && cat > \"$f\" && printf '%s' \"$f\"";
        let output = self.raw_output(upload, script.as_bytes())?;
        let path = String::from_utf8_lossy(&output.stdout).to_string();
        let safe = path.starts_with('/')
            && path
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'));
        if !output.status.success() || !safe {
            bail!(
                "failed to upload a script to {}: {}",
                self.host,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(self.client(true, &format!("sh {path}")))
    }

    /// Run `script` remotely with `input` after it on stdin, and collect its output.
    pub fn output(&self, script: &str, input: &[u8]) -> Result<Output> {
        let (command, stdin) = self.command(script, input);
        run_with_input(command, stdin)
    }

    /// Like `output`, without the sudo prelude.
    fn raw_output(&self, script: &str, input: &[u8]) -> Result<Output> {
        let mut stdin = format!("{}\n{script}", script.len()).into_bytes();
        stdin.extend_from_slice(input);
        run_with_input(self.client(false, BOOTSTRAP), stdin)
    }

    /// Whether `script` succeeds remotely, without the sudo prelude.
    fn succeeds(&self, script: &str) -> bool {
        self.raw_output(script, &[])
            .is_ok_and(|o| o.status.success())
    }

    fn with_prelude(&self, script: &str) -> String {
        match &*self.sudo.lock().unwrap_or_else(|e| e.into_inner()) {
            Sudo::Password(password) => format!("{}{script}", sudo_prelude(password)),
            _ => script.to_string(),
        }
    }

    /// Make sure remote commands can use sudo. Passwordless sudo is used as
    /// is; otherwise the password is asked once, kept in memory, and given to
    /// sudo through an askpass helper in each command that may need it.
    pub fn ensure_sudo(&self, task: &str) -> Result<()> {
        let mut state = self.sudo.lock().unwrap_or_else(|e| e.into_inner());
        match &*state {
            Sudo::Passwordless | Sudo::Password(_) => return Ok(()),
            Sudo::Failed => bail!("sudo on {} failed earlier in this run", self.host),
            Sudo::Unknown => {}
        }
        if self.succeeds("sudo -n true") {
            *state = Sudo::Passwordless;
            return Ok(());
        }
        let question = format!("sudo password for {}@{}", self.user, self.host);
        for attempt in 0..3 {
            if attempt == 0 {
                ui().suspend(|| {
                    eprintln!(
                        "{} {} {}",
                        style("🔒").yellow(),
                        style(format!("{task} needs sudo on {}", self.host)).bold(),
                        style("· asked once for the whole run").dim()
                    );
                });
            }
            let password = match ui().password(&question) {
                Ok(password) => password,
                Err(e) => {
                    *state = Sudo::Failed;
                    return Err(e);
                }
            };
            let check = format!("{}sudo -k; sudo true", sudo_prelude(&password));
            if self.succeeds(&check) {
                ui::add_redactions([&password]);
                *state = Sudo::Password(password);
                return Ok(());
            }
            ui().suspend(|| eprintln!("{}", style("Sorry, try again.").red()));
        }
        *state = Sudo::Failed;
        bail!("sudo authentication on {} failed", self.host)
    }

    /// Use this sudo password without asking, for tests.
    #[doc(hidden)]
    pub fn set_sudo_password(&self, password: &str) {
        *self.sudo.lock().unwrap_or_else(|e| e.into_inner()) = Sudo::Password(password.into());
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        let master = self.master.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some((mut child, stdin)) = master {
            // Closing stdin ends the remote `cat`, and with it the connection.
            drop(stdin);
            let _ = self
                .base()
                .args(["-O", "exit", "--"])
                .arg(&self.host)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.control_dir);
    }
}

/// Put a `sudo` on PATH that gets the password from an askpass helper. Both
/// live in a private temporary directory, removed when the command ends.
fn sudo_prelude(password: &str) -> String {
    format!(
        r#"TASKPOD_D=$(mktemp -d "${{XDG_RUNTIME_DIR:-${{TMPDIR:-/tmp}}}}/taskpod.XXXXXX") || exit 1
trap 'rm -rf "$TASKPOD_D"' EXIT
trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM
TASKPOD_SUDO=$(command -v sudo) || {{ echo "sudo isn't installed" >&2; exit 127; }}
mkdir "$TASKPOD_D/bin" || exit 1
printf '%s\n' {password} > "$TASKPOD_D/password"
printf '#!/bin/sh\nexec cat "%s/password"\n' "$TASKPOD_D" > "$TASKPOD_D/askpass"
printf '#!/bin/sh\nSUDO_ASKPASS="%s/askpass" exec "%s" -A "$@"\n' "$TASKPOD_D" "$TASKPOD_SUDO" > "$TASKPOD_D/bin/sudo"
chmod 700 "$TASKPOD_D/askpass" "$TASKPOD_D/bin/sudo" || exit 1
PATH="$TASKPOD_D/bin:$PATH"; export PATH
"#,
        password = quote(password)
    )
}

/// Spawn `command`, write `input` to its stdin while collecting its output.
fn run_with_input(mut command: Command, input: Vec<u8>) -> Result<Output> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start ssh")?;
    let stdin = child.stdin.take().expect("stdin is piped");
    let writer = std::thread::spawn(move || write_all(stdin, &input));
    let output = child.wait_with_output()?;
    let _ = writer.join();
    Ok(output)
}

/// Write `input` and close the pipe. A command may exit without reading it all.
pub fn write_all(mut stdin: ChildStdin, input: &[u8]) {
    let _ = stdin.write_all(input);
}

/// Quote `text` as one POSIX shell word.
pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn create_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes() {
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn bootstrap_is_safe_for_any_login_shell() {
        let inner = BOOTSTRAP
            .strip_prefix("sh -c '")
            .and_then(|s| s.strip_suffix('\''))
            .unwrap();
        assert!(!inner.contains('\'') && !inner.contains('\\'));
    }
}
