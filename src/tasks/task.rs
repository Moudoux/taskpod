use crate::tasks::condition::Condition;
use crate::tasks::environment::{Environment, Value};
use crate::tasks::run::Run;
use anyhow::Result;
use log::{debug, info};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq)]
pub enum RunState {
    /// The command has not started yet
    NotStarted,
    /// The command is running
    Running,
    /// The command has been skipped
    Skipped,
    /// The command has finished
    Finished,
    Failed(ExitStatus, String),
}

#[derive(Debug)]
pub struct Task {
    pub name: String,
    pub depends: Vec<String>,
    pub description: Option<Value>,
    pub environment: Environment,
    pub conditions: Vec<Condition>,
    pub run: Vec<Run>,
    pub workdir: Option<Value>,
    pub state: Mutex<RunState>,
    pub parent_id: Option<String>,
    pub subtask: Vec<Box<Task>>,
}

impl Task {
    pub fn new(name: String, env: Environment) -> Self {
        Task {
            name,
            description: None,
            depends: Vec::new(),
            environment: env,
            run: Vec::new(),
            workdir: None,
            conditions: Vec::new(),
            subtask: Vec::new(),
            parent_id: None,
            state: Mutex::new(RunState::NotStarted),
        }
    }

    pub fn print(&self, ignore_conditions: bool) -> Result<()> {
        let mut state = self.state.lock().expect("Failed to lock run state");
        if *state != RunState::NotStarted {
            return Ok(());
        }
        *state = RunState::Finished;
        if !ignore_conditions && !Condition::should_run_vec(&self.conditions, &self.environment)? {
            info!("Skipping task {}", self.name);
            *state = RunState::Skipped;
            return Ok(());
        }
        self.print_name()?;
        for run in &self.run {
            run.print(&self.environment, 2)?;
        }
        for subtask in &self.subtask {
            subtask.print(ignore_conditions)?;
        }
        Ok(())
    }

    pub fn print_name(&self) -> Result<()> {
        match &self.description {
            Some(desc) => {
                let parsed_desc = desc.as_string(&self.environment)?;
                info!("{}", parsed_desc);
            }
            None => info!("{}", self.name),
        }
        Ok(())
    }

    pub fn run(&self, ignore_conditions: bool) -> Result<()> {
        let mut state = self.state.lock().expect("Failed to lock run state");
        if *state != RunState::NotStarted {
            return Ok(());
        }
        *state = RunState::Running;
        // Check conditions
        if !ignore_conditions && !Condition::should_run_vec(&self.conditions, &self.environment)? {
            debug!("Skipping task {}", self.name);
            *state = RunState::Skipped;
            return Ok(());
        }
        self.print_name()?;
        let run_state = self.do_run()?;
        if run_state == RunState::Finished {
            for subtask in &self.subtask {
                subtask.run(ignore_conditions)?;
            }
        }
        *state = run_state;
        std::io::stdout().flush()?;
        Ok(())
    }

    fn do_run(&self) -> Result<RunState> {
        let mut workdir = Vec::new();
        let default_dir = match self.workdir {
            Some(ref dir) => {
                let parsed_dir = dir.as_string(&self.environment)?;
                PathBuf::from(parsed_dir)
            }
            None => PathBuf::from("."),
        };
        workdir.push(default_dir);
        for run in &self.run {
            let binary = run.binary(&self.environment)?;
            let last_workdir = workdir.last().expect("Workdir stack is empty");
            if binary == "cd" {
                let path = run.parts[1].as_string(&self.environment)?;
                if path == ".." {
                    workdir.pop();
                } else {
                    workdir.push(last_workdir.join(path));
                }
                continue;
            }
            let output = run.execute(&self.environment, None, Some(last_workdir))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Ok(RunState::Failed(output.status, stderr.to_string()));
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            let trimmed = stdout.trim();
            if !trimmed.is_empty() {
                println!("{}", trimmed);
            }
        }
        Ok(RunState::Finished)
    }
}
