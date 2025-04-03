use anyhow::Result;
use clap::Parser;
use taskpod::taskfile::TaskFile;

#[derive(Parser)]
#[command(author, about, long_about = None)]
struct Args {
    /// Task file to execute
    #[arg(default_value = "main.task")]
    file: String,

    /// Enables verbose output
    #[arg(short, long, default_value_t = false)]
    verbose: bool,

    /// Dry run, do not execute any tasks
    #[arg(short, long, default_value_t = false)]
    dry_run: bool,

    /// Ignore task conditions
    #[arg(long, default_value_t = false)]
    ignore_conditions: bool,

    /// Inspect a task
    #[arg(short, long)]
    inspect: Option<String>,

    /// List all tasks
    #[arg(short, long, default_value_t = false)]
    list: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut clog = colog::default_builder();
    if args.verbose {
        clog.filter(None, log::LevelFilter::Debug);
    }
    clog.init();

    let dry_run = args.dry_run || args.inspect.is_some();
    let file = TaskFile::from_file(&args.file, dry_run)?;

    if args.list {
        for task in file.tasks.values() {
            task.print_name()?;
        }
        return Ok(());
    }

    let ignore_conditions = args.ignore_conditions;
    if let Some(task_name) = args.inspect {
        if let Some(task) = file.tasks.get(&task_name) {
            file.execute_task(task, true, ignore_conditions)?;
        } else {
            return Err(anyhow::anyhow!("Task {} not found", task_name));
        }
        return Ok(());
    }

    if !args.dry_run {
        file.execute(ignore_conditions)?;
    } else {
        file.print(ignore_conditions)?;
    }

    Ok(())
}
