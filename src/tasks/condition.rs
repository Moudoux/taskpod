use crate::tasks::environment::{Environment, Value};
use crate::tasks::run::Run;
use anyhow::Result;
use std::io::Write;

#[derive(Debug)]
pub enum ConditionType {
    Exists(Value),
    Shell(Run),
    Ask(Value),
    Equals(Value, Value),
}

#[derive(Debug)]
pub struct Condition {
    pub condition_type: ConditionType,
    pub invert: bool,
}

impl Condition {
    pub fn should_run_vec(conditions: &[Condition], env: &Environment) -> Result<bool> {
        for condition in conditions {
            if !condition.should_run(env)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn should_run(&self, env: &Environment) -> Result<bool> {
        let exists = |value: &Value| -> Result<bool> {
            let str = value.as_string(env)?;
            let path = std::path::Path::new(&str);
            Ok(path.exists())
        };
        let result = match &self.condition_type {
            ConditionType::Exists(value) => exists(value),
            ConditionType::Shell(run) => {
                let output = run.execute(env, None, None)?;
                let success = output.status.success();
                Ok(success)
            }
            ConditionType::Ask(value) => {
                let prompt = value.as_string(env)?;
                print!("{} (Y/n): ", prompt);
                std::io::stdout().flush()?;
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                Ok(!input.is_empty() && (&input[0..1] == "Y" || &input[0..1] == "y"))
            }
            ConditionType::Equals(first, second) => first.equals(second, env),
        }?;
        if self.invert {
            return Ok(!result);
        }
        Ok(result)
    }
}
