//! Running tasks: selection, parallel scheduling, dry runs and diffs.

pub mod pull;
mod step;
pub mod sudo;
mod target;

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
}

impl Outcome {
    fn ok(&self) -> bool {
        !matches!(self, Outcome::Failed(_) | Outcome::Blocked(_))
    }
}

/// Ask a yes/no question.
pub fn ask(prompt: &str) -> Result<bool> {
    ui().confirm(prompt)
}

/// The tasks to run for `targets` (or every non-manual task), with the tasks
/// they need, in an order where each task comes after its dependencies.
pub fn select(file: &TaskFile, targets: &[String]) -> Result<Vec<String>> {
    for target in targets {
        if !file.tasks.contains_key(target) {
            let names: Vec<&str> = file.tasks.keys().map(String::as_str).collect();
            bail!("no task named '{target}' (tasks: {})", names.join(", "));
        }
    }
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
        stack.extend(
            file.tasks
                .values()
                .filter(|t| !t.manual && !selected.contains(t.name.as_str()))
                .filter(|t| t.parent.as_deref().is_some_and(|p| selected.contains(p)))
                .map(|t| t.name.as_str()),
        );
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
        }
    }

    pub fn root_scope(&self) -> Scope<'a> {
        Scope::root(&self.file.vars, &self.file.ctx)
    }

    /// The directory a task runs in.
    pub fn workdir(&self, task: &Task) -> Result<PathBuf> {
        let ctx = &self.file.ctx;
        let base = ctx.default_cwd(&ctx.dir);
        let Some(workdir) = &task.workdir else {
            return Ok(base);
        };
        let root = self.root_scope();
        let scope = root.child(&task.vars);
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
        let mut dependents = vec![Vec::new(); tasks.len()];
        let mut waiting: Vec<usize> = vec![0; tasks.len()];
        for (i, task) in tasks.iter().enumerate() {
            let task_deps: BTreeSet<usize> = deps(task).map(|d| index[d]).collect();
            waiting[i] = task_deps.len();
            for dep in task_deps {
                dependents[dep].push(i);
            }
        }
        let mut outcomes: Vec<Option<Outcome>> = vec![None; tasks.len()];
        let mut ready: VecDeque<usize> = (0..tasks.len()).filter(|&i| waiting[i] == 0).collect();
        let (sender, receiver) = mpsc::channel::<(usize, Outcome)>();

        let what = match self.opts.mode {
            _ if self.opts.pull => "pulled",
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
                    } else if let Some(dep) =
                        deps(task).find(|d| !outcomes[index[d]].as_ref().is_some_and(Outcome::ok))
                    {
                        Some(Outcome::Blocked(dep.to_string()))
                    } else if let Some(parent) = &task.parent
                        && !self.opts.pull
                        && outcomes[index[parent.as_str()]] != Some(Outcome::Ran)
                    {
                        Some(Outcome::Skipped(format!("{parent} had nothing to do")))
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
        let outcome = run
            .run()
            .unwrap_or_else(|e| Outcome::Failed(format!("{e:#}")));
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
        self.report(task, &outcome, &detail, &lines);
        outcome
    }

    fn report(&self, task: &Task, outcome: &Outcome, detail: &str, lines: &[String]) {
        let ui = ui();
        let name = task.name.as_str();
        let diff = self.opts.mode == Mode::Diff;
        let line = match outcome {
            Outcome::Ran if self.opts.pull => Some(ui::differs(name, detail)),
            Outcome::Ran if self.opts.checking() => Some(ui::would_change(name, detail)),
            Outcome::Ran => Some(ui::done(name, detail)),
            Outcome::UpToDate if !diff || ui.verbose => Some(ui::up_to_date(name)),
            Outcome::Skipped(reason) if !diff || ui.verbose => Some(ui::skipped(name, reason)),
            Outcome::Failed(error) => Some(ui::failed(name, error)),
            Outcome::Blocked(dep) => Some(ui::failed(name, &format!("not run, {dep} failed"))),
            _ => None,
        };
        if let Some(line) = line {
            ui.line(line);
            let quiet = *outcome == Outcome::UpToDate && !ui.verbose;
            for line in lines.iter().filter(|_| !quiet) {
                ui.line(format!("    {line}"));
            }
        }
    }
}

/// Print a summary and return whether everything succeeded.
pub fn summarize(
    outcomes: &[(String, Outcome)],
    start: Instant,
    opts: &Options,
    file: &Path,
) -> bool {
    let count = |f: fn(&Outcome) -> bool| outcomes.iter().filter(|(_, o)| f(o)).count();
    let ran = count(|o| *o == Outcome::Ran);
    let current = count(|o| *o == Outcome::UpToDate);
    let skipped = count(|o| matches!(o, Outcome::Skipped(_)));
    let failed: Vec<&str> = outcomes
        .iter()
        .filter(|(_, o)| !o.ok())
        .map(|(n, _)| n.as_str())
        .collect();
    let elapsed = style(format!("in {:.1?}", start.elapsed())).dim();
    let ui = ui();
    ui.line("");
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
                "Nothing would change, the system matches {}",
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
