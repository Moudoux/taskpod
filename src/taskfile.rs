use crate::parsing::parser::Parser;
use crate::tasks::condition::Condition;
use crate::tasks::environment::{Environment, Value};
use crate::tasks::task::Task;
use log::{debug, info};
use std::collections::HashMap;
use std::sync::Arc;

pub struct TaskFile {
    pub path: String,
    pub conditions: Vec<Condition>,
    pub environment: Arc<Environment>,
    pub tasks: HashMap<String, Task>,
}

impl TaskFile {
    pub fn from_file(path: &str) -> anyhow::Result<Self> {
        let start_time = std::time::Instant::now();
        let mut parser = Parser::from_file(path)?;
        let environment = Arc::new(Environment::new());
        Self::populate_default_env(environment.clone());
        parser.run(environment.clone())?;
        let mut tasks = parser.tasks;
        let subtasks = parser.subtasks;
        for (_, subtask) in subtasks {
            let parent_id = subtask.parent_id.as_ref().ok_or(anyhow::anyhow!(
                "Subtask {} has no parent task",
                subtask.name
            ))?;
            let parent_task = tasks.get_mut(parent_id).ok_or(anyhow::anyhow!(
                "Subtask {} references non-existent parent task {parent_id}",
                subtask.name
            ))?;
            parent_task.subtask.push(Box::new(subtask));
        }
        let duration = start_time.elapsed();
        debug!("Parsed {} ({} tasks) in {:?}", path, tasks.len(), duration);
        Ok(TaskFile {
            path: path.to_string(),
            conditions: parser.conditions,
            environment,
            tasks,
        })
    }

    fn populate_default_env(env: Arc<Environment>) {
        env.add("pwd".into(), Value::Env("PWD".into()));
        env.add("home".into(), Value::Env("HOME".into()));
        env.add("user".into(), Value::Env("USER".into()));
    }

    pub fn print(&self, ignore_conditions: bool) -> anyhow::Result<()> {
        for task in self.tasks.values() {
            self.execute_task(task, true, ignore_conditions)?;
        }
        Ok(())
    }

    pub fn execute(&self, ignore_conditions: bool) -> anyhow::Result<()> {
        let start = std::time::Instant::now();
        info!("Executing {}", self.path);
        if !ignore_conditions && !Condition::should_run_vec(&self.conditions, &self.environment)? {
            info!("Skipping tasks file execution");
            return Ok(());
        }
        for task in self.tasks.values() {
            self.execute_task(task, false, ignore_conditions)?;
        }
        let duration = start.elapsed();
        info!("Done in {:?}", duration);
        Ok(())
    }

    pub fn execute_task(
        &self,
        task: &Task,
        dry_run: bool,
        ignore_conditions: bool,
    ) -> anyhow::Result<()> {
        for depend in &task.depends {
            let subtask = self
                .tasks
                .get(depend)
                .ok_or_else(|| anyhow::anyhow!("Task {} not found in tasks file", depend))?;

            if dry_run {
                subtask.print(ignore_conditions)?;
            } else {
                subtask.run(ignore_conditions)?;
            }
        }

        if dry_run {
            task.print(ignore_conditions)?;
        } else {
            task.run(ignore_conditions)?;
        }
        Ok(())
    }
}
