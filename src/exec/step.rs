//! Running the steps of one task.

use super::{Mode, Outcome, Runner, sudo};
use crate::ast::{Block, Encoding, FileSource, FileStep, Guard, Step, Task};
use crate::ui::{self, TaskLine, ui};
use crate::vars::{Scope, Vars, expand_tilde};
use anyhow::{Context, Result, anyhow, bail};
use console::style;
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
    /// The live line, when running for real.
    line: Option<TaskLine>,
    /// Lines shown under the task's status line when it's done: the plan in
    /// checks, what changed in a live display.
    lines: Vec<String>,
    failed: bool,
}

impl<'r> TaskRun<'r> {
    pub fn new(runner: &'r Runner<'r>, task: &'r Task) -> Self {
        TaskRun {
            runner,
            task,
            slot: Slot::take(runner),
            cwd: PathBuf::new(),
            changed: false,
            line: None,
            lines: Vec::new(),
            failed: false,
        }
    }

    fn mode(&self) -> Mode {
        self.runner.opts.mode
    }

    fn checking(&self) -> bool {
        self.mode() != Mode::Run
    }

    pub fn run(&mut self) -> Result<Outcome> {
        if !self.checking() {
            self.line = Some(ui().task(&self.task.name));
        }
        let result = self.run_inner();
        self.failed = result.is_err();
        result
    }

    fn run_inner(&mut self) -> Result<Outcome> {
        self.cwd = self.runner.workdir(self.task)?;
        let root = self.runner.root_scope();
        let scope = root.child(&self.task.vars);
        self.status("checking");
        if let Some(guard) = self
            .runner
            .failing_guard(&self.task.body.guards, &scope, &self.cwd)?
        {
            let reason = guard.describe();
            return Ok(Outcome::Skipped(scope.expand(&reason).unwrap_or(reason)));
        }
        let task = self.task;
        self.block(&task.body, &scope, 0)?;
        Ok(if self.changed {
            Outcome::Ran
        } else {
            Outcome::UpToDate
        })
    }

    /// Lines to print under the status line, with recent output after a failure.
    pub fn finish(&mut self) -> Vec<String> {
        let mut lines = std::mem::take(&mut self.lines);
        if let Some(line) = self.line.take()
            && self.failed
        {
            let tail = line.take_tail();
            if !tail.is_empty() {
                lines.push(style("last output:").dim().to_string());
                lines.extend(
                    tail.into_iter()
                        .map(|l| format!("{} {l}", style("│").dim())),
                );
            }
        }
        lines
    }

    pub fn desc(&self, scope: &Scope) -> String {
        match &self.task.desc {
            Some(desc) => scope.expand(desc).unwrap_or_else(|_| desc.clone()),
            None => String::new(),
        }
    }

    /// What the task is doing now, shown on its spinner.
    fn status(&self, message: &str) {
        if let Some(line) = &self.line
            && !line.shows_live_output()
        {
            line.status(message);
        }
    }

    fn push(&mut self, depth: usize, line: String) {
        self.lines
            .push(format!("{:indent$}{line}", "", indent = depth * 2));
    }

    /// Something changes (or would, in a check).
    fn change(&mut self, depth: usize, label: &str) {
        self.changed = true;
        match &self.line {
            Some(line) if line.shows_live_output() => line.status(&ui::change(label)),
            Some(line) => {
                line.status(label);
                self.push(depth, ui::change(label));
            }
            None => self.push(depth, ui::change(label)),
        }
    }

    /// Something is already as it should be.
    fn current(&mut self, depth: usize, label: &str) {
        match self.mode() {
            Mode::DryRun => self.push(depth, ui::ok(label)),
            Mode::Run if ui().verbose => {
                if let Some(line) = &self.line {
                    line.status(&ui::ok(label));
                }
            }
            Mode::Diff | Mode::Run => {}
        }
    }

    /// Run a block's steps. Its guards were already checked.
    fn block(&mut self, block: &'r Block, scope: &Scope, depth: usize) -> Result<()> {
        let mut _locks: Vec<MutexGuard<'r, ()>> = Vec::new();
        if !self.checking() {
            for name in &block.locks {
                self.status(&format!("waiting for lock {name}"));
                _locks.push(self.lock(name));
            }
            if block.sudo {
                sudo::ensure(&self.task.name)?;
            }
        }
        let runner = self.runner;
        for step in &block.steps {
            match step {
                Step::Run(script) => self.run_step(&scope.expand(script)?, depth)?,
                Step::File(file) => self.file(file, scope, depth)?,
                Step::Call { template, args } => {
                    let template = &runner.file.templates[template];
                    let args = scope.expand(args)?;
                    let label = format!("{} {args}", template.name).trim().to_string();
                    let arg_vars = Vars::for_args(&args);
                    let call_scope = scope.child(&arg_vars);
                    let inner = call_scope.child(&template.vars);
                    self.nested(&template.body, &inner, depth, &label)?;
                }
                Step::Block(inner) => {
                    let label = inner
                        .guards
                        .first()
                        .map_or_else(|| "block".into(), Guard::describe);
                    let label = scope.expand(&label).unwrap_or(label);
                    self.nested(inner, scope, depth, &label)?;
                }
            }
        }
        Ok(())
    }

    /// A template call or inline block: check its guards, then run it.
    fn nested(&mut self, block: &'r Block, scope: &Scope, depth: usize, label: &str) -> Result<()> {
        self.status(&format!("checking {label}"));
        if self
            .runner
            .failing_guard(&block.guards, scope, &self.cwd)?
            .is_some()
        {
            self.current(depth, label);
            return Ok(());
        }
        // The block only counts as a change if a step inside it changes something.
        let changed_before = std::mem::replace(&mut self.changed, false);
        let mark = self.lines.len();
        self.status(label);
        let result = self.block(block, scope, depth + 1);
        let inner_changed = self.changed;
        self.changed = changed_before || inner_changed;
        let live = self.line.as_ref().is_some_and(TaskLine::shows_live_output);
        let indent = " ".repeat(depth * 2);
        if (inner_changed || result.is_err()) && !live {
            self.lines
                .insert(mark, format!("{indent}{}", ui::change(label)));
        } else if !inner_changed && self.mode() == Mode::DryRun {
            self.lines
                .insert(mark, format!("{indent}{}", ui::ok(label)));
        }
        result
    }

    fn run_step(&mut self, script: &str, depth: usize) -> Result<()> {
        self.changed = true;
        if self.checking() {
            for line in script.lines() {
                self.push(depth, ui::command(line));
            }
            return Ok(());
        }
        self.status(&ui::command(&first_line(script)));
        if let Some(line) = &self.line
            && line.shows_live_output()
        {
            for text in script.lines() {
                ui().output(&self.task.name, &ui::command(text));
            }
        }
        self.exec(script)
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
            sudo::ensure(&self.task.name)?;
        }
        let ctx = &self.runner.file.ctx;
        let mut command = ctx.command(script, &self.cwd);
        let status = if self.task.interactive {
            let name = &self.task.name;
            ui().suspend(|| {
                eprintln!(
                    "{} {} {}",
                    style("▶").cyan(),
                    style(name).bold(),
                    style("· interactive").dim()
                );
                command.status()
            })
        } else {
            self.piped(&mut command)
        }
        .with_context(|| format!("failed to start shell {:?}", ctx.shell[0]))?;
        if !status.success() {
            bail!("`{}` exited with {}", first_line(script), describe(status));
        }
        Ok(())
    }

    /// Run with output going to the task's line.
    fn piped(&self, command: &mut std::process::Command) -> std::io::Result<ExitStatus> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let line = self.line.as_ref();
        std::thread::scope(|s| {
            if let (Some(out), Some(line)) = (stdout, line) {
                s.spawn(move || pump(out, line));
            }
            if let (Some(err), Some(line)) = (stderr, line) {
                s.spawn(move || pump(err, line));
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
        let shown = path.display().to_string();
        let content = file_content(file, scope, &self.runner.file.ctx.dir)
            .with_context(|| format!("file {shown}"))?;
        let mode = file.mode.unwrap_or(if content.starts_with(b"#!") {
            0o755
        } else {
            0o644
        });
        let owner = file.owner.as_deref().map(|o| scope.expand(o)).transpose()?;

        let existing = std::fs::read(&path).ok();
        let mut differences = Vec::new();
        if existing.as_deref() != Some(&content) {
            differences.push(match &existing {
                None if path.exists() => "can't read the current file".to_string(),
                None => "new file".to_string(),
                Some(_) => "content".to_string(),
            });
        }
        if let Some(current) = file_mode(&path)
            && current != mode
        {
            differences.push(format!("mode {current:o} → {mode:o}"));
        }
        if let Some(wanted) = &owner
            && let Some(current) = file_owner(&path)
            && !owner_matches(&current, wanted)
        {
            differences.push(format!("owner {current} → {wanted}"));
        }
        if differences.is_empty() {
            self.current(depth, &format!("file {shown}"));
            return Ok(());
        }
        self.change(depth, &format!("file {shown} · {}", differences.join(", ")));
        match self.mode() {
            Mode::DryRun => Ok(()),
            Mode::Diff => {
                if let Some(old) = &existing
                    && old != &content
                {
                    for line in diff_lines(old, &content, file.encoding) {
                        self.push(depth + 1, line);
                    }
                }
                Ok(())
            }
            Mode::Run if file.sudo || owner.is_some() => {
                write_with_sudo(&self.task.name, &path, &content, mode, owner.as_deref())
                    .with_context(|| format!("failed to write {shown}"))
            }
            Mode::Run => write_file(&path, &content, mode)
                .with_context(|| format!("failed to write {shown}")),
        }
    }
}

fn first_line(script: &str) -> String {
    let first = script.lines().next().unwrap_or_default();
    if script.lines().count() > 1 {
        format!("{first} …")
    } else {
        first.to_string()
    }
}

/// The bytes a file step writes.
fn file_content(file: &FileStep, scope: &Scope, dir: &Path) -> Result<Vec<u8>> {
    match &file.source {
        FileSource::Inline(text) => file.encoding.encode(&scope.expand(text)?),
        FileSource::Copy(source) => {
            let source = PathBuf::from(expand_tilde(&scope.expand(source)?));
            let source = if source.is_absolute() {
                source
            } else {
                dir.join(source)
            };
            let bytes = std::fs::read(&source)
                .with_context(|| format!("failed to read {}", source.display()))?;
            if file.raw {
                return Ok(bytes);
            }
            let text = String::from_utf8(bytes).map_err(|_| {
                anyhow!(
                    "{} isn't UTF-8, so it can't be re-encoded",
                    source.display()
                )
            })?;
            file.encoding.encode(&text)
        }
    }
}

/// Feed a stream to the task's line. For `\r` progress updates, the last one wins.
fn pump(stream: impl Read, line: &TaskLine) {
    let mut reader = BufReader::new(stream);
    let mut buffer = Vec::new();
    while let Ok(n) = reader.read_until(b'\n', &mut buffer) {
        if n == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&buffer);
        let text = text.trim_end_matches(['\n', '\r']);
        line.output(text.rsplit('\r').next().unwrap_or_default());
        buffer.clear();
    }
}

/// A unified diff between the current and the new content.
fn diff_lines(old: &[u8], new: &[u8], encoding: Encoding) -> Vec<String> {
    let (Some(old_text), Some(new_text)) = (encoding.decode(old), encoding.decode(new)) else {
        let message = format!(
            "binary content differs ({} → {} bytes)",
            old.len(),
            new.len()
        );
        return vec![style(message).dim().to_string()];
    };
    let diff = similar::TextDiff::from_lines(&old_text, &new_text);
    let mut lines = Vec::new();
    for hunk in diff.unified_diff().context_radius(2).iter_hunks() {
        lines.push(style(hunk.header().to_string()).cyan().to_string());
        for change in hunk.iter_changes() {
            let text = change.value().trim_end_matches('\n');
            lines.push(match change.tag() {
                similar::ChangeTag::Delete => style(format!("-{text}")).red().to_string(),
                similar::ChangeTag::Insert => style(format!("+{text}")).green().to_string(),
                similar::ChangeTag::Equal => style(format!(" {text}")).dim().to_string(),
            });
        }
    }
    if lines.is_empty() {
        lines.push(
            style("only the encoding or a byte order mark differs")
                .dim()
                .to_string(),
        );
    }
    lines
}

fn describe(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("status {code}"),
        None => status.to_string(),
    }
}

#[cfg(unix)]
fn file_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn file_mode(_: &Path) -> Option<u32> {
    None
}

/// `user:group` of a file.
fn file_owner(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let output = std::process::Command::new("stat")
        .args(["-c", "%U:%G"])
        .arg(path)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Whether `current` (`user:group`) satisfies `wanted` (`user` or `user:group`).
fn owner_matches(current: &str, wanted: &str) -> bool {
    if wanted.contains(':') {
        current == wanted
    } else {
        current.split(':').next() == Some(wanted)
    }
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

/// Write a file as root with `sudo install`, which also creates parent
/// directories and sets the owner.
fn write_with_sudo(
    task: &str,
    path: &Path,
    content: &[u8],
    mode: u32,
    owner: Option<&str>,
) -> Result<()> {
    sudo::ensure(task)?;
    let mut command = std::process::Command::new("sudo");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_text() {
        assert_eq!(Encoding::Utf8.encode("é").unwrap(), "é".as_bytes());
        assert_eq!(
            Encoding::Utf8Bom.encode("a").unwrap(),
            [0xEF, 0xBB, 0xBF, b'a']
        );
        assert_eq!(
            Encoding::Utf16Le.encode("a").unwrap(),
            [0xFF, 0xFE, b'a', 0]
        );
        assert_eq!(
            Encoding::Utf16Be.encode("a").unwrap(),
            [0xFE, 0xFF, 0, b'a']
        );
        assert_eq!(Encoding::Latin1.encode("é").unwrap(), [0xE9]);
        assert!(Encoding::Latin1.encode("€").is_err());
        assert!(Encoding::Ascii.encode("é").is_err());
        for encoding in [
            Encoding::Utf8,
            Encoding::Utf16Le,
            Encoding::Utf16Be,
            Encoding::Latin1,
        ] {
            let bytes = encoding.encode("héllo\n").unwrap();
            assert_eq!(
                encoding.decode(&bytes).as_deref(),
                Some("héllo\n"),
                "{encoding:?}"
            );
        }
    }

    #[test]
    fn owners() {
        assert!(owner_matches("root:root", "root"));
        assert!(owner_matches("root:wheel", "root:wheel"));
        assert!(!owner_matches("root:root", "root:wheel"));
        assert!(!owner_matches("me:me", "root"));
    }

    #[test]
    fn diffs() {
        console::set_colors_enabled(false);
        let lines = diff_lines(b"a\nb\nc\n", b"a\nB\nc\n", Encoding::Utf8);
        assert_eq!(lines, ["@@ -1,3 +1,3 @@", " a", "-b", "+B", " c"]);
    }
}
