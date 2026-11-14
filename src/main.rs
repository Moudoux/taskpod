use anyhow::Result;
use clap::Parser;
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;
use taskpod::exec::{Options, Runner, select, summarize};
use taskpod::term::term;
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

    /// Show what would run without changing anything. Runs `if` and `unless` checks
    #[arg(short, long, visible_short_alias = 'n')]
    pub dry_run: bool,

    /// Show what a task would do, same as --dry-run --run TASK
    #[arg(short, long, value_name = "TASK")]
    pub inspect: Option<String>,

    /// List all tasks
    #[arg(short, long)]
    pub list: bool,

    /// Tasks to skip
    #[arg(short, long, num_args = 1..)]
    pub skip: Vec<String>,

    /// Run tasks even when their `if`, `unless` or `ask` says not to
    #[arg(long)]
    pub ignore_conditions: bool,

    /// How many tasks to run at once. With 1, commands use the terminal directly
    #[arg(short, long, default_value_t = default_jobs())]
    pub jobs: usize,

    /// Show every command and up to date step
    #[arg(short, long)]
    pub verbose: bool,
}

fn default_jobs() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

fn main() -> ExitCode {
    let args = Args::parse();
    taskpod::term::init(args.verbose);
    match run(args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("{} {error:#}", term().paint("1;31", "error:"));
            ExitCode::FAILURE
        }
    }
}

fn run(mut args: Args) -> Result<bool> {
    let start = Instant::now();
    let file = taskpod::parser::parse_file(&args.file)?;

    if args.list {
        let scope = Scope::root(&file.vars, &file.ctx);
        let term = term();
        for task in file.tasks.values() {
            let desc = task
                .desc
                .as_deref()
                .map(|d| scope.expand(d).unwrap_or_else(|_| d.to_string()));
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
                format!(" [{}]", tags.join(", "))
            };
            println!(
                "{}{}  {}",
                term.paint("1", &task.name),
                tags,
                term.paint("2", desc.as_deref().unwrap_or(""))
            );
        }
        return Ok(true);
    }

    if let Some(task) = args.inspect.take() {
        args.dry_run = true;
        args.run = vec![task];
    }

    let opts = Options {
        dry_run: args.dry_run,
        ignore_guards: args.ignore_conditions,
        jobs: args.jobs.max(1),
        skip: args.skip.into_iter().collect::<HashSet<_>>(),
    };
    let order = select(&file, &args.run)?;
    let runner = Runner::new(&file, &opts);

    let scope = runner.root_scope();
    if let Some(guard) = runner.failing_guard(&file.guards, &scope, &file.ctx.dir)? {
        term().info(&format!("Nothing to do: {}", guard.describe()));
        return Ok(true);
    }

    let outcomes = runner.run(&order);
    Ok(summarize(&outcomes, start, opts.dry_run))
}
