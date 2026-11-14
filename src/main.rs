use anyhow::Result;
use clap::Parser;
use console::style;
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;
use taskpod::exec::{Mode, Options, Runner, select, summarize};
use taskpod::ui::ui;
use taskpod::vars::Scope;

#[derive(Parser)]
#[command(author, about, version, long_about = None)]
pub struct Args {
    /// Task file to run
    #[arg(default_value = "main.task")]
    pub file: PathBuf,

    /// Run only these tasks (and the tasks they need)
    #[arg(short, long, num_args = 1..)]
    pub run: Vec<String>,

    /// Show every task and step, and what would change. Runs `if` and `unless` checks, changes nothing
    #[arg(short, long, visible_short_alias = 'n')]
    pub dry_run: bool,

    /// Compare with the system: show only what would change, with file diffs
    #[arg(short = 'D', long, conflicts_with = "dry_run")]
    pub diff: bool,

    /// Show what a task would do, same as --dry-run --run TASK
    #[arg(short, long, value_name = "TASK")]
    pub inspect: Option<String>,

    /// List all tasks
    #[arg(short, long)]
    pub list: bool,

    /// Check the file for errors without running anything
    #[arg(long)]
    pub check: bool,

    /// sops-encrypted file of secrets, used as {{ secrets.name }}. Can be given more than once
    #[arg(long, value_name = "FILE")]
    pub secrets: Vec<PathBuf>,

    /// Bring changes made on the system back into the task files: file content
    /// and modes. Asks about each file; with -d or -D, only shows what would change
    #[arg(short, long)]
    pub pull: bool,

    /// With --pull, pull every change without asking
    #[arg(short, long, requires = "pull")]
    pub yes: bool,

    /// Configure another machine over SSH (user@host or a Host from ~/.ssh/config).
    /// taskpod doesn't need to be installed there
    #[arg(short = 'H', long, value_name = "HOST")]
    pub host: Option<String>,

    /// Tasks to skip
    #[arg(short, long, num_args = 1..)]
    pub skip: Vec<String>,

    /// Run tasks even when their `if`, `unless` or `ask` says not to
    #[arg(long)]
    pub ignore_conditions: bool,

    /// How many tasks to run at once
    #[arg(short, long, default_value_t = default_jobs())]
    pub jobs: usize,

    /// Stream every command and its output
    #[arg(short, long)]
    pub verbose: bool,

    /// Plain output without spinners, also used when not on a terminal
    #[arg(long)]
    pub plain: bool,
}

fn default_jobs() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

fn main() -> ExitCode {
    let args = Args::parse();
    taskpod::ui::init(args.verbose, args.plain);
    match run(args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            ui().end();
            let error = format!("{error:#}");
            eprintln!(
                "{} {}",
                style("error:").red().bold(),
                taskpod::ui::redact(&error)
            );
            ExitCode::FAILURE
        }
    }
}

fn run(mut args: Args) -> Result<bool> {
    let start = Instant::now();
    // Checking and listing only read the file, they don't need the machine.
    let remote = match &args.host {
        Some(host) if !args.check && !args.list => {
            Some(std::sync::Arc::new(taskpod::remote::Remote::connect(host)?))
        }
        _ => None,
    };
    let mut file = taskpod::parser::parse_file_on(&args.file, remote)?;
    for path in &args.secrets {
        file.ctx.secrets.add(std::path::absolute(path)?);
    }

    if args.check {
        println!("{} {} is valid", style("✓").green(), args.file.display());
        return Ok(true);
    }

    if args.list {
        let scope = Scope::root(&file.vars, &file.ctx);
        let width = file
            .tasks
            .keys()
            .map(|n| n.chars().count())
            .max()
            .unwrap_or(0);
        for task in file.tasks.values() {
            let desc = task
                .desc
                .as_deref()
                .map(|d| scope.expand(d).unwrap_or_else(|_| d.to_string()))
                .unwrap_or_default();
            let mut tags = Vec::new();
            if task.manual {
                tags.push("manual".to_string());
            }
            if let Some(parent) = &task.parent {
                tags.push(format!("after {parent}"));
            }
            let tags = if tags.is_empty() {
                String::new()
            } else {
                format!("[{}] ", tags.join(", "))
            };
            let name = format!("{:width$}", task.name);
            let line = format!(
                "{}  {}{}",
                style(name).bold(),
                style(tags).cyan(),
                style(desc).dim()
            );
            println!("{}", taskpod::ui::redact(&line));
        }
        return Ok(true);
    }

    if let Some(task) = args.inspect.take() {
        args.dry_run = true;
        args.run = vec![task];
    }

    let mode = match (args.dry_run, args.diff) {
        (_, true) => Mode::Diff,
        (true, _) => Mode::DryRun,
        _ => Mode::Run,
    };
    let opts = Options {
        mode,
        ignore_guards: args.ignore_conditions,
        jobs: args.jobs.max(1),
        skip: args.skip.into_iter().collect::<HashSet<_>>(),
        pull: args.pull,
        yes: args.yes,
    };
    let order = select(&file, &args.run)?;
    let runner = Runner::new(&file, &opts);

    let scope = runner.root_scope();
    let cwd = file.ctx.default_cwd(&file.ctx.dir);
    if let Some(guard) = runner.failing_guard(&file.guards, &scope, &cwd)? {
        ui().line(format!("Nothing to do: {}", guard.describe()));
        return Ok(true);
    }

    let outcomes = runner.run(&order);
    let ok = summarize(&outcomes, start, &opts, &args.file);
    let edits = std::mem::take(&mut *runner.edits.lock().unwrap_or_else(|e| e.into_inner()));
    if edits.is_empty() {
        return Ok(ok);
    }
    let applied = taskpod::exec::pull::apply(&edits);
    for file in &applied.written {
        ui().line(format!(
            "{} pulled into {}",
            style("✓").green(),
            file.display()
        ));
    }
    for problem in &applied.problems {
        ui().line(format!("{} {problem}", style("✗").red()));
    }
    Ok(ok && applied.problems.is_empty())
}
