use anyhow::Result;
use clap::Parser;
use taskpod::taskfile::TaskFile;

#[derive(Parser)]
#[command(author, about, long_about = None)]
struct Args {
    /// Task file to execute
    file: String,

    /// Enables verbose output
    #[arg(short, long, default_value_t = false)]
    verbose: bool,

    /// Dry run, do not execute any tasks
    #[arg(short, long, default_value_t = false)]
    dry_run: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut clog = colog::default_builder();
    if args.verbose {
        clog.filter(None, log::LevelFilter::Debug);
    }
    clog.init();

    let file = TaskFile::from_file(&args.file)?;
    if !args.dry_run {
        file.execute()?;
    } else {
        file.print()?;
    }

    Ok(())
}
