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

    /// Show the tasks and what they need, without running anything: as a tree,
    /// or as Graphviz (`dot`) or Mermaid. With --run, only those tasks and their needs
    #[arg(
        short,
        long,
        value_name = "FORMAT",
        num_args = 0..=1,
        default_missing_value = "tree",
        conflicts_with_all = ["list", "check", "json"]
    )]
    pub graph: Option<GraphFormat>,

    /// Show every file the task file reads, as a tree by the file that names it: includes,
    /// env, secrets and age-key files, each task's `file`/`directory` sources and the
    /// secrets it uses, manual tasks included. Changes nothing. With --run, only those tasks
    /// and their needs
    #[arg(long, conflicts_with_all = ["list", "check", "json", "graph"])]
    pub sources: bool,

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

    /// With --pull, pull every change without asking; with --undo, undo files changed
    /// since taskpod wrote them without asking
    #[arg(short, long)]
    pub yes: bool,

    /// Undo the tasks (with --run, those and their subtasks): runs each block's `undo`,
    /// puts files back as they were before taskpod first wrote them, and deletes `new`
    /// files. With -d or -D, only shows what it would do
    #[arg(short, long, conflicts_with = "pull")]
    pub undo: bool,

    /// Configure another machine over SSH (user@host or a Host from ~/.ssh/config).
    /// taskpod doesn't need to be installed there
    #[arg(short = 'H', long, value_name = "HOST")]
    pub host: Option<String>,

    /// An age identity, encrypted (to a security key, say), that the secrets files are also
    /// encrypted to: decrypted once, so several files need one unlock
    #[arg(long, value_name = "FILE")]
    pub age_key: Option<PathBuf>,

    /// Decrypt --age-key with only this age plugin (`age -j NAME`), no identity file
    #[arg(long, value_name = "NAME", requires = "age_key")]
    pub age_plugin: Option<String>,

    /// Print the result of a dry run or diff as JSON on stdout, for editors, instead of
    /// the usual output. Never asks anything
    #[arg(long, conflicts_with_all = ["check", "list"])]
    pub json: bool,

    /// Don't decrypt secrets: steps and tasks that need them are reported as not checked.
    /// Only for dry runs and diffs
    #[arg(long)]
    pub no_secrets: bool,

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

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum GraphFormat {
    Tree,
    Dot,
    Mermaid,
}

fn default_jobs() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

fn main() -> ExitCode {
    let args = Args::parse();
    // First, before any thread starts. Steps that ask get the terminal, except for --json.
    let _terminal = taskpod::tty::init(!args.json);
    taskpod::ui::init(args.verbose, args.plain, args.json);
    let (json, file, mode, pull) = (args.json, args.file.clone(), mode(&args), args.pull);
    match run(args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) if json => {
            let error = format!("{error:#}");
            let document = taskpod::exec::json::error(&file, mode, pull, &error);
            println!("{document}");
            ExitCode::FAILURE
        }
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

fn mode(args: &Args) -> Mode {
    match (args.dry_run || args.inspect.is_some(), args.diff) {
        (_, true) => Mode::Diff,
        (true, _) => Mode::DryRun,
        _ => Mode::Run,
    }
}

fn run(mut args: Args) -> Result<bool> {
    let start = Instant::now();
    let mode = mode(&args);
    if args.yes && !args.pull && !args.undo {
        anyhow::bail!("--yes only works with --pull or --undo");
    }
    if mode == Mode::Run && !args.check && !args.list && args.graph.is_none() && !args.sources {
        if args.json {
            anyhow::bail!("--json only works with --dry-run or --diff");
        }
        if args.no_secrets {
            anyhow::bail!("--no-secrets only works with --dry-run or --diff");
        }
    }
    // Checking and listing only read the file, they don't need the machine.
    let remote = match &args.host {
        Some(host) if !args.check && !args.list && args.graph.is_none() && !args.sources => {
            Some(std::sync::Arc::new(taskpod::remote::Remote::connect(host)?))
        }
        _ => None,
    };
    let mut file = taskpod::parser::parse_file_on(&args.file, remote)?;
    for path in &args.secrets {
        file.ctx.secrets.add(std::path::absolute(path)?);
    }
    if let Some(key) = &args.age_key {
        file.ctx
            .secrets
            .set_age_key(std::path::absolute(key)?, args.age_plugin.clone());
    }
    if args.no_secrets {
        file.ctx.secrets.disable();
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

    if let Some(format) = args.graph {
        use taskpod::exec::graph::{Format, render};
        let format = match format {
            GraphFormat::Tree => Format::Tree,
            GraphFormat::Dot => Format::Dot,
            GraphFormat::Mermaid => Format::Mermaid,
        };
        print!("{}", render(&file, &select(&file, &args.run)?, format));
        return Ok(true);
    }

    if args.sources {
        use taskpod::exec::sources::{Extra, render};
        let secrets = args
            .secrets
            .iter()
            .map(std::path::absolute)
            .collect::<std::io::Result<Vec<_>>>()?;
        let age_key = args.age_key.as_ref().map(std::path::absolute).transpose()?;
        let extra = Extra {
            secrets: &secrets,
            age_key: age_key.as_deref(),
        };
        // Without --run, every task: manual ones read files too.
        let order = if args.run.is_empty() {
            file.tasks.keys().cloned().collect()
        } else {
            select(&file, &args.run)?
        };
        print!("{}", render(&file, &order, &extra));
        return Ok(true);
    }

    // `mode` already counts --inspect as a dry run.
    if let Some(task) = args.inspect.take() {
        args.run = vec![task];
    }

    // A misspelt --skip would otherwise run the very task meant to be skipped.
    taskpod::exec::check_names(&file, &args.skip, "--skip")?;
    let opts = Options {
        mode,
        ignore_guards: args.ignore_conditions,
        jobs: args.jobs.max(1),
        skip: args.skip.into_iter().collect::<HashSet<_>>(),
        pull: args.pull,
        yes: args.yes,
        undo: args.undo,
    };
    let order = if args.undo {
        taskpod::exec::select_undo(&file, &args.run)?
    } else {
        select(&file, &args.run)?
    };
    let runner = Runner::new(&file, &opts);

    let scope = runner.root_scope();
    let cwd = file.ctx.default_cwd(&file.ctx.dir);
    if let Some(guard) = runner.failing_guard(&file.guards, &scope, &cwd)? {
        let reason = guard.describe();
        if args.json {
            let document = taskpod::exec::json::document(&args.file, &runner, &[], Some(&reason));
            println!("{document}");
        }
        ui().line(format!("Nothing to do: {reason}"));
        return Ok(true);
    }

    let outcomes = runner.run(&order);
    if args.json {
        println!(
            "{}",
            taskpod::exec::json::document(&args.file, &runner, &outcomes, None)
        );
        return Ok(outcomes.iter().all(|(_, o)| o.ok()));
    }
    let (cannot, notices) = {
        let reports = runner.reports.lock().unwrap_or_else(|e| e.into_inner());
        let cannot = reports.values().map(|r| r.cannot).sum();
        // In task order: a task's notices come after those of the tasks it needs.
        let notices: Vec<(String, String)> = outcomes
            .iter()
            .filter_map(|(name, _)| reports.get(name).map(|r| (name, &r.notices)))
            .flat_map(|(name, texts)| texts.iter().map(|t| (name.clone(), t.clone())))
            .collect();
        (cannot, notices)
    };
    let ok = summarize(&outcomes, start, &opts, &args.file, cannot, &notices);
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
