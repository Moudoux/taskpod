//! Running the steps of one task.

use super::{Outcome, Runner, sudo};
use crate::ast::{Block, FileSource, FileStep, Step, Task};
use crate::term::term;
use crate::vars::{Scope, Vars, expand_tilde};
use anyhow::{Context, Result, anyhow, bail};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::MutexGuard;

/// A job slot, given back while waiting for a lock.
struct Slot<'r> {
    runner: &'r Runner<'r>,
    held: bool,
}

impl<'r> Slot<'r> {
    fn take(runner: &'r Runner<'r>) -> Self {
        runner.slots.acquire();
        Slot { runner, held: true }
    }

    fn release(&mut self) {
        if self.held {
            self.runner.slots.release();
            self.held = false;
        }
    }

    fn reacquire(&mut self) {
        if !self.held {
            self.runner.slots.acquire();
            self.held = true;
        }
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

pub(crate) struct TaskRun<'r> {
    runner: &'r Runner<'r>,
    task: &'r Task,
    slot: Slot<'r>,
    cwd: PathBuf,
    /// Whether a step changed something.
    changed: bool,
    /// Whether the task's start line was printed.
    started: bool,
    /// Dry run output, printed when the task is done.
    pub lines: Vec<String>,
}

impl<'r> TaskRun<'r> {
    pub fn new(runner: &'r Runner<'r>, task: &'r Task) -> Self {
        TaskRun {
            runner,
            task,
            slot: Slot::take(runner),
            cwd: PathBuf::new(),
            changed: false,
            started: false,
            lines: Vec::new(),
        }
    }

    fn dry_run(&self) -> bool {
        self.runner.opts.dry_run
    }

    pub fn run(&mut self) -> Result<Outcome> {
        self.cwd = self.runner.workdir(self.task)?;
        let root = self.runner.root_scope();
        let scope = root.child(&self.task.vars);
        if let Some(guard) = self
            .runner
            .failing_guard(&self.task.body.guards, &scope, &self.cwd)?
        {
            return Ok(Outcome::Skipped(guard.describe()));
        }
        self.block(&self.task.body, &scope, 1)?;
        Ok(if self.changed {
            Outcome::Ran
        } else {
            Outcome::UpToDate
        })
    }

    /// Print the task's start line once, before its first change.
    fn start(&mut self, scope: &Scope) {
        if self.started || self.dry_run() {
            return;
        }
        self.started = true;
        term().status("▶", "34", &self.task.name, &self.desc(scope));
    }

    pub fn desc(&self, scope: &Scope) -> String {
        match &self.task.desc {
            Some(desc) => scope.expand(desc).unwrap_or_else(|_| desc.clone()),
            None => String::new(),
        }
    }

    /// Output line for this task, indented by `depth` in dry runs.
    fn say(&mut self, depth: usize, line: &str) {
        if self.dry_run() {
            self.lines
                .push(format!("{:indent$}{line}", "", indent = depth * 2));
        } else {
            term().output(&self.task.name, line);
        }
    }

    /// Run a block's steps. Its guards were already checked.
    fn block(&mut self, block: &'r Block, scope: &Scope, depth: usize) -> Result<()> {
        let mut _locks: Vec<MutexGuard<'r, ()>> = Vec::new();
        if !self.dry_run() {
            for name in &block.locks {
                _locks.push(self.lock(name));
            }
            if block.sudo {
                sudo::ensure()?;
            }
        }
        for step in &block.steps {
            match step {
                Step::Run(script) => {
                    let script = scope.expand(script)?;
                    self.start(scope);
                    self.changed = true;
                    if self.dry_run() {
                        for line in script.lines() {
                            self.say(depth, &format!("{} {line}", term().paint("2", "$")));
                        }
                    } else {
                        self.exec(&script)?;
                    }
                }
                Step::File(file) => self.file(file, scope, depth)?,
                Step::Call { template, args } => {
                    let template = &self.runner.file.templates[template];
                    let args = scope.expand(args)?;
                    let label = format!("{} {args}", template.name).trim().to_string();
                    let arg_vars = Vars::for_args(&args);
                    let call_scope = scope.child(&arg_vars);
                    let inner = call_scope.child(&template.vars);
                    let term = term();
                    match self
                        .runner
                        .failing_guard(&template.body.guards, &inner, &self.cwd)?
                    {
                        Some(_) => {
                            if self.dry_run() {
                                self.say(depth, &format!("{} {label}", term.paint("32", "✓")));
                            } else if term.verbose {
                                self.say(depth, &format!("✓ {label}: up to date"));
                            }
                        }
                        None => {
                            self.start(scope);
                            let arrow = term.paint("33", "→");
                            self.say(depth, &format!("{arrow} {label}"));
                            self.block(&template.body, &inner, depth + 1)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Take a named lock, giving the job slot back while waiting for it.
    fn lock(&mut self, name: &str) -> MutexGuard<'r, ()> {
        let mutex = &self.runner.locks[name];
        if let Ok(guard) = mutex.try_lock() {
            return guard;
        }
        self.slot.release();
        let guard = mutex.lock().unwrap_or_else(|e| e.into_inner());
        self.slot.reacquire();
        guard
    }

    fn exec(&mut self, script: &str) -> Result<()> {
        if sudo::mentions_sudo(script) {
            sudo::ensure()?;
        }
        let ctx = &self.runner.file.ctx;
        let term = term();
        if term.verbose {
            for line in script.lines() {
                self.say(0, &format!("$ {line}"));
            }
        }
        let mut command = ctx.command(script, &self.cwd);
        let inherit = self.task.interactive || self.runner.opts.jobs <= 1;
        let status = if inherit {
            command.status()
        } else {
            self.piped(&mut command)
        }
        .with_context(|| format!("failed to start shell {:?}", ctx.shell[0]))?;
        if !status.success() {
            let first = script.lines().next().unwrap_or_default();
            let more = if script.lines().count() > 1 {
                " ..."
            } else {
                ""
            };
            bail!("`{first}{more}` exited with {}", describe(status));
        }
        Ok(())
    }

    /// Run with output prefixed by the task name.
    fn piped(&self, command: &mut std::process::Command) -> std::io::Result<ExitStatus> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let name = self.task.name.as_str();
        std::thread::scope(|s| {
            if let Some(out) = stdout {
                s.spawn(move || pump(out, name));
            }
            if let Some(err) = stderr {
                s.spawn(move || pump(err, name));
            }
        });
        child.wait()
    }

    fn file(&mut self, file: &FileStep, scope: &Scope, depth: usize) -> Result<()> {
        let path = PathBuf::from(expand_tilde(&scope.expand(&file.path)?));
        let path = if path.is_absolute() {
            path
        } else {
            self.cwd.join(path)
        };
        let content = match &file.source {
            FileSource::Inline(text) => scope.expand(text)?.into_bytes(),
            FileSource::Copy(source) => {
                let source = PathBuf::from(expand_tilde(&scope.expand(source)?));
                let source = if source.is_absolute() {
                    source
                } else {
                    self.runner.file.ctx.dir.join(source)
                };
                std::fs::read(&source)
                    .with_context(|| format!("failed to read {}", source.display()))?
            }
        };
        let mode = file.mode.unwrap_or(if content.starts_with(b"#!") {
            0o755
        } else {
            0o644
        });
        let shown = path.display().to_string();
        if file_is_current(&path, &content, mode) {
            if self.dry_run() {
                self.say(depth, &format!("{} file {shown}", term().paint("32", "✓")));
            } else if term().verbose {
                self.say(depth, &format!("✓ file {shown}: up to date"));
            }
            return Ok(());
        }
        self.start(scope);
        self.changed = true;
        let arrow = term().paint("33", "→");
        if self.dry_run() {
            self.say(depth, &format!("{arrow} file {shown} (mode {mode:o})"));
            return Ok(());
        }
        self.say(depth, &format!("{arrow} writing {shown}"));
        if file.sudo {
            write_with_sudo(&path, &content, mode)
        } else {
            write_file(&path, &content, mode)
        }
        .with_context(|| format!("failed to write {shown}"))
    }
}

/// Print a stream line by line. For `\r` progress updates, the last one wins.
fn pump(stream: impl Read, task: &str) {
    let mut reader = BufReader::new(stream);
    let mut buffer = Vec::new();
    while let Ok(n) = reader.read_until(b'\n', &mut buffer) {
        if n == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buffer);
        let line = line.trim_end_matches(['\n', '\r']);
        let line = line.rsplit('\r').next().unwrap_or_default();
        term().output(task, line);
        buffer.clear();
    }
}

fn describe(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("status {code}"),
        None => status.to_string(),
    }
}

fn file_is_current(path: &Path, content: &[u8], mode: u32) -> bool {
    let Ok(existing) = std::fs::read(path) else {
        return false;
    };
    existing == content && mode_matches(path, mode)
}

#[cfg(unix)]
fn mode_matches(path: &Path, mode: u32) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o7777 == mode)
}

#[cfg(not(unix))]
fn mode_matches(_: &Path, _: u32) -> bool {
    true
}

fn write_file(path: &Path, content: &[u8], mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

/// Write a file as root with `sudo install`, which also creates parent directories.
fn write_with_sudo(path: &Path, content: &[u8], mode: u32) -> Result<()> {
    sudo::ensure()?;
    let mut child = std::process::Command::new("sudo")
        .arg("install")
        .arg("-D")
        .arg("-m")
        .arg(format!("{mode:o}"))
        .arg("/dev/stdin")
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .context("failed to run sudo")?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("no stdin for sudo"))?
        .write_all(content)?;
    let status = child.wait()?;
    if !status.success() {
        bail!("sudo install exited with {}", describe(status));
    }
    Ok(())
}
