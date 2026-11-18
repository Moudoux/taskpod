//! Running tasks: selection, parallel scheduling, dry runs and diffs.

pub mod graph;
pub mod journal;
pub mod json;
pub mod pull;
pub mod sources;
mod step;
pub mod sudo;
pub(crate) mod target;

use crate::ast::{Guard, Task, TaskFile};
use crate::parser::guard_passes;
use crate::ui::{self, ui};
use crate::vars::Scope;
use anyhow::{Result, bail};
use console::style;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, RwLock, mpsc};
use std::time::Instant;
use step::TaskRun;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Run,
    /// Show every task and step, and what would change.
    DryRun,
    /// Show only what would change, with file diffs.
    Diff,
}

pub struct Options {
    pub mode: Mode,
    /// Treat every `if`, `unless` and `ask` as passing.
    pub ignore_guards: bool,
    /// How many tasks may run at the same time.
    pub jobs: usize,
    /// Tasks to treat as skipped.
    pub skip: HashSet<String>,
    /// Bring system changes back into the task files instead; `mode` says
    /// whether to write them (`Run`) or only show them.
    pub pull: bool,
    /// Pull without asking about each file.
    pub yes: bool,
    /// Undo the tasks instead, in reverse order; `mode` says whether to do it
    /// (`Run`) or only show it.
    pub undo: bool,
}

impl Options {
    /// Whether tasks are only looked at, one at a time: checks and pulls.
    pub fn checking(&self) -> bool {
        self.mode != Mode::Run || self.pull
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The task did something (or would, in a dry run).
    Ran,
    /// The task ran, but every step was already up to date.
    UpToDate,
    /// A guard or `--skip` stopped the task.
    Skipped(String),
    Failed(String),
    /// A task it needs failed.
    Blocked(String),
    /// A check couldn't look at it: its checks or variables need secrets,
    /// and `--no-secrets` was given. Not a failure.
    Unchecked(String),
}

impl Outcome {
    pub fn ok(&self) -> bool {
        !matches!(self, Outcome::Failed(_) | Outcome::Blocked(_))
    }
}

/// What a step of a task does, or would do, for `--json`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StepKind {
    /// A file or block that changes (or would).
    Change,
    /// A command that runs (or would).
    Run,
    /// Already as it should be.
    Ok,
    /// Extra information, such as why a diff is incomplete.
    Note,
    /// Needs secrets, so a check with `--no-secrets` didn't look at it.
    Unchecked,
}

#[derive(Debug, Clone)]
pub struct StepRecord {
    /// How deep in template calls and inline blocks.
    pub depth: usize,
    pub kind: StepKind,
    pub label: String,
    /// Unified diff lines, for file changes in a diff.
    pub diff: Option<Vec<String>>,
}

/// The steps of one finished task.
#[derive(Debug, Clone, Default)]
pub struct TaskReport {
    pub steps: Vec<StepRecord>,
    /// Steps not checked because they need secrets.
    pub unchecked: usize,
    /// Steps `--undo` doesn't know how to undo.
    pub cannot: usize,
    /// `notice` messages the task reached, repeated at the end of the run.
    pub notices: Vec<String>,
}

/// Ask a yes/no question.
pub fn ask(prompt: &str) -> Result<bool> {
    ui().confirm(prompt)
}

/// The tasks to run for `targets` (or every non-manual task), with the tasks
/// they need, in an order where each task comes after its dependencies.
pub fn select(file: &TaskFile, targets: &[String]) -> Result<Vec<String>> {
    check_names(file, targets, "")?;
    let mut stack: Vec<&str> = if targets.is_empty() {
        file.tasks
            .values()
            .filter(|t| !t.manual)
            .map(|t| t.name.as_str())
            .collect()
    } else {
        targets.iter().map(String::as_str).collect()
    };
    let mut selected = BTreeSet::new();
    loop {
        while let Some(name) = stack.pop() {
            if selected.insert(name) {
                stack.extend(deps(&file.tasks[name]));
            }
        }
        // Subtasks follow their parent.
        stack.extend(subtasks_of(file, |name| selected.contains(name)));
        if stack.is_empty() {
            break;
        }
    }

    let mut order = Vec::new();
    let mut done = HashSet::new();
    let mut path = Vec::new();
    for name in &selected {
        visit(file, name, &mut done, &mut path, &mut order)?;
    }
    Ok(order)
}

/// The tasks to undo: `targets` and their subtasks (not what they need, which
/// other tasks may still use), or everything [`select`] picks. In the order
/// they were run; [`Runner::run`] undoes them the other way around.
pub fn select_undo(file: &TaskFile, targets: &[String]) -> Result<Vec<String>> {
    if targets.is_empty() {
        return select(file, &[]);
    }
    check_names(file, targets, "")?;
    let mut chosen: HashSet<&str> = targets.iter().map(String::as_str).collect();
    loop {
        let more: Vec<&str> = subtasks_of(file, |name| chosen.contains(name)).collect();
        if more.is_empty() {
            break;
        }
        chosen.extend(more);
    }
    // Undoing what another task still needs would break that task.
    let mut needed_by: Vec<(&str, &str)> = file
        .tasks
        .values()
        .filter(|t| !t.manual && !chosen.contains(t.name.as_str()))
        .flat_map(|t| {
            t.needs
                .iter()
                .filter(|n| chosen.contains(n.as_str()))
                .map(move |n| (n.as_str(), t.name.as_str()))
        })
        .collect();
    if !needed_by.is_empty() {
        needed_by.sort();
        let list: Vec<String> = needed_by
            .iter()
            .map(|(needed, by)| format!("{by} needs {needed}"))
            .collect();
        let mut with: Vec<&str> = targets.iter().map(String::as_str).collect();
        for (_, by) in &needed_by {
            if !with.contains(by) {
                with.push(by);
            }
        }
        bail!(
            "not undoing it, other tasks still need it: {}. To undo those too: -u -r {}",
            list.join(", "),
            with.join(" ")
        );
    }
    let all: Vec<String> = file.tasks.keys().cloned().collect();
    Ok(select(file, &all)?
        .into_iter()
        .filter(|n| chosen.contains(n.as_str()))
        .collect())
}

/// Fail if one of `names` isn't a task, listing the tasks there are.
/// `option` is the command line option it was given with, if any (`--skip`).
pub fn check_names(file: &TaskFile, names: &[String], option: &str) -> Result<()> {
    if let Some(unknown) = names.iter().find(|n| !file.tasks.contains_key(*n)) {
        let tasks: Vec<&str> = file.tasks.keys().map(String::as_str).collect();
        let option = if option.is_empty() {
            String::new()
        } else {
            format!("{option}: ")
        };
        bail!(
            "{option}no task named '{unknown}' (tasks: {})",
            tasks.join(", ")
        );
    }
    Ok(())
}

/// The non-manual subtasks of the tasks `chosen` says yes to, not chosen themselves.
fn subtasks_of(file: &TaskFile, chosen: impl Fn(&str) -> bool) -> impl Iterator<Item = &str> {
    file.tasks
        .values()
        .filter(move |t| !t.manual && !chosen(&t.name) && t.parent.as_deref().is_some_and(&chosen))
        .map(|t| t.name.as_str())
}

fn deps(task: &Task) -> impl Iterator<Item = &str> {
    task.needs.iter().chain(&task.parent).map(String::as_str)
}

fn visit<'a>(
    file: &'a TaskFile,
    name: &'a str,
    done: &mut HashSet<&'a str>,
    path: &mut Vec<&'a str>,
    order: &mut Vec<String>,
) -> Result<()> {
    if done.contains(name) {
        return Ok(());
    }
    if let Some(start) = path.iter().position(|n| *n == name) {
        let mut cycle = path[start..].to_vec();
        cycle.push(name);
        bail!("dependency cycle: {}", cycle.join(" -> "));
    }
    path.push(name);
    for dep in deps(&file.tasks[name]) {
        visit(file, dep, done, path, order)?;
    }
    path.pop();
    done.insert(name);
    order.push(name.to_string());
    Ok(())
}

/// Counting semaphore limiting how many tasks run at once.
pub struct Slots {
    free: Mutex<usize>,
    changed: Condvar,
}

impl Slots {
    fn new(count: usize) -> Self {
        Slots {
            free: Mutex::new(count.max(1)),
            changed: Condvar::new(),
        }
    }

    pub(crate) fn acquire(&self) {
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        while *free == 0 {
            free = self.changed.wait(free).unwrap_or_else(|e| e.into_inner());
        }
        *free -= 1;
    }

    pub(crate) fn release(&self) {
        *self.free.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.changed.notify_one();
    }
}

pub struct Runner<'a> {
    pub file: &'a TaskFile,
    pub opts: &'a Options,
    pub(crate) slots: Slots,
    /// Normal tasks hold this shared, interactive tasks exclusively.
    pub exclusive: RwLock<()>,
    pub locks: HashMap<String, Mutex<()>>,
    /// Changes `--pull` will make to the task files.
    pub edits: Mutex<Vec<pull::Edit>>,
    /// The steps of each task that ran, for `--json`.
    pub reports: Mutex<HashMap<String, TaskReport>>,
}

impl<'a> Runner<'a> {
    pub fn new(file: &'a TaskFile, opts: &'a Options) -> Self {
        let jobs = if opts.checking() { 1 } else { opts.jobs };
        Runner {
            file,
            opts,
            slots: Slots::new(jobs),
            exclusive: RwLock::new(()),
            locks: file
                .lock_names()
                .into_iter()
                .map(|n| (n, Mutex::new(())))
                .collect(),
            edits: Mutex::default(),
            reports: Mutex::default(),
        }
    }

    pub fn root_scope(&self) -> Scope<'a> {
        Scope::root(&self.file.vars, &self.file.ctx)
    }

    /// The directory a task runs in.
    pub fn workdir(&self, task: &Task, scope: &Scope) -> Result<PathBuf> {
        let ctx = &self.file.ctx;
        let base = ctx.default_cwd(&ctx.dir);
        let Some(workdir) = &task.workdir else {
            return Ok(base);
        };
        let dir = PathBuf::from(ctx.target_path(&scope.expand(workdir)?));
        Ok(if dir.is_absolute() {
            dir
        } else {
            base.join(dir)
        })
    }

    /// Evaluate guards. Returns the guard that failed, if any.
    pub fn failing_guard<'g>(
        &self,
        guards: &'g [Guard],
        scope: &Scope,
        cwd: &Path,
    ) -> Result<Option<&'g Guard>> {
        if self.opts.ignore_guards {
            return Ok(None);
        }
        for guard in guards {
            let passes = match guard {
                // Checks don't ask, they assume yes.
                Guard::Ask(_) if self.opts.checking() => true,
                // `TaskRun::nested` checks it first, it knows what changed.
                Guard::Changed => true,
                guard => guard_passes(guard, scope, cwd)?,
            };
            if !passes {
                return Ok(Some(guard));
            }
        }
        Ok(None)
    }

    /// Run the tasks in `order`, in parallel where dependencies allow.
    pub fn run(&self, order: &[String]) -> Vec<(String, Outcome)> {
        let index: HashMap<&str, usize> = order
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), i))
            .collect();
        let tasks: Vec<&Task> = order.iter().map(|n| &self.file.tasks[n]).collect();
        // What each task waits for: what it needs, or for an undo, what needs it.
        let mut waits_for: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); tasks.len()];
        for (i, task) in tasks.iter().enumerate() {
            for dep in deps(task).filter_map(|d| index.get(d).copied()) {
                if self.opts.undo {
                    waits_for[dep].insert(i);
                } else {
                    waits_for[i].insert(dep);
                }
            }
        }
        let mut dependents = vec![Vec::new(); tasks.len()];
        let waiting: Vec<usize> = waits_for.iter().map(BTreeSet::len).collect();
        let mut waiting = waiting;
        for (i, waits) in waits_for.iter().enumerate() {
            for &dep in waits {
                dependents[dep].push(i);
            }
        }
        let mut outcomes: Vec<Option<Outcome>> = vec![None; tasks.len()];
        let mut ready: VecDeque<usize> = (0..tasks.len()).filter(|&i| waiting[i] == 0).collect();
        if self.opts.undo {
            // Last run, first undone.
            ready = ready.into_iter().rev().collect();
        }
        let (sender, receiver) = mpsc::channel::<(usize, Outcome)>();

        let what = match self.opts.mode {
            _ if self.opts.pull => "pulled",
            Mode::Run if self.opts.undo => "undone",
            Mode::Run => "tasks",
            Mode::DryRun | Mode::Diff => "checked",
        };
        let names: Vec<&str> = order.iter().map(String::as_str).collect();
        ui().begin(&names, what);

        std::thread::scope(|s| {
            let mut running = 0;
            loop {
                while let Some(i) = ready.pop_front() {
                    let task = tasks[i];
                    let outcome = if self.opts.skip.contains(&task.name) {
                        Some(Outcome::Skipped("--skip".into()))
                    } else if let Some(&dep) = waits_for[i]
                        .iter()
                        .find(|&&d| !outcomes[d].as_ref().is_some_and(Outcome::ok))
                    {
                        Some(Outcome::Blocked(tasks[dep].name.clone()))
                    } else if let Some(parent) = &task.parent
                        && !self.opts.pull
                        && !self.opts.undo
                        && outcomes[index[parent.as_str()]] != Some(Outcome::Ran)
                    {
                        let why = match outcomes[index[parent.as_str()]] {
                            Some(Outcome::Unchecked(_)) => "wasn't checked",
                            _ => "had nothing to do",
                        };
                        Some(Outcome::Skipped(format!("{parent} {why}")))
                    } else {
                        None
                    };
                    running += 1;
                    match outcome {
                        Some(outcome) => {
                            self.report(task, &outcome, "", &[]);
                            sender.send((i, outcome)).expect("receiver is alive");
                        }
                        // Checks run one task at a time so their output reads in order.
                        None if self.opts.checking() => {
                            sender
                                .send((i, self.run_task(task)))
                                .expect("receiver is alive");
                        }
                        None => {
                            let sender = sender.clone();
                            s.spawn(move || {
                                let outcome = self.run_task(task);
                                sender.send((i, outcome)).expect("receiver is alive");
                            });
                        }
                    }
                }
                if running == 0 {
                    break;
                }
                let (i, outcome) = receiver.recv().expect("a task is running");
                running -= 1;
                ui().advance();
                outcomes[i] = Some(outcome);
                for &d in &dependents[i] {
                    waiting[d] -= 1;
                    if waiting[d] == 0 {
                        ready.push_back(d);
                    }
                }
            }
        });
        ui().end();

        order
            .iter()
            .cloned()
            .zip(
                outcomes
                    .into_iter()
                    .map(|o| o.expect("every task finished")),
            )
            .collect()
    }

    fn run_task(&self, task: &Task) -> Outcome {
        // Interactive tasks wait until nothing else runs, and block others meanwhile.
        let (_shared, _exclusive) = if task.interactive && !self.opts.checking() {
            (
                None,
                Some(self.exclusive.write().unwrap_or_else(|e| e.into_inner())),
            )
        } else {
            (
                Some(self.exclusive.read().unwrap_or_else(|e| e.into_inner())),
                None,
            )
        };
        let start = Instant::now();
        let mut run = TaskRun::new(self, task);
        let outcome = run.run().unwrap_or_else(|e| {
            if self.opts.checking() && crate::secrets::not_checked(&e) {
                Outcome::Unchecked(format!("{e:#}"))
            } else {
                Outcome::Failed(format!("{e:#}"))
            }
        });
        let elapsed = format!("· {:.1?}", start.elapsed());
        let root = self.root_scope();
        let desc = run.desc(&root.child(&task.vars));
        let detail = match (desc.is_empty(), self.opts.checking()) {
            (true, true) => String::new(),
            (true, false) => elapsed,
            (false, true) => format!("· {desc}"),
            (false, false) => format!("· {desc} {elapsed}"),
        };
        let lines = run.finish();
        self.reports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(task.name.clone(), run.take_report());
        self.report(task, &outcome, &detail, &lines);
        outcome
    }

    fn report(&self, task: &Task, outcome: &Outcome, detail: &str, lines: &[String]) {
        let ui = ui();
        let name = task.name.as_str();
        let diff = self.opts.mode == Mode::Diff;
        let line = match outcome {
            Outcome::Ran if self.opts.pull => Some(ui::differs(name, detail)),
            Outcome::Ran if self.opts.undo && self.opts.checking() => {
                Some(ui::would_change(name, detail))
            }
            Outcome::Ran if self.opts.undo => Some(ui::undone(name, detail)),
            // Shown in a diff too when something can't be undone.
            Outcome::UpToDate if self.opts.undo && (!diff || ui.verbose || !lines.is_empty()) => {
                Some(ui::nothing_to_undo(name))
            }
            Outcome::Ran if self.opts.checking() => Some(ui::would_change(name, detail)),
            Outcome::Ran => Some(ui::done(name, detail)),
            Outcome::UpToDate if !diff || ui.verbose => Some(ui::up_to_date(name)),
            Outcome::Skipped(reason) if !diff || ui.verbose => Some(ui::skipped(name, reason)),
            Outcome::Unchecked(reason) => Some(ui::unchecked(name, reason)),
            Outcome::Failed(error) => Some(ui::failed(name, error)),
            Outcome::Blocked(dep) if self.opts.undo => Some(ui::failed(
                name,
                &format!("not undone, undoing {dep} failed"),
            )),
            Outcome::Blocked(dep) => Some(ui::failed(name, &format!("not run, {dep} failed"))),
            _ => None,
        };
        if let Some(line) = line {
            ui.line(line);
            // Notes about what can't be undone show even when nothing else would.
            let quiet = *outcome == Outcome::UpToDate && !ui.verbose && !self.opts.undo;
            for line in lines.iter().filter(|_| !quiet) {
                ui.line(format!("    {line}"));
            }
        }
    }
}

/// Print a summary and return whether everything succeeded. `notices` are
/// the `notice` messages tasks reached, as (task, text), listed above it.
pub fn summarize(
    outcomes: &[(String, Outcome)],
    start: Instant,
    opts: &Options,
    file: &Path,
    cannot: usize,
    notices: &[(String, String)],
) -> bool {
    let count = |f: fn(&Outcome) -> bool| outcomes.iter().filter(|(_, o)| f(o)).count();
    let ran = count(|o| *o == Outcome::Ran);
    let current = count(|o| *o == Outcome::UpToDate);
    let skipped = count(|o| matches!(o, Outcome::Skipped(_)));
    let unchecked = count(|o| matches!(o, Outcome::Unchecked(_)));
    let not_checked = if unchecked > 0 {
        format!(", {unchecked} not checked (secrets)")
    } else {
        String::new()
    };
    let failed: Vec<&str> = outcomes
        .iter()
        .filter(|(_, o)| !o.ok())
        .map(|(n, _)| n.as_str())
        .collect();
    let elapsed = style(format!("in {:.1?}", start.elapsed())).dim();
    let ui = ui();
    ui.line("");
    for (task, text) in notices {
        ui.line(ui::notice(&format!("{task}: {text}")));
    }
    if !notices.is_empty() {
        ui.line("");
    }
    if opts.undo {
        let verb = if opts.checking() {
            "would be undone"
        } else {
            "undone"
        };
        let mut parts = vec![format!("{ran} {verb}")];
        if cannot > 0 {
            let steps = if cannot == 1 { "step" } else { "steps" };
            parts.push(format!("{cannot} {steps} can't be undone"));
        }
        if unchecked > 0 {
            parts.push(format!("{unchecked} not checked (secrets)"));
        }
        if !failed.is_empty() {
            parts.push(format!("{} failed ({})", failed.len(), failed.join(", ")));
        }
        let summary = style(parts.join(", ")).bold();
        let summary = if failed.is_empty() {
            summary
        } else {
            summary.red()
        };
        ui.line(format!("{summary} {elapsed}"));
        return failed.is_empty();
    }
    if opts.pull && ran == 0 && failed.is_empty() {
        ui.line(format!(
            "{} {} {elapsed}",
            style("✓").green().bold(),
            style(format!(
                "Nothing to pull, {} matches the system",
                file.display()
            ))
            .bold()
        ));
        return true;
    }
    if opts.checking() && ran == 0 && failed.is_empty() {
        ui.line(format!(
            "{} {} {elapsed}",
            style("✓").green().bold(),
            style(format!(
                "Nothing would change, the system matches {}{not_checked}",
                file.display()
            ))
            .bold()
        ));
        return true;
    }
    let verb = if opts.pull {
        "differ"
    } else if opts.checking() {
        "would change"
    } else {
        "ran"
    };
    let mut parts = vec![format!("{ran} {verb}")];
    if opts.mode == Mode::Diff || opts.pull {
        let changed: Vec<&str> = outcomes
            .iter()
            .filter(|(_, o)| *o == Outcome::Ran)
            .map(|(n, _)| n.as_str())
            .collect();
        parts = vec![format!("{ran} {verb}: {}", changed.join(", "))];
    } else {
        parts.push(format!("{current} up to date"));
        parts.push(format!("{skipped} skipped"));
    }
    if unchecked > 0 {
        parts.push(format!("{unchecked} not checked (secrets)"));
    }
    if !failed.is_empty() {
        parts.push(format!("{} failed ({})", failed.len(), failed.join(", ")));
    }
    let summary = parts.join(", ");
    let summary = if failed.is_empty() {
        style(summary).bold()
    } else {
        style(summary).red().bold()
    };
    ui.line(format!("{summary} {elapsed}"));
    failed.is_empty()
}

#[cfg(test)]
mod tests;
