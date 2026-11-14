//! Ask for sudo credentials once, then keep them fresh, so tasks running in
//! parallel never prompt for a password at the same time.

use crate::term::term;
use anyhow::{Result, bail};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

static AUTHENTICATED: Mutex<bool> = Mutex::new(false);

pub fn ensure() -> Result<()> {
    if cfg!(not(unix)) {
        return Ok(());
    }
    let mut authenticated = AUTHENTICATED.lock().unwrap_or_else(|e| e.into_inner());
    if *authenticated {
        return Ok(());
    }
    if !sudo(&["-n", "true"]) {
        let term = term();
        let _held = term.hold();
        println!("{}", term.paint("33", ":: sudo needs to authenticate"));
        let status = Command::new("sudo").arg("-v").status();
        if !status.is_ok_and(|s| s.success()) {
            bail!("sudo authentication failed");
        }
    }
    *authenticated = true;
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(Duration::from_secs(60));
            sudo(&["-n", "-v"]);
        }
    });
    Ok(())
}

fn sudo(args: &[&str]) -> bool {
    Command::new("sudo")
        .args(args)
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
            _ if escaped => escaped = false,
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' => escaped = true,
            Some(_) => {}
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
    }
}
