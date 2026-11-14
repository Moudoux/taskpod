//! Running tasks: selection, parallel scheduling and dry runs.

mod step;
pub mod sudo;

use crate::ast::{Guard, Task, TaskFile};
use crate::parser::guard_passes;
use crate::term::term;
use crate::vars::{Scope, expand_tilde};
use anyhow::{Result, bail};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, RwLock, mpsc};
use std::time::Instant;
use step::TaskRun;

pub struct Options {
    /// Show what would run without changing anything.
    pub dry_run: bool,
    /// Treat every `if`, `unless` and `ask` as passing.
    pub ignore_guards: bool,
    /// How many tasks may run at the same time.
    pub jobs: usize,
    /// Tasks to treat as skipped.
    pub skip: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The task did something.
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
    let term = term();
    let _held = term.hold();
    print!("{} {prompt} [y/N] ", term.paint("33", "?"));
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !std::io::stdin().is_terminal() {
        println!("{}", answer.trim());
    }
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
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
}

impl<'a> Runner<'a> {
    pub fn new(file: &'a TaskFile, opts: &'a Options) -> Self {
        let jobs = if opts.dry_run { 1 } else { opts.jobs };
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
        }
    }

    pub fn root_scope(&self) -> Scope<'a> {
        Scope::root(&self.file.vars, &self.file.ctx)
    }

    /// The directory a task runs in.
    pub fn workdir(&self, task: &Task) -> Result<PathBuf> {
        let base = &self.file.ctx.dir;
        let Some(workdir) = &task.workdir else {
            return Ok(base.clone());
        };
        let root = self.root_scope();
        let scope = root.child(&task.vars);
        let dir = PathBuf::from(expand_tilde(&scope.expand(workdir)?));
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
                // A dry run doesn't ask, it assumes yes.
                Guard::Ask(_) if self.opts.dry_run => true,
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
        crate::term::set_width(order.iter().map(String::as_str));
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
                        && outcomes[index[parent.as_str()]] != Some(Outcome::Ran)
                    {
                        Some(Outcome::Skipped(format!("{parent} had nothing to do")))
                    } else {
                        None
                    };
                    match outcome {
                        Some(outcome) => {
                            report(task, &outcome, None, self.opts.dry_run);
                            sender.send((i, outcome)).expect("receiver is alive");
                            running += 1;
                        }
                        // Dry runs are sequential so their output reads in order.
                        None if self.opts.dry_run => {
                            let outcome = self.run_task(task);
                            sender.send((i, outcome)).expect("receiver is alive");
                            running += 1;
                        }
                        None => {
                            let sender = sender.clone();
                            running += 1;
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
                outcomes[i] = Some(outcome);
                for &d in &dependents[i] {
                    waiting[d] -= 1;
                    if waiting[d] == 0 {
                        ready.push_back(d);
                    }
                }
            }
        });

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
        let (_shared, _exclusive) = if task.interactive {
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
        if self.opts.dry_run && outcome == Outcome::Ran {
            let root = self.root_scope();
            let desc = run.desc(&root.child(&task.vars));
            term().status("▶", "34", &task.name, &desc);
        }
        report(task, &outcome, Some(start), self.opts.dry_run);
        if self.opts.dry_run && (outcome != Outcome::UpToDate || term().verbose) {
            for line in &run.lines {
                println!("{line}");
            }
        }
        outcome
    }
}

fn report(task: &Task, outcome: &Outcome, start: Option<Instant>, dry_run: bool) {
    let term = term();
    let elapsed = start
        .map(|s| format!("({:.1?})", s.elapsed()))
        .unwrap_or_default();
    match outcome {
        Outcome::Ran if dry_run => {}
        Outcome::Ran => term.status("✓", "32", &task.name, &elapsed),
        Outcome::UpToDate => term.status("✓", "32", &task.name, "up to date"),
        Outcome::Skipped(reason) => {
            term.status("○", "2", &task.name, &format!("skipped: {reason}"))
        }
        Outcome::Failed(error) => term.status("✗", "31", &task.name, &format!("failed: {error}")),
        Outcome::Blocked(dep) => {
            term.status("✗", "31", &task.name, &format!("not run: {dep} failed"))
        }
    }
}

/// Print a summary and return whether everything succeeded.
pub fn summarize(outcomes: &[(String, Outcome)], start: Instant, dry_run: bool) -> bool {
    let count = |f: fn(&Outcome) -> bool| outcomes.iter().filter(|(_, o)| f(o)).count();
    let ran = count(|o| *o == Outcome::Ran);
    let current = count(|o| *o == Outcome::UpToDate);
    let skipped = count(|o| matches!(o, Outcome::Skipped(_)));
    let failed: Vec<&str> = outcomes
        .iter()
        .filter(|(_, o)| !o.ok())
        .map(|(n, _)| n.as_str())
        .collect();
    let term = term();
    let verb = if dry_run { "would run" } else { "ran" };
    let mut summary = format!("{ran} {verb}, {current} up to date, {skipped} skipped");
    if !failed.is_empty() {
        summary.push_str(&format!(
            ", {} failed ({})",
            failed.len(),
            failed.join(", ")
        ));
    }
    let elapsed = format!("{:.1?}", start.elapsed());
    let code = if failed.is_empty() { "1" } else { "1;31" };
    println!(
        "\n{} {}",
        term.paint(code, &summary),
        term.paint("2", &format!("in {elapsed}"))
    );
    failed.is_empty()
}

#[cfg(test)]
mod tests;
