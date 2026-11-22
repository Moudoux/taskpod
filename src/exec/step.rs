//! Running the steps of one task.

mod adopt;

use super::journal::{self, Entry, Previous};
use super::target::{self, Current, DirState, describe};
use super::{Mode, Outcome, Runner, StepKind, StepRecord, TaskReport, pull, sudo};
use crate::ast::{
    Block, CallArgs, Encoding, FileSource, FileStep, Guard, LinePlace, LineStep, SettingStep, Step,
    Task, Template, WaitStep,
};
use crate::parser::{check_passes, guard_passes};
use crate::ui::{self, TaskLine, ui};
use crate::vars::{Ctx, Scope, Vars, expand_tilde};
use anyhow::{Context, Result, anyhow, bail};
use console::style;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read};
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
    /// Whether a step before the current block changed something, for
    /// `changed` guards: `changed` only counts the block's own steps.
    changed_before: bool,
    /// The live line, when running for real.
    line: Option<TaskLine>,
    /// Lines shown under the task's status line when it's done: the plan in
    /// checks, what changed in a live display.
    lines: Vec<String>,
    /// The same steps as data, for `--json`.
    report: TaskReport,
    failed: bool,
    /// How many of the blocks running now say `terminal` or `sudo`: their
    /// commands get the terminal when they ask for it.
    terminal: usize,
}

impl<'r> TaskRun<'r> {
    pub fn new(runner: &'r Runner<'r>, task: &'r Task) -> Self {
        TaskRun {
            runner,
            task,
            slot: Slot::take(runner),
            cwd: PathBuf::new(),
            changed: false,
            changed_before: false,
            terminal: 0,
            line: None,
            lines: Vec::new(),
            report: TaskReport::default(),
            failed: false,
        }
    }

    fn mode(&self) -> Mode {
        self.runner.opts.mode
    }

    /// Only looking: a check or a pull, nothing runs.
    fn checking(&self) -> bool {
        self.runner.opts.checking()
    }

    /// Whether reads may go through sudo: when running for real, and in
    /// checks with `--root`. Steps that don't write as root never need it.
    fn reads_as_root(&self) -> bool {
        self.mode() == Mode::Run || self.runner.opts.root
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
        let root = self.runner.root_scope();
        let task_scope = root.child(&self.task.vars);
        let tmp = crate::vars::tmp_vars(&task_scope, self.checking());
        let scope = task_scope.child(&tmp);
        let result = self.run_task(&scope);
        self.remove_tmp(&tmp, result)
    }

    /// Remove a `{{ tmp }}` directory once what used it is done. Failing to
    /// is said, but the result of what used it matters more.
    fn remove_tmp<T>(&mut self, tmp: &Vars, result: Result<T>) -> Result<T> {
        if let Err(e) = crate::vars::remove_tmp(tmp, &self.runner.file.ctx)
            && result.is_ok()
        {
            self.push(0, style(format!("{e:#}")).dim().to_string());
        }
        result
    }

    fn run_task(&mut self, scope: &Scope) -> Result<Outcome> {
        let scope = *scope;
        self.cwd = self.runner.workdir(self.task, &scope)?;
        self.status("checking");
        if self.runner.opts.undo {
            return self.undo_task(&scope);
        }
        if self.runner.opts.adopt {
            return self.adopt_task(&scope);
        }
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

    pub fn take_report(&mut self) -> TaskReport {
        std::mem::take(&mut self.report)
    }

    fn record(&mut self, depth: usize, kind: StepKind, label: &str) {
        self.report.steps.push(StepRecord {
            depth,
            kind,
            label: label.to_string(),
            diff: None,
        });
    }

    /// Attach diff lines to the last step; lines that aren't part of a
    /// unified diff (notes about binary content or hidden secrets) become notes.
    fn record_diff(&mut self, depth: usize, lines: &[String]) {
        let mut diff = Vec::new();
        let mut notes = Vec::new();
        for line in lines {
            let plain = console::strip_ansi_codes(line).into_owned();
            if plain.starts_with(['@', '+', '-', ' ']) {
                diff.push(plain);
            } else {
                notes.push(plain);
            }
        }
        if let Some(step) = self.report.steps.last_mut()
            && !diff.is_empty()
        {
            step.diff = Some(diff);
        }
        for note in notes {
            self.record(depth, StepKind::Note, &note);
        }
    }

    /// A step that needs secrets, in a check with `--no-secrets`.
    fn unchecked(&mut self, depth: usize, label: &str) {
        self.report.unchecked += 1;
        self.record(depth, StepKind::Unchecked, label);
        self.push(
            depth,
            style(format!("? {label} · uses secrets, not checked"))
                .dim()
                .to_string(),
        );
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
        self.record(depth, StepKind::Change, label);
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
        self.record(depth, StepKind::Ok, label);
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
        self.enter(block, scope, depth, Self::block_steps)
    }

    /// Run `steps` on a block with its locks held, sudo ready and the
    /// terminal given to commands that ask, if it says so.
    fn enter(
        &mut self,
        block: &'r Block,
        scope: &Scope,
        depth: usize,
        steps: fn(&mut Self, &'r Block, &Scope, usize) -> Result<()>,
    ) -> Result<()> {
        let mut _locks: Vec<MutexGuard<'r, ()>> = Vec::new();
        if !self.checking() {
            for name in &block.locks {
                self.status(&format!("waiting for lock {name}"));
                _locks.push(self.lock(name));
            }
            if block.sudo {
                sudo::ensure(&self.task.name, &self.runner.file.ctx)?;
            }
        }
        let terminal = block.sudo || block.terminal;
        self.terminal += usize::from(terminal);
        let result = steps(self, block, scope, depth);
        self.terminal -= usize::from(terminal);
        result
    }

    fn block_steps(&mut self, block: &'r Block, scope: &Scope, depth: usize) -> Result<()> {
        for step in &block.steps {
            let result = self.step(step, scope, depth);
            // Checks with `--no-secrets` go on past steps that need secrets.
            if let Err(e) = &result
                && self.checking()
                && crate::secrets::not_checked(e)
            {
                // The label as written names secrets, never their values.
                self.unchecked(depth, &first_line(&step.describe()));
                continue;
            }
            result?;
        }
        Ok(())
    }

    fn step(&mut self, step: &'r Step, scope: &Scope, depth: usize) -> Result<()> {
        match step {
            Step::Run(script) => self.run_step(&scope.expand(script)?, depth),
            Step::Setting(setting) => self.setting(setting, scope, depth),
            // Only `--undo` runs these, see `undo_block`.
            Step::Undo(_) => Ok(()),
            Step::Notice(text) => {
                self.notice(&scope.expand(text)?, depth);
                Ok(())
            }
            Step::Wait(wait) => self.wait(wait, scope, depth),
            Step::File(file) => self.file(file, scope, depth),
            Step::Line(line) => self.line(line, scope, depth),
            Step::Call { template, args } => {
                let call = Call::new(&self.runner.file.templates[template], args, scope)?;
                let call_scope = scope.child(&call.args);
                let inner = call_scope.child(&call.vars);
                let tmp = crate::vars::tmp_vars(&inner, self.checking());
                let inner = inner.child(&tmp);
                let result = self.nested(&call.template.body, &inner, depth, &call.label);
                self.remove_tmp(&tmp, result)
            }
            Step::Block(inner) => {
                let label = inner
                    .guards
                    .first()
                    .map_or_else(|| "block".into(), Guard::describe);
                let label = scope.expand(&label).unwrap_or(label);
                self.nested(inner, scope, depth, &label)
            }
        }
    }

    /// Where the steps of a template or inline block run, if it says
    /// (`workdir`), relative to the directory around it. Its own guards are
    /// checked around it: they say whether to go in.
    fn block_cwd(&self, block: &Block, scope: &Scope) -> Result<Option<PathBuf>> {
        let Some(dir) = &block.workdir else {
            return Ok(None);
        };
        let ctx = &self.runner.file.ctx;
        let dir = PathBuf::from(ctx.target_path(&scope.expand(dir)?));
        Ok(Some(if dir.is_absolute() {
            dir
        } else {
            self.cwd.join(dir)
        }))
    }

    /// Run `f` in a block's `workdir`, if it has one.
    fn in_workdir<T>(
        &mut self,
        block: &Block,
        scope: &Scope,
        f: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let Some(dir) = self.block_cwd(block, scope)? else {
            return f(self);
        };
        let outer = std::mem::replace(&mut self.cwd, dir);
        let result = f(self);
        self.cwd = outer;
        result
    }

    /// A template call or inline block: check its guards, then run it.
    fn nested(&mut self, block: &'r Block, scope: &Scope, depth: usize, label: &str) -> Result<()> {
        self.status(&format!("checking {label}"));
        let undo = self.runner.opts.undo;
        if undo && block.keep && block.otherwise.is_none() {
            self.current(depth, &format!("{label} · kept"));
            return Ok(());
        }
        let nothing_changed = !self.runner.opts.ignore_guards
            && block.guards.iter().any(|g| matches!(g, Guard::Changed))
            && !self.changed
            && !self.changed_before;
        let skip = if undo {
            self.undo_guard(&block.guards, scope)?.is_some()
        } else {
            nothing_changed
                || self
                    .runner
                    .failing_guard(&block.guards, scope, &self.cwd)?
                    .is_some()
        };
        if skip {
            // `} else {`: only after an `if`, whose check ran once, here.
            if let Some(other) = &block.otherwise {
                return self.nested(other, scope, depth, &format!("{label} · else"));
            }
            self.current(depth, label);
            return Ok(());
        }
        if undo && block.keep {
            self.current(depth, &format!("{label} · kept"));
            return Ok(());
        }
        // The block only counts as a change if a step inside it changes something.
        let changed_before = std::mem::replace(&mut self.changed, false);
        let outer_changed = self.changed_before;
        self.changed_before |= changed_before;
        let mark = self.lines.len();
        let step_mark = self.report.steps.len();
        let unchecked_before = self.report.unchecked;
        self.status(label);
        let result = self.in_workdir(block, scope, |this| {
            if undo {
                this.undo_block(block, scope, depth + 1)
            } else {
                this.block(block, scope, depth + 1)
            }
        });
        let inner_changed = self.changed;
        self.changed = changed_before || inner_changed;
        self.changed_before = outer_changed;
        let live = self.line.as_ref().is_some_and(TaskLine::shows_live_output);
        let indent = " ".repeat(depth * 2);
        if (inner_changed || result.is_err()) && !live {
            self.lines
                .insert(mark, format!("{indent}{}", ui::change(label)));
        } else if !inner_changed && self.mode() == Mode::DryRun {
            self.lines
                .insert(mark, format!("{indent}{}", ui::ok(label)));
        }
        let kind = if inner_changed || result.is_err() {
            StepKind::Change
        } else if self.report.unchecked > unchecked_before {
            StepKind::Unchecked
        } else {
            StepKind::Ok
        };
        let record = StepRecord {
            depth,
            kind,
            label: label.to_string(),
            diff: None,
        };
        self.report.steps.insert(step_mark, record);
        result
    }

    fn run_step(&mut self, script: &str, depth: usize) -> Result<()> {
        // Commands can't be pulled back.
        if self.runner.opts.pull {
            return Ok(());
        }
        self.changed = true;
        self.record(depth, StepKind::Run, script.trim_end());
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
        let ctx = &self.runner.file.ctx;
        let mentions_sudo = sudo::mentions_sudo(script);
        if mentions_sudo {
            sudo::ensure(&self.task.name, ctx)?;
        }
        let ask = self.terminal > 0 || mentions_sudo;
        let status = if self.task.interactive {
            let mut command = ctx.interactive_command(script, &self.cwd)?;
            crate::tty::clean(&mut command);
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
            let (status, wanted) = self
                .piped(ctx.command(script, &self.cwd), script, ask)
                .with_context(|| format!("failed to start {:?}", ctx.program()))?;
            if let Some(process) = wanted {
                let wanted = crate::tty::wanted(&process);
                let first = first_line(script);
                if ask {
                    bail!("`{first}`: {wanted}, but taskpod has no terminal to give it");
                }
                bail!(
                    "`{first}`: {wanted}, but this step can't ask. Add `terminal` to its task \
                     or block to give it the terminal when it asks, or make the task `interactive`"
                );
            }
            Ok(status)
        }
        .with_context(|| format!("failed to start {:?}", ctx.program()))?;
        if !status.success() {
            bail!("`{}` exited with {}", first_line(script), describe(status));
        }
        Ok(())
    }

    /// Run with output going to the task's line. If it stops to use the
    /// terminal and `ask`, it's given the terminal once nothing else has it,
    /// until it exits; otherwise it's ended, and the process that wanted
    /// the terminal is returned with its exit status.
    fn piped(
        &mut self,
        mut spawn: crate::vars::Spawn,
        script: &str,
        ask: bool,
    ) -> std::io::Result<(ExitStatus, Option<String>)> {
        spawn.command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let (mut child, job) = spawn.spawn()?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (sender, events) = std::sync::mpsc::channel();
        if ask {
            job.on_stop(sender.clone());
        }
        let output = std::sync::Mutex::new(Output::Line);
        let (line, slot, output) = (self.line.as_ref(), &mut self.slot, &output);
        let (name, first) = (self.task.name.as_str(), first_line(script));
        let status = std::thread::scope(|s| {
            if let (Some(out), Some(line)) = (stdout, line) {
                s.spawn(move || pump(out, line, output));
            }
            if let (Some(err), Some(line)) = (stderr, line) {
                s.spawn(move || pump(err, line, output));
            }
            s.spawn(move || sender.send(crate::tty::Event::Exited(child.wait())));
            let wait = || loop {
                match events.recv() {
                    Ok(crate::tty::Event::Exited(status)) => return status,
                    Ok(crate::tty::Event::Stopped) => job.resume(),
                    Err(_) => unreachable!("the waiter sends before it ends"),
                }
            };
            loop {
                match events.recv() {
                    Ok(crate::tty::Event::Exited(status)) => return status,
                    Ok(crate::tty::Event::Stopped) if crate::tty::can_hand_over() => {
                        // Waiting for the terminal can take a while: let other tasks run.
                        slot.release();
                        Output::set(output, Output::Held(Vec::new()));
                        let status = ui().suspend(|| {
                            let process = job.wanted().unwrap_or_default();
                            let what = format!("· {first} · {process} asks on the terminal");
                            eprintln!(
                                "{} {} {}",
                                style("▶").cyan(),
                                style(name).bold(),
                                style(ui::redact(&what)).dim()
                            );
                            if let Output::Held(lines) = Output::set(output, Output::Direct) {
                                for text in lines {
                                    eprintln!("{text}");
                                }
                            }
                            let Some(_terminal) = job.foreground() else {
                                job.kill();
                                return wait();
                            };
                            wait()
                        });
                        Output::set(output, Output::Line);
                        slot.reacquire();
                        return status;
                    }
                    Ok(crate::tty::Event::Stopped) => job.kill(),
                    Err(_) => unreachable!("the waiter sends before it ends"),
                }
            }
        })?;
        Ok((status, job.finish()))
    }

    fn file(&mut self, file: &FileStep, scope: &Scope, depth: usize) -> Result<()> {
        if file.dir {
            return self.dir(file, scope, depth);
        }
        let ctx = &self.runner.file.ctx;
        let path = self.target(&scope.expand(&file.path)?);
        let shown = path.display().to_string();
        let owner = file.owner.as_deref().map(|o| scope.expand(o)).transpose()?;
        // A step that writes as root reads as root too, when running for
        // real: a file only root can read is otherwise rewritten every run.
        let as_root = self.reads_as_root() && (file.sudo || owner.is_some());
        // What's on the system, when it was already read to check a download.
        let mut read = None;
        let content = match &file.source {
            // Nothing to pull: the content is the download's.
            FileSource::Url { .. } if self.runner.opts.pull => {
                self.current(depth, &format!("file {shown} · downloaded"));
                return Ok(());
            }
            FileSource::Url { url, sha256 } => {
                // Already the right file: no need to download it again.
                let current = target::read(ctx, &self.task.name, &path, as_root)?;
                let bytes = match &current {
                    Current::Content(bytes) if sha256_hex(bytes) == *sha256 => bytes.clone(),
                    _ => download(&scope.expand(url)?, sha256)
                        .with_context(|| format!("file {shown}"))?,
                };
                read = Some(current);
                bytes
            }
            _ => file_content(file, scope, source_dir(file, &ctx.dir))
                .with_context(|| format!("file {shown}"))?,
        };
        let mode = file.mode.unwrap_or(if content.starts_with(b"#!") {
            0o755
        } else {
            0o644
        });
        if self.runner.opts.pull {
            return self.pull_file(file, scope, depth, &path, &content, mode, owner.as_deref());
        }
        if self.runner.opts.undo {
            return self.undo_file(file, depth, &path, &content, owner.as_deref());
        }

        let current = match read {
            Some(current) => current,
            None => target::read(ctx, &self.task.name, &path, as_root)?,
        };
        // Unreadable, or not even seen (in a directory only root can
        // enter): there may be a file, so it's never taken for a new one.
        let missing = current == Current::Missing;
        let existing = match current {
            Current::Content(bytes) => Some(bytes),
            Current::Missing | Current::Unreadable => None,
        };
        let mut differences = Vec::new();
        let stat = target::stat(ctx, &self.task.name, &path, as_root)?;
        if existing.as_deref() != Some(&content) {
            differences.push(match &existing {
                None if stat.is_some() || !missing => "can't read the current file".to_string(),
                None => "new file".to_string(),
                Some(old) if file.raw => content_difference(old, &content, None),
                Some(old) => content_difference(old, &content, Some(file.encoding)),
            });
        }
        if let Some((current, _)) = &stat
            && *current != mode
        {
            differences.push(format!("mode {current:o} → {mode:o}"));
        }
        if let Some(wanted) = &owner
            && let Some((_, current)) = &stat
            && !owner_matches(current, wanted)
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
                    let lines = diff_lines(old, &content, file.encoding);
                    self.record_diff(depth + 1, &lines);
                    for line in lines {
                        self.push(depth + 1, line);
                    }
                }
                Ok(())
            }
            Mode::Run => {
                self.remember(file, &path, existing, missing, stat, owner.is_some())?;
                target::write(
                    ctx,
                    &self.task.name,
                    &path,
                    &content,
                    mode,
                    owner.as_deref(),
                    file.sudo,
                )
                .with_context(|| format!("failed to write {shown}"))
            }
        }
    }

    /// `directory "path" from "dir"`: a file step for each file in the
    /// source directory, at the same place under the step's path. Files on
    /// the system that the source doesn't have are left alone.
    fn dir(&mut self, file: &FileStep, scope: &Scope, depth: usize) -> Result<()> {
        if let FileSource::Empty = file.source {
            return self.make_dir(file, scope, depth);
        }
        let FileSource::Copy(source) = &file.source else {
            bail!("directory {}: needs `from` a directory", file.path);
        };
        let join = |base: &str, name: &str| format!("{}/{name}", base.trim_end_matches('/'));
        let dir = copy_source(source, scope, source_dir(file, &self.runner.file.ctx.dir))?;
        let mut names = Vec::new();
        list_files(&dir, "", &mut names)
            .with_context(|| format!("failed to list {}", dir.display()))?;
        if names.is_empty() {
            bail!("{} has no files to copy", dir.display());
        }
        for name in &names {
            // The names are literal text: escape `{{` so it isn't expanded.
            let name = name.replace("{{", "\\{{");
            let step = FileStep {
                path: join(&file.path, &name),
                dir: false,
                source: FileSource::Copy(join(source, &name)),
                ..file.clone()
            };
            self.file(&step, scope, depth)?;
        }
        if self.runner.opts.pull {
            self.pull_new_files(file, scope, depth, &dir, &names)?;
        }
        Ok(())
    }

    /// `directory "path"` with no `from`: the directory exists, with its mode
    /// (default 755) and owner. The journal records whether taskpod created
    /// it, or its mode and owner before (as a file entry with no content).
    fn make_dir(&mut self, file: &FileStep, scope: &Scope, depth: usize) -> Result<()> {
        let ctx = &self.runner.file.ctx;
        let task = self.task.name.clone();
        let path = self.target(&scope.expand(&file.path)?);
        let label = format!("directory {}", path.display());
        let mode = file.mode.unwrap_or(0o755);
        let owner = file.owner.as_deref().map(|o| scope.expand(o)).transpose()?;
        let root = file.sudo || owner.is_some();
        // Nothing to pull: the step holds no content.
        if self.runner.opts.pull {
            self.current(depth, &label);
            return Ok(());
        }
        if self.runner.opts.undo {
            return self.undo_dir(file, depth, &path, root);
        }
        let state = target::dir_state(ctx, &task, &path, false)?;
        if state == DirState::NotDir {
            bail!("{} exists and isn't a directory", path.display());
        }
        let as_root = self.reads_as_root() && root;
        let stat = target::stat(ctx, &task, &path, as_root)?.filter(|_| state != DirState::Missing);
        // Not seen, in a directory only root can enter: it may be there.
        let unseen = stat.is_none() && state == DirState::Unlistable;
        let mut differences = Vec::new();
        match &stat {
            None if unseen => differences.push("can't see the current directory".to_string()),
            None => differences.push("new directory".to_string()),
            Some((current, current_owner)) => {
                if *current != mode {
                    differences.push(format!("mode {current:o} → {mode:o}"));
                }
                if let Some(wanted) = &owner
                    && !owner_matches(current_owner, wanted)
                {
                    differences.push(format!("owner {current_owner} → {wanted}"));
                }
            }
        }
        if differences.is_empty() {
            self.current(depth, &label);
            return Ok(());
        }
        self.change(depth, &format!("{label} · {}", differences.join(", ")));
        if self.mode() != Mode::Run {
            return Ok(());
        }
        let previous = match stat {
            None if unseen => None,
            None => Some(Previous::Missing),
            Some((mode, owner)) => Some(Previous::File {
                content: Vec::new(),
                mode,
                owner,
                mtime: None,
            }),
        };
        if let Some(previous) = &previous {
            journal::record(ctx, &task, &path, root, previous)?;
        }
        target::make_dir(ctx, &task, &path, mode, owner.as_deref(), file.sudo)
            .with_context(|| format!("failed to make {}", path.display()))
    }

    /// Undo a `directory` without `from`: remove it if taskpod created it and
    /// it's empty, or put its mode and owner back.
    fn undo_dir(&mut self, file: &FileStep, depth: usize, path: &Path, root: bool) -> Result<()> {
        let ctx = &self.runner.file.ctx;
        let task = self.task.name.clone();
        let label = format!("directory {}", path.display());
        if file.keep {
            self.current(depth, &format!("{label} · kept"));
            return Ok(());
        }
        let write = !self.checking();
        let entry = journal::read(ctx, &task, path, root, self.reads_as_root() && root)?;
        let recorded = entry != Entry::None;
        let previous = match entry {
            Entry::Some(previous) => Some(previous),
            Entry::Unreadable => None,
            Entry::None if file.new => Some(Previous::Missing),
            Entry::None => {
                if target::stat(ctx, &task, path, self.reads_as_root() && root)?.is_none() {
                    self.current(depth, &format!("{label} · not on the system"));
                } else {
                    let text = format!(
                        "{label}: no record of it before taskpod; mark it `new` if taskpod created it, or `keep`"
                    );
                    self.cannot(depth, &text);
                }
                return Ok(());
            }
        };
        let forget = |runner: &Runner| -> Result<()> {
            if write && recorded {
                journal::remove(&runner.file.ctx, &task, path, root)?;
            }
            Ok(())
        };
        let state = target::dir_state(ctx, &task, path, self.reads_as_root() && root)?;
        match previous {
            None => {
                self.change(
                    depth,
                    &format!("{label} · put back what was there before taskpod{UNREAD_JOURNAL}"),
                );
                if write {
                    bail!("can't read the undo journal's entry for {}", path.display());
                }
                Ok(())
            }
            Some(Previous::Missing) => match state {
                DirState::Missing => {
                    self.current(depth, &format!("{label} · already gone"));
                    forget(self.runner)
                }
                DirState::NotDir => {
                    self.cannot(
                        depth,
                        &format!("{label}: it isn't a directory any more, left as it is"),
                    );
                    Ok(())
                }
                DirState::NotEmpty => {
                    self.cannot(
                        depth,
                        &format!("{label}: taskpod created it, but it isn't empty, left as it is"),
                    );
                    Ok(())
                }
                DirState::Empty | DirState::Unlistable => {
                    let note = if state == DirState::Unlistable {
                        " if it's empty"
                    } else {
                        ""
                    };
                    self.change(
                        depth,
                        &format!("{label} · delete it{note}, taskpod created it"),
                    );
                    if !write {
                        return Ok(());
                    }
                    if state == DirState::Unlistable {
                        self.cannot(
                            depth,
                            &format!("{label}: can't see if it's empty, left as it is"),
                        );
                        return Ok(());
                    }
                    target::remove_dir(ctx, &task, path, root)
                        .with_context(|| format!("failed to undo {}", path.display()))?;
                    forget(self.runner)
                }
            },
            Some(Previous::File { mode, owner, .. }) => {
                let current = target::stat(ctx, &task, path, self.reads_as_root() && root)?;
                if current
                    .as_ref()
                    .is_some_and(|(m, o)| *m == mode && (!root || *o == owner))
                {
                    self.current(depth, &format!("{label} · already as it was"));
                    return forget(self.runner);
                }
                if current.is_none() {
                    self.current(depth, &format!("{label} · not on the system"));
                    return Ok(());
                }
                self.change(
                    depth,
                    &format!(
                        "{label} · put back mode {mode:o}{}",
                        if root {
                            format!(", owner {owner}")
                        } else {
                            String::new()
                        }
                    ),
                );
                if !write {
                    return Ok(());
                }
                target::make_dir(
                    ctx,
                    &task,
                    path,
                    mode,
                    root.then_some(owner.as_str()),
                    file.sudo,
                )
                .with_context(|| format!("failed to undo {}", path.display()))?;
                forget(self.runner)
            }
        }
    }

    /// A path on the machine being configured, relative ones from the workdir.
    fn target(&self, path: &str) -> PathBuf {
        let path = PathBuf::from(self.runner.file.ctx.target_path(path));
        if path.is_absolute() {
            path
        } else {
            self.cwd.join(path)
        }
    }

    /// The paths `file` and `line` steps in `block` write, as far as they
    /// can be worked out without running anything.
    fn written_paths(&self, block: &Block, scope: &Scope, out: &mut HashSet<PathBuf>) {
        for step in &block.steps {
            match step {
                Step::File(file) if !file.dir => {
                    if let Ok(path) = scope.expand(&file.path) {
                        out.insert(self.target(&path));
                    }
                }
                Step::Line(line) => {
                    if let Ok(path) = scope.expand(&line.path) {
                        out.insert(self.target(&path));
                    }
                }
                Step::Block(inner) => {
                    self.written_paths(inner, scope, out);
                    if let Some(other) = &inner.otherwise {
                        self.written_paths(other, scope, out);
                    }
                }
                Step::Call { template, args } => {
                    let Ok(call) = Call::new(&self.runner.file.templates[template], args, scope)
                    else {
                        continue;
                    };
                    let call_scope = scope.child(&call.args);
                    self.written_paths(&call.template.body, &call_scope.child(&call.vars), out);
                }
                _ => {}
            }
        }
    }

    /// `--pull` for a directory: files on the system that the source
    /// directory doesn't have are copied into it.
    fn pull_new_files(
        &mut self,
        file: &FileStep,
        scope: &Scope,
        depth: usize,
        source: &Path,
        names: &[String],
    ) -> Result<()> {
        let ctx = &self.runner.file.ctx;
        let task = self.task.name.clone();
        let dir = self.target(&scope.expand(&file.path)?);
        // Files other steps of the task write, like one written inline from
        // secrets, aren't the directory's to pull.
        let mut written = HashSet::new();
        self.written_paths(
            &self.task.body,
            &self.runner.root_scope().child(&self.task.vars),
            &mut written,
        );
        let as_root = self.reads_as_root() && file.sudo;
        // The source's own directories: the top, and every one on the way
        // to one of its files.
        let mut subdirs = vec![String::new()];
        for name in names {
            subdirs.extend(name.match_indices('/').map(|(i, _)| name[..i].to_string()));
        }
        subdirs.sort();
        subdirs.dedup();
        let system = match target::list(ctx, &task, &dir, &subdirs, as_root) {
            Ok(system) => system,
            Err(e) => {
                let hint = if file.sudo {
                    ""
                } else {
                    ", add `sudo` to list it as root"
                };
                let text = format!("directory {} · {e:#}{hint}", dir.display());
                self.record(depth, StepKind::Change, &text);
                self.push(depth, ui::change(&text));
                return Ok(());
            }
        };
        for name in system.iter().filter(|n| !names.contains(n)) {
            let path = dir.join(name);
            if written.contains(&path) {
                continue;
            }
            let target = source.join(name);
            let label = format!("file {} · only on the system", path.display());
            let refuse = |this: &mut Self, why: &str| {
                this.changed = true;
                this.record(depth, StepKind::Change, &label);
                this.push(depth, ui::change(&label));
                let note = style(format!("not pulled: {why}")).yellow().to_string();
                this.record_diff(depth + 1, std::slice::from_ref(&note));
                this.push(depth + 1, note);
            };
            let bytes = match target::read(ctx, &task, &path, as_root)? {
                Current::Content(bytes) => bytes,
                _ => {
                    refuse(self, "can't read it");
                    continue;
                }
            };
            let bytes = if file.raw {
                bytes
            } else {
                match file.encoding.decode(&bytes) {
                    Some(text) => text.into_bytes(),
                    None => {
                        refuse(self, &format!("it isn't valid {}", file.encoding.name()));
                        continue;
                    }
                }
            };
            if std::str::from_utf8(&bytes).is_ok_and(pull::has_secret) {
                refuse(self, "it holds a secret value");
                continue;
            }
            let label = format!("{label}, new in {}", target.display());
            self.change(depth, &label);
            let shown = match self.mode() {
                Mode::DryRun => Vec::new(),
                Mode::Diff | Mode::Run => diff_lines(&[], &bytes, Encoding::Utf8),
            };
            let edit = pull::Edit::Copy {
                file: target,
                bytes,
            };
            self.offer_pull(depth, &label, shown, vec![edit])?;
        }
        Ok(())
    }

    /// `line`: put the line in place if it isn't, recording in the journal
    /// what taskpod changed; with `--undo`, put that back. The rest of the
    /// file, its mode and owner stay as they are.
    fn line(&mut self, step: &LineStep, scope: &Scope, depth: usize) -> Result<()> {
        let ctx = &self.runner.file.ctx;
        let line = ResolvedLine::new(step, scope, ctx, &self.cwd)?;
        let label = format!("line {}", line.path.display());
        if self.runner.opts.undo {
            if step.keep {
                self.current(depth, &format!("{label} · kept"));
                return Ok(());
            }
            return self.undo_line_step(step, &line, depth, &label);
        }
        let as_root = step.sudo
            && (self.mode() == Mode::Run && !self.runner.opts.pull || self.runner.opts.root);
        let Some((existing, base)) = self.read_line_files(step, &line, as_root, depth, &label)?
        else {
            return Ok(());
        };
        let old = match (&existing, &base) {
            (Some(old), _) => old.clone(),
            (None, Some(base)) => base.clone(),
            (None, None) => match &line.base {
                Some(b) => bail!(
                    "{} doesn't exist, nor does its base {}",
                    line.path.display(),
                    b.display()
                ),
                None => bail!("{} doesn't exist", line.path.display()),
            },
        };
        let text = &line.text;
        // The line taskpod last wrote for this step: after a `match` replaced a
        // commented-out default, the match no longer finds it, but this does.
        // `None` in a check that can't read the root journal, or when the file
        // is gone (its records are stale).
        let ctx = &self.runner.file.ctx;
        let task = self.task.name.clone();
        let key = line.key();
        let journal = match existing {
            Some(_) => journal::read_lines(ctx, &task, &line.path, step.sudo, as_root)?,
            None => None,
        };
        let previous = journal
            .as_ref()
            .and_then(|j| j.get(&key))
            .map(|r| r.text.clone());
        let Some((new, change)) = ensure_line(
            &old,
            text,
            line.matcher.as_deref(),
            previous.as_deref(),
            line.place.as_ref(),
        )?
        else {
            // In the file, but recorded as an older text (edited to this one):
            // record this text, so the line is still found once it changes.
            if self.mode() == Mode::Run
                && !self.runner.opts.pull
                && previous.as_ref().is_some_and(|p| p != text)
                && let Some(mut lines) = journal
            {
                for record in lines.lines.iter_mut().filter(|r| r.key == key) {
                    record.text = text.clone();
                }
                journal::write_lines(ctx, &task, &line.path, step.sudo, &lines)?;
            }
            self.current(depth, &format!("{label} · in the file"));
            return Ok(());
        };
        if self.runner.opts.pull {
            let text = format!("{label} · not as the step says, nothing to pull");
            self.record(depth, StepKind::Ok, &text);
            self.push(depth, ui::ok(&text));
            return Ok(());
        }
        let mut what = match &change {
            LineChange::Added => format!("adds {text:?}"),
            LineChange::Replaced(was) => format!("replaces {was:?} with {text:?}"),
            LineChange::Moved => match &line.place {
                Some(LinePlace::Before(p)) => format!("moves {text:?} before {p:?}"),
                Some(LinePlace::After(p)) => format!("moves {text:?} after {p:?}"),
                None => format!("moves {text:?}"),
            },
        };
        if existing.is_none() {
            let base = step.base.as_deref().unwrap_or_default();
            what = format!("new file from {base}, {what}");
        }
        self.change(depth, &format!("{label} · {what}"));
        match self.mode() {
            Mode::DryRun => Ok(()),
            Mode::Diff => {
                let old = existing.unwrap_or_default();
                self.show_diff(depth, &old, &new);
                Ok(())
            }
            Mode::Run => {
                let task = self.task.name.clone();
                let root = step.sudo;
                // What taskpod changed, before changing it.
                let recorded =
                    journal::read_lines(ctx, &task, &line.path, root, root)?.unwrap_or_default();
                let mut lines = if existing.is_none() {
                    // Records of a file that's gone since are stale.
                    journal::Lines {
                        created: true,
                        base: base.clone(),
                        lines: Vec::new(),
                    }
                } else {
                    recorded.clone()
                };
                let key = line.key();
                match lines.lines.iter_mut().find(|l| l.key == key) {
                    Some(record) => record.text = text.clone(),
                    None => {
                        let replaced = match &change {
                            LineChange::Added => Some(None),
                            LineChange::Replaced(was) => Some(Some(was.clone())),
                            // It was there before taskpod, only in the wrong place.
                            LineChange::Moved => None,
                        };
                        if let Some(replaced) = replaced {
                            lines.lines.push(journal::LineRecord {
                                key,
                                text: text.clone(),
                                replaced,
                            });
                        }
                    }
                }
                if lines != recorded {
                    journal::write_lines(ctx, &task, &line.path, root, &lines)?;
                }
                // A new file gets its base's mode and owner.
                let from = if existing.is_some() {
                    Some(&line.path)
                } else {
                    line.base.as_ref()
                };
                self.write_line_file(step, &line.path, from, &new)
            }
        }
    }

    /// The step's file and its base, as text; `None` (reported) if one can't be read.
    #[allow(clippy::type_complexity)]
    fn read_line_files(
        &mut self,
        step: &LineStep,
        line: &ResolvedLine,
        as_root: bool,
        depth: usize,
        label: &str,
    ) -> Result<Option<(Option<String>, Option<String>)>> {
        let ctx = &self.runner.file.ctx;
        let task = self.task.name.clone();
        let mut contents = Vec::new();
        for p in std::iter::once(&line.path).chain(&line.base) {
            match read_text(ctx, &task, p, as_root)? {
                Some(text) => contents.push(text),
                None => {
                    let hint = if step.sudo {
                        ""
                    } else {
                        ", add `sudo` to read it as root"
                    };
                    let text = format!("{label} · can't read {}{hint}", p.display());
                    self.record(depth, StepKind::Change, &text);
                    self.push(depth, ui::change(&text));
                    return Ok(None);
                }
            }
        }
        let existing = contents.remove(0);
        Ok(Some((existing, contents.pop().flatten())))
    }

    /// Write a `line` step's file with the mode and owner of `from`.
    fn write_line_file(
        &self,
        step: &LineStep,
        path: &Path,
        from: Option<&PathBuf>,
        content: &str,
    ) -> Result<()> {
        let ctx = &self.runner.file.ctx;
        let (mode, owner) = match from {
            Some(p) => target::stat(ctx, &self.task.name, p, step.sudo)?,
            None => None,
        }
        .ok_or_else(|| anyhow!("can't stat {}", path.display()))?;
        // Written as root, `install` would make root the owner.
        let owner = step.sudo.then_some(owner.as_str());
        target::write(
            ctx,
            &self.task.name,
            path,
            content.as_bytes(),
            mode,
            owner,
            step.sudo,
        )
        .with_context(|| format!("failed to write {}", path.display()))
    }

    fn show_diff(&mut self, depth: usize, old: &str, new: &str) {
        let lines = diff_lines(old.as_bytes(), new.as_bytes(), Encoding::Utf8);
        self.record_diff(depth + 1, &lines);
        for line in lines {
            self.push(depth + 1, line);
        }
    }

    /// `--undo` for a `line` step: take out the line taskpod wrote, or put
    /// back the one it replaced, and delete a file taskpod created once
    /// nothing else of taskpod's is in it.
    fn undo_line_step(
        &mut self,
        step: &LineStep,
        line: &ResolvedLine,
        depth: usize,
        label: &str,
    ) -> Result<()> {
        let ctx = &self.runner.file.ctx;
        let task = self.task.name.clone();
        let root = step.sudo;
        let write = !self.checking();
        let Some((existing, base)) =
            self.read_line_files(step, line, self.reads_as_root() && root, depth, label)?
        else {
            return Ok(());
        };
        let Some(old) = existing else {
            self.current(depth, &format!("{label} · no file, nothing to undo"));
            return Ok(());
        };
        let journal =
            journal::read_lines(ctx, &task, &line.path, root, self.reads_as_root() && root)?;
        let key = line.key();
        let found = journal.as_ref().and_then(|j| j.get(&key)).cloned();
        let record = match found {
            Some(record) => record,
            // A root-only journal in a check, or `new`: taskpod added the line.
            None if journal.is_none() || step.new => journal::LineRecord {
                key: key.clone(),
                text: line.text.clone(),
                replaced: None,
            },
            None => {
                if find_line(
                    &split_lines(&old).0,
                    &line.text,
                    line.matcher.as_deref(),
                    None,
                    line.place.as_ref(),
                )
                .is_some()
                {
                    let text = format!(
                        "{label}: no record that taskpod added the line; mark the step `new` if it did, or `keep`"
                    );
                    self.cannot(depth, &text);
                } else {
                    self.current(depth, &format!("{label} · not in the file"));
                }
                return Ok(());
            }
        };
        let mut journal = journal.unwrap_or_default();
        let had_record = journal.get(&key).is_some();
        let forget = |runner: &Runner, journal: &mut journal::Lines, all: bool| -> Result<()> {
            if !write {
                return Ok(());
            }
            let before = journal.lines.len();
            journal.lines.retain(|l| !all && l.key != key);
            if journal.lines.len() == before {
                return Ok(());
            }
            journal::write_lines(&runner.file.ctx, &task, &line.path, root, journal)
        };
        let Some((new, edited)) = undo_line(
            &old,
            &record.text,
            line.matcher.as_deref(),
            line.place.as_ref(),
            record.replaced.as_deref(),
        ) else {
            self.current(depth, &format!("{label} · already gone"));
            return forget(self.runner, &mut journal, false);
        };
        // A file taskpod created goes with the last of its lines.
        let others = journal.lines.iter().any(|l| l.key != key);
        let created =
            !others && (journal.created || (!had_record && step.new && line.base.is_some()));
        let is_base =
            |text: &str| journal.base.as_deref() == Some(text) || base.as_deref() == Some(text);
        let delete = created && (is_base(&new) || line.base.is_none() && new.trim().is_empty());
        let changed = edited || created && !delete;
        let what = if delete {
            "delete the file, taskpod created it".to_string()
        } else if let Some(was) = &record.replaced {
            format!("put back {was:?}")
        } else {
            format!("remove {:?}", record.text)
        };
        let note = if edited {
            " · the line changed since taskpod wrote it"
        } else if changed {
            " · taskpod created the file, but it has other changes now, so it stays"
        } else {
            ""
        };
        self.change(depth, &format!("{label} · {what}{note}"));
        match self.mode() {
            Mode::DryRun => return Ok(()),
            Mode::Diff => {
                if !delete {
                    self.show_diff(depth, &old, &new);
                }
                return Ok(());
            }
            Mode::Run => {}
        }
        if edited
            && !self.runner.opts.yes
            && !ui().confirm(&format!(
                "The line in {} changed since taskpod wrote it. Undo it anyway?",
                line.path.display()
            ))?
        {
            self.push(depth + 1, style("not undone").dim().to_string());
            return Ok(());
        }
        if delete {
            target::remove(ctx, &task, &line.path, root)
                .with_context(|| format!("failed to undo {}", line.path.display()))?;
        } else {
            self.write_line_file(step, &line.path, Some(&line.path), &new)
                .with_context(|| format!("failed to undo {}", line.path.display()))?;
        }
        forget(self.runner, &mut journal, delete || created)
    }

    /// Before the first write to a file, record what it was, for `--undo`.
    fn remember(
        &self,
        file: &FileStep,
        path: &Path,
        existing: Option<Vec<u8>>,
        missing: bool,
        stat: Option<(u32, String)>,
        has_owner: bool,
    ) -> Result<()> {
        let ctx = &self.runner.file.ctx;
        let task = &self.task.name;
        let root = file.sudo || has_owner;
        let previous = match (existing, stat) {
            (Some(content), Some((mode, owner))) => Previous::File {
                content,
                mode,
                owner,
                mtime: None,
            },
            (None, None) if missing => Previous::Missing,
            // Not readable as the user: as root, if the step writes as root.
            (None, Some((mode, owner))) if root => match target::read(ctx, task, path, true)? {
                Current::Content(content) => Previous::File {
                    content,
                    mode,
                    owner,
                    mtime: None,
                },
                _ => return Ok(()),
            },
            _ => return Ok(()),
        };
        journal::record(ctx, task, path, root, &previous)
    }

    // ---- Undo ----------------------------------------------------------------

    fn undo_task(&mut self, scope: &Scope) -> Result<Outcome> {
        let task = self.task;
        if task.body.keep {
            return Ok(Outcome::Skipped("keep".into()));
        }
        if let Some(reason) = self.undo_guard(&task.body.guards, scope)? {
            return Ok(Outcome::Skipped(reason));
        }
        self.undo_block(&task.body, scope, 0)?;
        Ok(if self.changed {
            Outcome::Ran
        } else {
            Outcome::UpToDate
        })
    }

    /// Guards, read for an undo: an `if` still says where the block applies,
    /// and an `unless` says when it's done, which is when there's something
    /// to undo. Returns why there's nothing to undo, if there isn't.
    fn undo_guard(&self, guards: &[Guard], scope: &Scope) -> Result<Option<String>> {
        if self.runner.opts.ignore_guards {
            return Ok(None);
        }
        for guard in guards {
            let applies = match guard {
                Guard::Ask(_) if self.checking() => true,
                // What ran after a change is undone either way.
                Guard::Changed => true,
                Guard::Unless(_) => !guard_passes(guard, scope, &self.cwd)?,
                Guard::If(_) | Guard::Ask(_) => guard_passes(guard, scope, &self.cwd)?,
            };
            if !applies {
                let reason = guard.describe();
                let reason = scope.expand(&reason).unwrap_or(reason);
                return Ok(Some(match guard {
                    Guard::Unless(_) => format!("not done ({reason})"),
                    _ => reason,
                }));
            }
        }
        Ok(None)
    }

    /// Undo a block's steps, last first. Its guards were already checked.
    fn undo_block(&mut self, block: &'r Block, scope: &Scope, depth: usize) -> Result<()> {
        self.enter(block, scope, depth, Self::undo_steps)
    }

    fn undo_steps(&mut self, block: &'r Block, scope: &Scope, depth: usize) -> Result<()> {
        let covered = block.undo_covers_commands();
        // `changed` reactions follow the files, lines and settings right
        // before them: they run again once those are undone, if that changed
        // something, before the commands and calls before them (a package
        // removal) are undone.
        let mut reactions: Vec<(&'r Step, bool)> = Vec::new();
        for step in block.steps.iter().rev() {
            let reaction = self.is_reaction(step);
            if reaction || !is_config(step) {
                for (step, changed) in std::mem::take(&mut reactions).into_iter().rev() {
                    self.react(step, scope, depth, changed)?;
                }
            }
            if reaction {
                reactions.push((step, false));
                continue;
            }
            let changed_before = std::mem::replace(&mut self.changed, false);
            let result = match step {
                Step::Run(_) if covered => Ok(()),
                Step::Run(script) => {
                    let text = format!("can't undo `{}`, it has no `undo`", first_line(script));
                    self.cannot(depth, &text);
                    Ok(())
                }
                Step::Undo(script) => self.run_step(&scope.expand(script)?, depth),
                // Notices and waits are about doing the task, not undoing it.
                Step::Notice(_) | Step::Wait(_) => Ok(()),
                // Files and nested blocks know they're undoing.
                other => self.step(other, scope, depth),
            };
            if self.changed {
                for reaction in &mut reactions {
                    reaction.1 = true;
                }
            }
            self.changed |= changed_before;
            if let Err(e) = &result
                && self.checking()
                && crate::secrets::not_checked(e)
            {
                self.unchecked(depth, &first_line(&step.describe()));
                continue;
            }
            result?;
        }
        for (step, changed) in reactions.into_iter().rev() {
            self.react(step, scope, depth, changed)?;
        }
        Ok(())
    }

    /// A `changed` block (or a call of a template that is one) that only
    /// runs commands, with no `undo` or `keep`: see [`Block::is_reaction`].
    fn is_reaction(&self, step: &Step) -> bool {
        match step {
            Step::Block(block) => block.otherwise.is_none() && block.is_reaction(),
            Step::Call { template, .. } => self.runner.file.templates[template].body.is_reaction(),
            _ => false,
        }
    }

    /// In an undo, run a reaction again, as it runs after a change: once the
    /// steps before it are undone, if that changed something.
    fn react(&mut self, step: &'r Step, scope: &Scope, depth: usize, changed: bool) -> Result<()> {
        let (block, label, arg_vars, template_vars) = match step {
            Step::Block(block) => (
                block,
                "changed".to_string(),
                Vars::default(),
                Vars::default(),
            ),
            Step::Call { template, args } => {
                let Call {
                    template,
                    label,
                    args,
                    vars,
                } = Call::new(&self.runner.file.templates[template], args, scope)?;
                (&template.body, label, args, vars)
            }
            _ => unreachable!("only blocks and calls are reactions"),
        };
        let call_scope = scope.child(&arg_vars);
        let inner = call_scope.child(&template_vars);
        if !changed && !self.runner.opts.ignore_guards {
            self.current(depth, &format!("{label} · nothing undone before it"));
            return Ok(());
        }
        let tmp = match step {
            Step::Call { .. } => crate::vars::tmp_vars(&inner, self.checking()),
            _ => Vars::default(),
        };
        let inner = inner.child(&tmp);
        let result = self.react_in(block, &inner, depth, &label);
        self.remove_tmp(&tmp, result)
    }

    /// [`Self::react`], with the template call's variables in `scope`.
    fn react_in(
        &mut self,
        block: &'r Block,
        scope: &Scope,
        depth: usize,
        label: &str,
    ) -> Result<()> {
        if self
            .runner
            .failing_guard(&block.guards, scope, &self.cwd)?
            .is_some()
        {
            self.current(depth, label);
            return Ok(());
        }
        self.change(depth, &format!("{label} · again, after the undo"));
        self.in_workdir(block, scope, |this| this.block(block, scope, depth + 1))
    }

    /// `setting "value" { get ...; set ... }`: run `set` when `get` prints
    /// something else, recording what it printed first, for `--undo`.
    fn setting(&mut self, setting: &'r SettingStep, scope: &Scope, depth: usize) -> Result<()> {
        // A value isn't in a file to pull.
        if self.runner.opts.pull {
            return Ok(());
        }
        let wanted = scope.expand(&setting.value)?;
        let get = scope.expand(&setting.get)?;
        let label = match &setting.name {
            Some(name) => format!("setting {}", scope.expand(name)?),
            None => format!("setting {}", first_line(&get)),
        };
        if self.runner.opts.undo {
            return self.undo_setting(setting, scope, depth, &label, &get, &wanted);
        }
        let current = self.runner.file.ctx.capture(&get, &self.cwd);
        if current.as_deref() == Ok(wanted.as_str()) {
            self.current(depth, &format!("{label} · {}", shown_value(&wanted)));
            return Ok(());
        }
        let from = match &current {
            Ok(value) => shown_value(value),
            Err(_) => "(get failed)".to_string(),
        };
        self.change(
            depth,
            &format!("{label} · {from} → {}", shown_value(&wanted)),
        );
        if self.checking() {
            return Ok(());
        }
        let ctx = &self.runner.file.ctx;
        if let Ok(old) = &current {
            journal::record_setting(ctx, &self.task.name, &get, old)?;
        }
        self.set_value(setting, scope, &wanted, depth)?;
        let now = self
            .runner
            .file
            .ctx
            .capture(&get, &self.cwd)
            .map_err(|e| anyhow!("after `set`: {e}"))?;
        if now != wanted {
            bail!(
                "`set` ran, but `{}` prints {} instead of {}",
                first_line(&get),
                shown_value(&now),
                shown_value(&wanted)
            );
        }
        Ok(())
    }

    /// Run a setting's `set` with `{{ value }}`.
    fn set_value(
        &mut self,
        setting: &SettingStep,
        scope: &Scope,
        value: &str,
        depth: usize,
    ) -> Result<()> {
        let mut vars = Vars::default();
        vars.literal("value", value);
        let inner = scope.child(&vars);
        self.run_step(&inner.expand(&setting.set)?, depth + 1)
    }

    /// Set a setting back to its value from before taskpod, from the journal.
    fn undo_setting(
        &mut self,
        setting: &SettingStep,
        scope: &Scope,
        depth: usize,
        label: &str,
        get: &str,
        wanted: &str,
    ) -> Result<()> {
        if setting.keep {
            self.current(depth, &format!("{label} · kept"));
            return Ok(());
        }
        let ctx = &self.runner.file.ctx;
        let task = self.task.name.clone();
        let current = ctx.capture(get, &self.cwd);
        let recorded = journal::read_setting(ctx, &task, get)?;
        let recorded_value = recorded.is_some();
        let default = setting
            .default
            .as_deref()
            .map(|d| scope.expand(d))
            .transpose()?;
        let Some(old) = recorded.or(default) else {
            if current.as_deref() == Ok(wanted) {
                let text = format!(
                    "{label}: no record of its value before taskpod set it; add `keep` to leave it"
                );
                self.cannot(depth, &text);
            } else {
                self.current(depth, &format!("{label} · not set by taskpod"));
            }
            return Ok(());
        };
        let write = !self.checking();
        if current.as_deref() == Ok(old.as_str()) {
            self.current(depth, &format!("{label} · already as it was"));
            if write && recorded_value {
                journal::remove_setting(ctx, &task, get)?;
            }
            return Ok(());
        }
        // With no record, only undo what taskpod would have set.
        if !recorded_value && current.as_deref() != Ok(wanted) {
            self.current(depth, &format!("{label} · not set by taskpod"));
            return Ok(());
        }
        let edited = current.as_deref() != Ok(wanted);
        let from = match &current {
            Ok(value) => shown_value(value),
            Err(_) => "(get failed)".to_string(),
        };
        let note = if edited {
            " · changed since taskpod set it"
        } else {
            ""
        };
        self.change(
            depth,
            &format!("{label} · {from} → {}{note}", shown_value(&old)),
        );
        if !write {
            return Ok(());
        }
        if edited
            && !self.runner.opts.yes
            && !ui().confirm(&format!(
                "`{}` changed since taskpod set it. Undo it anyway?",
                first_line(get)
            ))?
        {
            self.push(depth + 1, style("not undone").dim().to_string());
            return Ok(());
        }
        self.set_value(setting, scope, &old, depth)?;
        if recorded_value {
            journal::remove_setting(&self.runner.file.ctx, &task, get)?;
        }
        Ok(())
    }

    /// `wait ... until <check>`: check every second until it passes, with
    /// the job slot given back meanwhile. Checks only say they'd wait. It
    /// doesn't count as a change.
    fn wait(&mut self, wait: &WaitStep, scope: &Scope, depth: usize) -> Result<()> {
        if self.runner.opts.pull {
            return Ok(());
        }
        let label = scope.expand(&wait.label())?;
        if check_passes(&wait.check, scope, &self.cwd)? {
            self.current(depth, &label);
            return Ok(());
        }
        if self.checking() {
            self.current(depth, &format!("{label} · would wait"));
            return Ok(());
        }
        // Shown with live output too: nothing else says why it's stuck.
        if let Some(line) = &self.line {
            line.status(&label);
        }
        let start = std::time::Instant::now();
        let timeout = wait.timeout.map(std::time::Duration::from_secs);
        self.slot.release();
        let result = loop {
            if let Some(timeout) = timeout {
                let left = timeout.saturating_sub(start.elapsed());
                if left.is_zero() {
                    break Err(anyhow!(
                        "{label}: still waiting after {}s, gave up",
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(left.min(std::time::Duration::from_secs(1)));
            } else {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            match check_passes(&wait.check, scope, &self.cwd) {
                Ok(true) => break Ok(()),
                Ok(false) => {}
                Err(e) => break Err(e),
            }
        };
        self.slot.reacquire();
        result?;
        self.current(depth, &label);
        Ok(())
    }

    /// A `notice`: shown under the task and again at the end, in runs and
    /// checks alike. It doesn't count as a change.
    fn notice(&mut self, text: &str, depth: usize) {
        if self.runner.opts.pull {
            return;
        }
        self.record(depth, StepKind::Note, text);
        self.push(depth, ui::notice(text));
        self.report.notices.push(text.to_string());
    }

    /// Something `--undo` doesn't know how to undo.
    fn cannot(&mut self, depth: usize, text: &str) {
        self.report.cannot += 1;
        self.record(depth, StepKind::Note, text);
        self.push(depth, ui::notice(text));
    }

    /// Put a file back as it was before taskpod wrote it, from the journal, or
    /// delete it if the step says taskpod created it.
    fn undo_file(
        &mut self,
        file: &FileStep,
        depth: usize,
        path: &Path,
        content: &[u8],
        owner: Option<&str>,
    ) -> Result<()> {
        let ctx = &self.runner.file.ctx;
        let task = self.task.name.clone();
        let label = format!("file {}", path.display());
        if file.keep {
            self.current(depth, &format!("{label} · kept"));
            return Ok(());
        }
        let root = file.sudo || owner.is_some();
        let write = !self.checking();
        let entry = journal::read(ctx, &task, path, root, self.reads_as_root() && root)?;
        let recorded = entry != Entry::None;
        let previous = match entry {
            Entry::Some(previous) => Some(previous),
            // A root-only entry, in a check: it's there, but not readable.
            Entry::Unreadable => None,
            Entry::None if file.new => Some(Previous::Missing),
            Entry::None => {
                if target::stat(ctx, &task, path, self.reads_as_root() && root)?.is_none() {
                    self.current(depth, &format!("{label} · not on the system"));
                } else {
                    let text = format!(
                        "{label}: no record of what was there before taskpod wrote it; mark it `new` if taskpod created it, or `keep`"
                    );
                    self.cannot(depth, &text);
                }
                return Ok(());
            }
        };
        let current = target::read(ctx, &task, path, self.reads_as_root() && root)?;
        // Not seen (in a directory only root can enter) isn't gone.
        let missing = current == Current::Missing;
        let current = match current {
            Current::Content(bytes) => Some(bytes),
            Current::Missing | Current::Unreadable => None,
        };
        let stat = target::stat(ctx, &task, path, self.reads_as_root() && root)?;
        let forget = |runner: &Runner| -> Result<()> {
            if write && recorded {
                journal::remove(&runner.file.ctx, &task, path, root)?;
            }
            Ok(())
        };
        match &previous {
            Some(Previous::Missing) if missing && stat.is_none() => {
                self.current(depth, &format!("{label} · already gone"));
                return forget(self.runner);
            }
            Some(Previous::File {
                content: old,
                mode,
                owner: old_owner,
                ..
            }) if current.as_ref() == Some(old)
                && stat
                    .as_ref()
                    .is_some_and(|(m, o)| m == mode && (!root || o == old_owner)) =>
            {
                self.current(depth, &format!("{label} · already as it was"));
                return forget(self.runner);
            }
            _ => {}
        }
        let edited = current.as_deref().is_some_and(|c| c != content);
        let what = match &previous {
            Some(Previous::Missing) => "delete it, taskpod created it".to_string(),
            Some(Previous::File { mode, .. }) => {
                format!("put back what was there before taskpod (mode {mode:o})")
            }
            None => format!("put back what was there before taskpod{UNREAD_JOURNAL}"),
        };
        let edited_note = if edited {
            " · changed since taskpod wrote it"
        } else {
            ""
        };
        self.change(depth, &format!("{label} · {what}{edited_note}"));
        match self.mode() {
            Mode::DryRun => return Ok(()),
            Mode::Diff => {
                if let (Some(Previous::File { content: old, .. }), Some(now)) =
                    (&previous, &current)
                    && old != now
                {
                    // What was there before may hold anything: hide it for files with secrets.
                    let text = String::from_utf8_lossy(content);
                    if ui::redact(&text) != text {
                        let note = "the old content is hidden, this file holds secrets";
                        self.record(depth + 1, StepKind::Note, note);
                        self.push(depth + 1, style(note).dim().to_string());
                    } else {
                        let lines = diff_lines(now, old, file.encoding);
                        self.record_diff(depth + 1, &lines);
                        for line in lines {
                            self.push(depth + 1, line);
                        }
                    }
                }
                return Ok(());
            }
            Mode::Run => {}
        }
        if edited
            && !self.runner.opts.yes
            && !ui().confirm(&format!(
                "{} changed since taskpod wrote it. Undo it anyway?",
                path.display()
            ))?
        {
            self.push(depth + 1, style("not undone").dim().to_string());
            return Ok(());
        }
        let previous = previous
            .ok_or_else(|| anyhow!("can't read the undo journal's entry for {}", path.display()))?;
        match previous {
            Previous::Missing => target::remove(ctx, &task, path, root),
            Previous::File {
                content,
                mode,
                owner,
                mtime,
            } => target::write(
                ctx,
                &task,
                path,
                &content,
                mode,
                root.then_some(owner.as_str()),
                file.sudo,
            )
            .and_then(|()| match mtime {
                // Put back when it was last changed too, as it was before taskpod.
                Some(mtime) => target::set_mtime(ctx, &task, path, mtime, root),
                None => Ok(()),
            }),
        }
        .with_context(|| format!("failed to undo {}", path.display()))?;
        forget(self.runner)
    }

    /// `--pull` for one file step: what would change in the task files, and
    /// with `Mode::Run`, queue the changes after asking.
    #[allow(clippy::too_many_arguments)]
    fn pull_file(
        &mut self,
        file: &FileStep,
        scope: &Scope,
        depth: usize,
        path: &Path,
        content: &[u8],
        mode: u32,
        owner: Option<&str>,
    ) -> Result<()> {
        let ctx = &self.runner.file.ctx;
        let label = format!("file {}", path.display());
        let as_root = self.reads_as_root() && (file.sudo || owner.is_some());
        let system = match target::read(ctx, &self.task.name, path, as_root)? {
            Current::Content(bytes) => bytes,
            Current::Missing => {
                let text = format!("{label} · not on the system, nothing to pull");
                self.record(depth, StepKind::Ok, &text);
                self.push(depth, ui::ok(&text));
                return Ok(());
            }
            Current::Unreadable => {
                let hint = if file.sudo || owner.is_some() {
                    ""
                } else {
                    ", add `sudo` to the step to read it as root"
                };
                let text = format!("{label} · can't read it{hint}");
                self.record(depth, StepKind::Change, &text);
                self.push(depth, ui::change(&text));
                return Ok(());
            }
        };
        let mut edits = Vec::new();
        let mut parts = Vec::new();
        let mut details = Vec::new();
        let mut notes = Vec::new();
        // Options added after the path, as one edit: two at one place would conflict.
        let mut inserted = String::new();
        let encoding = match (!file.raw).then(|| pull_encoding(file.encoding, &system)) {
            Some(Some(found)) => {
                parts.push(format!(
                    "encoding {} → {}",
                    file.encoding.name(),
                    found.name()
                ));
                match &file.origin.encoding {
                    Some(span) => edits.push(replace(file, span, found.name())),
                    None => inserted.push_str(&format!(" encoding {}", found.name())),
                }
                found
            }
            _ => file.encoding,
        };
        if system != content {
            match pull_content(file, encoding, scope, source_dir(file, &ctx.dir), &system) {
                Ok(pulled) => {
                    if let Some(edit) = pulled.edit {
                        edits.push(edit);
                        parts.push("content".to_string());
                    }
                    details.extend(pulled.diff);
                    notes.extend(pulled.refused);
                }
                Err(e) => notes.push(
                    style(format!("content not pulled: {e:#}"))
                        .yellow()
                        .to_string(),
                ),
            }
        }
        if let Some((current, current_owner)) = target::stat(ctx, &self.task.name, path, as_root)? {
            if current != mode {
                match &file.origin.mode {
                    Some(span) => edits.push(replace(file, span, &format!("{current:o}"))),
                    None => inserted.push_str(&format!(" mode {current:o}")),
                }
                parts.push(format!("mode {mode:o} → {current:o}"));
            }
            if let Some(wanted) = owner
                && !owner_matches(&current_owner, wanted)
            {
                // Keep the step's form: `user`, or `user:group` if it gave a group.
                let pulled = if wanted.contains(':') {
                    current_owner.as_str()
                } else {
                    current_owner.split(':').next().unwrap_or_default()
                };
                match &file.origin.owner {
                    Some(span) if span.text.contains("{{") => notes.push(
                        style(format!(
                            "owner is {current_owner}, not {wanted}; not pulled, the step's owner uses variables"
                        ))
                        .yellow()
                        .to_string(),
                    ),
                    Some(span) => {
                        let text = match span.text.chars().next() {
                            Some(q @ ('"' | '\'')) => format!("{q}{pulled}{q}"),
                            _ => pulled.to_string(),
                        };
                        edits.push(replace(file, span, &text));
                        parts.push(format!("owner {wanted} → {pulled}"));
                    }
                    None => {}
                }
            }
        }
        if !inserted.is_empty() {
            edits.push(pull::Edit::Source {
                file: file.origin.file.clone(),
                start: file.origin.after_path,
                end: file.origin.after_path,
                original: String::new(),
                text: inserted,
            });
        }
        if edits.is_empty() {
            if notes.is_empty() {
                self.current(depth, &label);
            } else {
                self.changed = true;
                self.record(depth, StepKind::Change, &label);
                self.push(depth, ui::change(&label));
                for note in notes {
                    self.record_diff(depth + 1, std::slice::from_ref(&note));
                    self.push(depth + 1, note);
                }
            }
            return Ok(());
        }
        let label = format!("{label} · {}", parts.join(", "));
        self.change(depth, &label);
        let shown: Vec<String> = match self.mode() {
            Mode::DryRun => notes,
            Mode::Diff | Mode::Run => details.into_iter().chain(notes).collect(),
        };
        self.offer_pull(depth, &label, shown, edits)
    }

    /// Show a pulled change, and when writing, queue its edits (after asking,
    /// unless `--yes`). The change itself was already reported.
    fn offer_pull(
        &mut self,
        depth: usize,
        label: &str,
        shown: Vec<String>,
        edits: Vec<pull::Edit>,
    ) -> Result<()> {
        self.record_diff(depth + 1, &shown);
        if self.mode() != Mode::Run {
            for line in shown {
                self.push(depth + 1, line);
            }
            return Ok(());
        }
        let accept = self.runner.opts.yes || {
            // Show what would change before asking; the task's report comes later.
            let ui = ui();
            ui.line(format!(
                "{} {}",
                style(&self.task.name).bold(),
                ui::change(label)
            ));
            for line in &shown {
                ui.line(format!("    {line}"));
            }
            ui.confirm("Pull this into the task files?")?
        };
        if accept {
            self.runner
                .edits
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(edits);
            if self.runner.opts.yes {
                for line in shown {
                    self.push(depth + 1, line);
                }
            }
        } else {
            self.push(depth + 1, style("not pulled").dim().to_string());
        }
        Ok(())
    }
}

/// A template call with its arguments worked out.
struct Call<'t> {
    template: &'t Template,
    /// How the call is shown: the template's name and its arguments.
    label: String,
    /// `{{ args }}`, `{{ 1 }}`, ... and the named parameters.
    args: Vars,
    /// The template's own variables, for this call.
    vars: Vars,
}

impl<'t> Call<'t> {
    /// A call of `template` with `args` as written, filled in from `scope`.
    fn new(template: &'t Template, args: &str, scope: &Scope) -> Result<Self> {
        let args = CallArgs::new(template, args, |s| scope.expand(s))?;
        let label = format!("{} {}", template.name, args.text)
            .trim()
            .to_string();
        template
            .check_call(&args)
            .map_err(|e| anyhow!("`{label}`: {e}"))?;
        Ok(Call {
            template,
            label,
            args: template.arg_vars(&args),
            vars: template.vars.for_call(),
        })
    }
}

/// A `line` step with its variables filled in.
struct ResolvedLine {
    path: PathBuf,
    text: String,
    matcher: Option<String>,
    place: Option<LinePlace>,
    base: Option<PathBuf>,
}

impl ResolvedLine {
    fn new(step: &LineStep, scope: &Scope, ctx: &Ctx, cwd: &Path) -> Result<Self> {
        let target_path = |p: &str| -> Result<PathBuf> {
            let path = PathBuf::from(ctx.target_path(&scope.expand(p)?));
            Ok(if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            })
        };
        Ok(ResolvedLine {
            path: target_path(&step.path)?,
            text: scope.expand(&step.text)?,
            matcher: step
                .matcher
                .as_deref()
                .map(|m| scope.expand(m))
                .transpose()?,
            place: match &step.place {
                Some(LinePlace::Before(p)) => Some(LinePlace::Before(scope.expand(p)?)),
                Some(LinePlace::After(p)) => Some(LinePlace::After(scope.expand(p)?)),
                None => None,
            },
            base: step.base.as_deref().map(target_path).transpose()?,
        })
    }

    /// The step's records in the journal: its `match`, or else its text.
    fn key(&self) -> String {
        self.matcher.clone().unwrap_or_else(|| self.text.clone())
    }
}

/// A step that only puts files, lines and settings in place (or says
/// something): what a `changed` reaction follows in an undo.
fn is_config(step: &Step) -> bool {
    match step {
        Step::File(_) | Step::Line(_) | Step::Setting(_) | Step::Notice(_) | Step::Wait(_) => true,
        Step::Block(block) => {
            block.steps.iter().all(is_config)
                && block
                    .otherwise
                    .as_ref()
                    .is_none_or(|b| b.steps.iter().all(is_config))
        }
        _ => false,
    }
}

/// A setting's value in a label: quoted when empty or spaced.
fn shown_value(value: &str) -> String {
    if value.is_empty() || value.contains(char::is_whitespace) {
        format!("{value:?}")
    } else {
        value.to_string()
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

struct Pulled {
    edit: Option<pull::Edit>,
    diff: Vec<String>,
    refused: Vec<String>,
}

/// The change to a file step's content that makes it match `system`.
fn pull_content(
    file: &FileStep,
    encoding: Encoding,
    scope: &Scope,
    dir: &Path,
    system: &[u8],
) -> Result<Pulled> {
    let decode = |bytes: &[u8]| {
        encoding
            .decode(bytes)
            .ok_or_else(|| anyhow!("the system's file isn't valid {}", encoding.name()))
    };
    match &file.source {
        FileSource::Empty => bail!("a directory without `from` has nothing to pull"),
        FileSource::Url { .. } => bail!("a downloaded file has nothing to pull"),
        FileSource::Copy(source) => {
            let source = copy_source(source, scope, dir)?;
            let bytes = if file.raw {
                system.to_vec()
            } else {
                decode(system)?.into_bytes()
            };
            if std::str::from_utf8(&bytes).is_ok_and(pull::has_secret) {
                bail!("the system's file holds a secret value");
            }
            let old = std::fs::read(&source).unwrap_or_default();
            Ok(Pulled {
                diff: diff_lines(&old, &bytes, Encoding::Utf8),
                edit: Some(pull::Edit::Copy {
                    file: source,
                    bytes,
                }),
                refused: Vec::new(),
            })
        }
        FileSource::Inline(template) => {
            let mapped = pull::map_back(template, scope, &decode(system)?)?;
            if mapped.text == *template {
                return Ok(Pulled {
                    edit: None,
                    diff: Vec::new(),
                    refused: mapped.refused,
                });
            }
            let span = file
                .origin
                .content
                .as_ref()
                .ok_or_else(|| anyhow!("the content's place in the task file is unknown"))?;
            Ok(Pulled {
                diff: diff_lines(template.as_bytes(), mapped.text.as_bytes(), Encoding::Utf8),
                edit: Some(pull::Edit::Source {
                    file: file.origin.file.clone(),
                    start: span.start,
                    end: span.end,
                    original: span.text.clone(),
                    text: pull::literal(&span.text, &mapped.text),
                }),
                refused: mapped.refused,
            })
        }
    }
}

/// Replace one of a file step's option values.
fn replace(file: &FileStep, span: &crate::ast::Span, text: &str) -> pull::Edit {
    pull::Edit::Source {
        file: file.origin.file.clone(),
        start: span.start,
        end: span.end,
        original: span.text.clone(),
        text: text.to_string(),
    }
}

/// The encoding to pull for a step whose system file is `system`, if it isn't
/// the step's. Only clear cases: a byte order mark that differs, or text the
/// step's encoding can't hold.
fn pull_encoding(step: Encoding, system: &[u8]) -> Option<Encoding> {
    let found = Encoding::detect(system);
    let bom = |e: Encoding| matches!(e, Encoding::Utf8Bom | Encoding::Utf16Le | Encoding::Utf16Be);
    if found == step || system.is_empty() {
        return None;
    }
    if bom(found) || bom(step) {
        return Some(found);
    }
    match (step, found) {
        // Plain ASCII is valid in all three, so only non-ASCII text decides.
        (Encoding::Ascii | Encoding::Latin1, Encoding::Utf8) if !system.is_ascii() => Some(found),
        (Encoding::Utf8 | Encoding::Ascii, Encoding::Latin1) => Some(found),
        _ => None,
    }
}

/// What differs between the system's file and the step's content, named as
/// precisely as possible. `encoding` is `None` for raw copies.
fn content_difference(old: &[u8], new: &[u8], encoding: Option<Encoding>) -> String {
    let Some(encoding) = encoding else {
        return "content".into();
    };
    let found = Encoding::detect(old);
    let (Some(old_text), Some(new_text)) = (found.decode(old), encoding.decode(new)) else {
        return "content".into();
    };
    let lf = |t: &str| t.replace("\r\n", "\n");
    if old_text == new_text {
        if found == encoding {
            "content".into()
        } else {
            format!("encoding {} → {}", found.name(), encoding.name())
        }
    } else if lf(&old_text) == lf(&new_text) {
        if old_text.contains("\r\n") {
            "line endings CRLF → LF".into()
        } else {
            "line endings LF → CRLF".into()
        }
    } else if old_text.trim_end_matches(['\r', '\n']) == new_text.trim_end_matches(['\r', '\n']) {
        if new_text.len() > old_text.len() {
            "adds a final newline".into()
        } else {
            "removes the final newline".into()
        }
    } else {
        "content".into()
    }
}

/// Where a step's relative `from` paths start: the directory of the task
/// file it's written in, which may be an included one. Paths without
/// variables were already made absolute by the parser; this is for the others.
fn source_dir<'a>(file: &'a FileStep, main: &'a Path) -> &'a Path {
    file.origin
        .file
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(main)
}

/// Where a `from` file is.
fn copy_source(source: &str, scope: &Scope, dir: &Path) -> Result<PathBuf> {
    let source = PathBuf::from(expand_tilde(&scope.expand(source)?));
    Ok(if source.is_absolute() {
        source
    } else {
        dir.join(source)
    })
}

/// A text file on the target machine: `Some(None)` when it's missing, `None`
/// when it can't be read.
fn read_text(ctx: &Ctx, task: &str, path: &Path, as_root: bool) -> Result<Option<Option<String>>> {
    let mut current = target::read(ctx, task, path, false)?;
    if matches!(current, Current::Unreadable) && as_root {
        current = target::read(ctx, task, path, true)?;
    }
    Ok(match current {
        Current::Content(bytes) => {
            Some(Some(String::from_utf8(bytes).map_err(|_| {
                anyhow!("{} isn't UTF-8 text", path.display())
            })?))
        }
        Current::Missing => Some(None),
        Current::Unreadable => None,
    })
}

/// Whether a line is `text`, ignoring differences in spacing (columns
/// lined up with tabs or spaces) and line endings.
fn same_line(line: &str, text: &str) -> bool {
    line.split_whitespace().eq(text.split_whitespace())
}

/// What a `line` step changes in a file's text.
#[derive(Debug, Clone, PartialEq)]
enum LineChange {
    Added,
    /// A line found with `match` was replaced: what it was.
    Replaced(String),
    /// It was on the wrong side of the `before` or `after` line.
    Moved,
}

/// The lines of `text` with their endings, and the ending new lines get.
fn split_lines(text: &str) -> (Vec<String>, &'static str) {
    let ending = if text.contains("\r\n") { "\r\n" } else { "\n" };
    (
        text.split_inclusive('\n').map(String::from).collect(),
        ending,
    )
}

/// Whether a line starts with `prefix`, ignoring spacing like `same_line`:
/// leading spacing, and a run of tabs or spaces matching a single space.
fn starts_with(line: &str, prefix: &str) -> bool {
    squeeze(line).starts_with(&squeeze(prefix))
}

/// `text` without leading spacing, with each run of spacing (a trailing one
/// too) as a single space.
fn squeeze(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for c in text.trim_start().chars() {
        if c.is_whitespace() {
            space = true;
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    if space {
        out.push(' ');
    }
    out
}

/// A line's ending, `""` for a last line without one.
fn ending_of(line: &str) -> &'static str {
    if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    }
}

/// The index of the step's line: one that's `text`, or else one starting with
/// `matcher` or that's `previous` (the line taskpod last wrote for the step,
/// which a `match` for a commented-out default no longer finds), with whether
/// it's exactly the text. One on the right side of
/// `place` comes first, so the same line can be elsewhere in the file too
/// (pacman.conf's `Include` line in each repository's section). Only when
/// none is, a single one on the wrong side is the step's (to be moved); with
/// several, they belong where they are.
fn find_line(
    lines: &[String],
    text: &str,
    matcher: Option<&str>,
    previous: Option<&str>,
    place: Option<&LinePlace>,
) -> Option<(usize, bool)> {
    let exact = |l: &String| same_line(l, text);
    let matched = |l: &String| {
        matcher.is_some_and(|m| starts_with(l, m)) || previous.is_some_and(|p| same_line(l, p))
    };
    let placed = |is: &dyn Fn(&String) -> bool| {
        (0..lines.len()).find(|&i| is(&lines[i]) && !misplaced(lines, place, i))
    };
    let anywhere = |is: &dyn Fn(&String) -> bool| {
        let mut found = (0..lines.len()).filter(|&i| is(&lines[i]));
        let first = found.next()?;
        (place.is_none() || found.next().is_none()).then_some(first)
    };
    placed(&exact)
        .map(|i| (i, true))
        .or_else(|| placed(&matched).map(|i| (i, false)))
        .or_else(|| anywhere(&exact).map(|i| (i, true)))
        .or_else(|| anywhere(&matched).map(|i| (i, false)))
}

/// Whether the line at `i` is on the wrong side of its anchor. With no
/// anchor (besides itself), it's fine where it is.
fn misplaced(lines: &[String], place: Option<&LinePlace>, i: usize) -> bool {
    match line_anchor(lines, place, Some(i)) {
        Some(Ok(at)) => match place {
            Some(LinePlace::Before(_)) => i > at,
            _ => i < at,
        },
        _ => false,
    }
}

/// Where a line goes by `place`, not counting the line at `skip`: `None`
/// without a place, an error if no line starts with the prefix.
fn line_anchor(
    lines: &[String],
    place: Option<&LinePlace>,
    skip: Option<usize>,
) -> Option<Result<usize>> {
    let others = || (0..lines.len()).filter(move |&j| Some(j) != skip);
    Some(match place? {
        LinePlace::Before(prefix) => others()
            .find(|&j| starts_with(&lines[j], prefix))
            .ok_or_else(|| anyhow!("no line starts with {prefix:?} to put it before")),
        LinePlace::After(prefix) => others()
            .rfind(|&j| starts_with(&lines[j], prefix))
            .map(|j| j + 1)
            .ok_or_else(|| anyhow!("no line starts with {prefix:?} to put it after")),
    })
}

/// Insert `line` (which ends in a newline) at `at`, giving the line before
/// it a newline if it's a last line without one.
fn insert_line(lines: &mut Vec<String>, at: usize, line: String, ending: &str) {
    if at > 0 && ending_of(&lines[at - 1]).is_empty() {
        lines[at - 1].push_str(ending);
    }
    lines.insert(at, line);
}

/// `old` with the step's line where it should be, and what changed; `None`
/// if it's there already. `previous` is the line taskpod last wrote for the
/// step, from the journal.
fn ensure_line(
    old: &str,
    text: &str,
    matcher: Option<&str>,
    previous: Option<&str>,
    place: Option<&LinePlace>,
) -> Result<Option<(String, LineChange)>> {
    let (mut lines, ending) = split_lines(old);
    let mut change = None;
    match find_line(&lines, text, matcher, previous, place) {
        Some((i, exact)) => {
            if !exact {
                let was = lines[i].trim_end_matches(['\r', '\n']).to_string();
                lines[i] = format!("{text}{}", ending_of(&lines[i]));
                change = Some(LineChange::Replaced(was));
            }
            // On the wrong side of its anchor, it moves.
            if misplaced(&lines, place, i) {
                let mut line = lines.remove(i);
                if ending_of(&line).is_empty() {
                    line.push_str(ending);
                }
                let at = line_anchor(&lines, place, None).expect("a place")?;
                insert_line(&mut lines, at, line, ending);
                change.get_or_insert(LineChange::Moved);
            }
        }
        None => {
            let at = match line_anchor(&lines, place, None) {
                Some(at) => at?,
                None => lines.len(),
            };
            insert_line(&mut lines, at, format!("{text}{ending}"), ending);
            change = Some(LineChange::Added);
        }
    }
    Ok(change.map(|c| (lines.concat(), c)))
}

/// `old` with the line taskpod wrote taken out, or the line it replaced put
/// back; `None` if it isn't there. `true` when it was only found by `matcher`,
/// so it changed since taskpod wrote it.
fn undo_line(
    old: &str,
    written: &str,
    matcher: Option<&str>,
    place: Option<&LinePlace>,
    replaced: Option<&str>,
) -> Option<(String, bool)> {
    let (mut lines, _) = split_lines(old);
    let (i, exact) = find_line(&lines, written, matcher, None, place)?;
    match replaced {
        Some(was) => lines[i] = format!("{was}{}", ending_of(&lines[i])),
        None => {
            lines.remove(i);
        }
    }
    Some((lines.concat(), !exact))
}

/// The files under `dir`, as paths relative to it joined with `/`, sorted.
pub(crate) fn list_files(dir: &Path, prefix: &str, names: &mut Vec<String>) -> Result<()> {
    let mut entries = std::fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            bail!("{} isn't a UTF-8 name", entry.path().display());
        };
        let name = format!("{prefix}{name}");
        // Through symlinks, like the file steps themselves.
        if std::fs::metadata(entry.path())?.is_dir() {
            list_files(&entry.path(), &format!("{name}/"), names)?;
        } else {
            names.push(name);
        }
    }
    Ok(())
}

/// The bytes a file step writes, for content that isn't downloaded:
/// [`TaskRun::file`] downloads `url` files itself, once it knows they differ.
fn file_content(file: &FileStep, scope: &Scope, dir: &Path) -> Result<Vec<u8>> {
    match &file.source {
        FileSource::Empty => bail!("a directory without `from` has no content"),
        FileSource::Url { .. } => bail!("a downloaded file's content comes from its url"),
        FileSource::Inline(text) => file.encoding.encode(&scope.expand(text)?),
        FileSource::Copy(source) => {
            let source = copy_source(source, scope, dir)?;
            if source.is_dir() {
                bail!(
                    "{} is a directory, copy it with `directory` instead of `file`",
                    source.display()
                );
            }
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

/// Said of a root journal entry a check couldn't read.
const UNREAD_JOURNAL: &str = ", if the journal has it (--root reads it)";

pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Download `url` with curl, on this machine, and check it against
/// `sha256`. Each download is kept for the rest of the run.
fn download(url: &str, sha256: &str) -> Result<Vec<u8>> {
    type Cache = std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>;
    static CACHE: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let key = format!("{sha256} {url}");
    if let Some(bytes) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return Ok(bytes.clone());
    }
    let mut command = std::process::Command::new("curl");
    command
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--url",
            url,
        ])
        .stdin(Stdio::null());
    let output = crate::tty::clean(&mut command)
        .output()
        .context("failed to start curl, which downloads `url` files")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("downloading {url} failed: {}", stderr.trim());
    }
    let got = sha256_hex(&output.stdout);
    if got != sha256 {
        bail!(
            "{url} has sha256 {got}, not {sha256}: if it changed on purpose, check the new file \
             and put its sha256 in the task file"
        );
    }
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, output.stdout.clone());
    Ok(output.stdout)
}

/// Where a command's output goes.
enum Output {
    /// The task's live line.
    Line,
    /// Kept until the command has the terminal, while the live display stops.
    Held(Vec<String>),
    /// Straight to the terminal, which the command has.
    Direct,
}

impl Output {
    fn set(output: &std::sync::Mutex<Output>, new: Output) -> Output {
        std::mem::replace(&mut output.lock().unwrap_or_else(|e| e.into_inner()), new)
    }
}

/// Feed a stream to the task's line. For `\r` progress updates, the last one wins.
fn pump(stream: impl Read, line: &TaskLine, output: &std::sync::Mutex<Output>) {
    let mut reader = BufReader::new(stream);
    let mut buffer = Vec::new();
    while let Ok(n) = reader.read_until(b'\n', &mut buffer) {
        if n == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&buffer);
        let text = text.trim_end_matches(['\n', '\r']);
        // Locked while writing, so the output never goes to the live line
        // once that's stopped (it would wait for it, and the command for us).
        match &mut *output.lock().unwrap_or_else(|e| e.into_inner()) {
            Output::Line => line.output(text.rsplit('\r').next().unwrap_or_default()),
            Output::Held(lines) => lines.push(ui::redact(text).into_owned()),
            Output::Direct => eprintln!("{}", ui::redact(text)),
        }
        buffer.clear();
    }
}

/// A unified diff between the current and the new content.
fn diff_lines(old: &[u8], new: &[u8], encoding: Encoding) -> Vec<String> {
    // The system's file may be in another encoding than the step's.
    let old_text = encoding
        .decode(old)
        .or_else(|| Encoding::detect(old).decode(old));
    let (Some(old_text), Some(new_text)) = (old_text, encoding.decode(new)) else {
        let message = format!(
            "binary content differs ({} → {} bytes)",
            old.len(),
            new.len()
        );
        return vec![style(message).dim().to_string()];
    };
    // The old content of a file that holds secrets may hold old secrets too.
    let secret = ui::redact(&new_text) != new_text;
    let diff = similar::TextDiff::from_lines(&old_text, &new_text);
    let mut lines = Vec::new();
    for hunk in diff.unified_diff().context_radius(2).iter_hunks() {
        lines.push(style(hunk.header().to_string()).cyan().to_string());
        for change in hunk.iter_changes() {
            let text = change.value().trim_end_matches('\n');
            lines.push(match change.tag() {
                similar::ChangeTag::Delete if secret => style("-••••").red().to_string(),
                similar::ChangeTag::Delete => style(format!("-{text}")).red().to_string(),
                similar::ChangeTag::Insert => style(format!("+{text}")).green().to_string(),
                similar::ChangeTag::Equal => style(format!(" {text}")).dim().to_string(),
            });
        }
    }
    if secret && !lines.is_empty() {
        let note = "removed lines are hidden, this file holds secrets";
        lines.insert(0, style(note).dim().to_string());
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

/// Whether `current` (`user:group`) satisfies `wanted` (`user` or `user:group`).
fn owner_matches(current: &str, wanted: &str) -> bool {
    if wanted.contains(':') {
        current == wanted
    } else {
        current.split(':').next() == Some(wanted)
    }
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
    fn line_prefixes_ignore_spacing() {
        assert!(starts_with(
            "auth\t\tsufficient  pam_u2f.so cue\n",
            "auth sufficient pam_u2f.so"
        ));
        assert!(starts_with(
            "  \"DefaultAction\": \"allow\",\n",
            "\"DefaultAction\""
        ));
        assert!(starts_with("auth\tinclude\tsystem-auth\n", "auth "));
        assert!(!starts_with("authfile x\n", "auth "));
        assert!(!starts_with("account required pam_unix.so\n", "auth"));
    }

    #[test]
    fn match_replaces_a_line_spaced_with_tabs() {
        let old = "#%PAM-1.0\nauth\t\tsufficient\tpam_u2f.so cue\nauth\t\tinclude\t\tsystem-auth\n";
        let new = "auth sufficient pam_u2f.so cue authfile=/etc/u2f_mappings";
        let (text, change) = ensure_line(
            old,
            new,
            Some("auth sufficient pam_u2f.so"),
            None,
            Some(&LinePlace::Before("auth".into())),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            text,
            format!("#%PAM-1.0\n{new}\nauth\t\tinclude\t\tsystem-auth\n")
        );
        assert_eq!(
            change,
            LineChange::Replaced("auth\t\tsufficient\tpam_u2f.so cue".into())
        );
    }

    #[test]
    fn a_placed_line_can_be_elsewhere_too() {
        let include = "Include = /etc/pacman.d/mirrorlist";
        let place = LinePlace::After("[multilib]".into());
        let repos = format!("[core]\n{include}\n\n[extra]\n{include}\n\n[multilib]\n");
        let old = format!("{repos}#{include}\n");
        let (text, change) = ensure_line(&old, include, Some("#Include"), None, Some(&place))
            .unwrap()
            .unwrap();
        let new = format!("{repos}{include}\n");
        assert_eq!(text, new);
        assert_eq!(change, LineChange::Replaced(format!("#{include}")));
        // Found under [multilib] from then on, and put back from there.
        assert!(
            ensure_line(&new, include, Some("#Include"), None, Some(&place))
                .unwrap()
                .is_none()
        );
        let undone = undo_line(
            &new,
            include,
            Some("#Include"),
            Some(&place),
            Some(&format!("#{include}")),
        );
        assert_eq!(undone, Some((old, false)));
        // Missing, it's added: the ones in [core] and [extra] stay put.
        let (text, change) = ensure_line(&repos, include, None, None, Some(&place))
            .unwrap()
            .unwrap();
        assert_eq!(text, new);
        assert_eq!(change, LineChange::Added);
        assert_eq!(undo_line(&repos, include, None, Some(&place), None), None);
    }

    #[test]
    fn diffs() {
        console::set_colors_enabled(false);
        let lines = diff_lines(b"a\nb\nc\n", b"a\nB\nc\n", Encoding::Utf8);
        assert_eq!(lines, ["@@ -1,3 +1,3 @@", " a", "-b", "+B", " c"]);
    }
}
