//! Ask for sudo credentials once, then keep them fresh, so tasks running in
//! parallel never prompt for a password at the same time.

use crate::ui::ui;
use anyhow::{Result, bail};
use console::style;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

#[derive(PartialEq)]
enum State {
    Unknown,
    Authenticated,
    /// Authentication failed once; don't ask again for every task.
    Failed,
}

static STATE: Mutex<State> = Mutex::new(State::Unknown);

/// Make sure sudo has cached credentials. The first time, the live display
/// pauses and sudo itself asks (password, security key or fingerprint), so
/// taskpod never sees the password.
pub fn ensure(task: &str, ctx: &crate::vars::Ctx) -> Result<()> {
    if let Some(remote) = &ctx.remote {
        return remote.ensure_sudo(task);
    }
    if cfg!(not(unix)) {
        return Ok(());
    }
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    match *state {
        State::Authenticated => return Ok(()),
        State::Failed => bail!("sudo authentication failed earlier in this run"),
        State::Unknown => {}
    }
    if !sudo(&["-n", "true"]) {
        let ok = ui().suspend(|| {
            eprintln!(
                "{} {} {}",
                style("🔒").yellow(),
                style(format!("{task} needs sudo")).bold(),
                style("· authenticate once for the whole run").dim()
            );
            crate::tty::clean(Command::new("sudo").arg("-v"))
                .status()
                .is_ok_and(|s| s.success())
        });
        if !ok {
            *state = State::Failed;
            bail!("sudo authentication failed");
        }
    }
    *state = State::Authenticated;
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(Duration::from_secs(60));
            sudo(&["-n", "-v"]);
        }
    });
    Ok(())
}

fn sudo(args: &[&str]) -> bool {
    crate::tty::clean(Command::new("sudo").args(args))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Whether a script calls sudo. Quoted text, like `echo "run sudo x"`, doesn't count.
pub fn mentions_sudo(script: &str) -> bool {
    let mut unquoted = String::with_capacity(script.len());
    let mut quote = None;
    let mut escaped = false;
    for c in script.chars() {
        match quote {
            // Outside quotes the character stands for itself: `\sudo` runs sudo.
            None if escaped => {
                escaped = false;
                unquoted.push(c);
            }
            _ if escaped => escaped = false,
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' => escaped = true,
            Some(_) => {}
            // `it\'s` has no quote in it.
            None if c == '\\' => escaped = true,
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                unquoted.push(' ');
            }
            None => unquoted.push(c),
        }
    }
    unquoted
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
        .any(|word| word == "sudo")
}

#[cfg(test)]
mod tests {
    #[test]
    fn detects_sudo() {
        assert!(super::mentions_sudo("echo x | sudo tee /etc/x"));
        assert!(super::mentions_sudo("a &&sudo b"));
        assert!(!super::mentions_sudo("pseudo-random sudoers"));
        assert!(!super::mentions_sudo("echo \"run 'sudo x'\" 'sudo'"));
        assert!(super::mentions_sudo("echo \"a\\\"\" && sudo b"));
        assert!(super::mentions_sudo("echo it\\'s && sudo b"));
        assert!(super::mentions_sudo("\\sudo b"));
    }
}
