use crate::tasks::environment::{Environment, Value};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

#[derive(Debug, Clone)]
pub enum OutputMode {
    Piped,
    Inherited,
    None,
}

impl OutputMode {
    pub fn stdio(&self) -> Stdio {
        match self {
            OutputMode::Piped => Stdio::piped(),
            OutputMode::Inherited => Stdio::inherit(),
            OutputMode::None => Stdio::null(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Run {
    pub parts: Vec<Value>,
    pub sub_run: Option<Box<Run>>,
    pub output: OutputMode,
}

impl Run {
    pub fn binary(&self, env: &Environment) -> anyhow::Result<String> {
        self.parts[0].as_string(env)
    }

    pub fn print(&self, env: &Environment, level: usize) -> anyhow::Result<()> {
        print!("{:indent$}", "", indent = level);
        for part in &self.parts {
            let value = match part {
                Value::File(name, _) => name.to_string(),
                _ => part.as_string(env)?,
            };
            print!("{value} ");
        }
        println!();
        if let Some(sub_run) = &self.sub_run {
            sub_run.print(env, level + 2)?;
        }
        Ok(())
    }

    /// Execute the command
    pub fn execute(
        &self,
        environment: &Environment,
        stdin: Option<&[u8]>,
        workdir: Option<&PathBuf>,
    ) -> anyhow::Result<Output> {
        let binary = self.binary(environment)?;
        let mut command = Command::new(binary);
        for part in &self.parts[1..] {
            let arg = part.as_string(environment)?;
            command.arg(arg);
        }
        if let Some(workdir) = workdir {
            command.current_dir(workdir);
        }
        self.setup_stream(&mut command, stdin);
        let mut process = command.spawn()?;
        if let Some(input) = stdin {
            let process_stdin = process.stdin.as_mut().expect("Failed to open stdin");
            process_stdin.write_all(input)?;
        }
        let output = process.wait_with_output()?;
        let stdout = &output.stdout;
        if let Some(sub_run) = &self.sub_run {
            let sub_output = sub_run.execute(environment, Some(stdout), workdir)?;
            return Ok(sub_output);
        }
        Ok(output)
    }

    fn setup_stream(&self, command: &mut Command, stdin: Option<&[u8]>) {
        match self.output {
            OutputMode::Piped => {
                command.stdout(Stdio::piped());
            }
            OutputMode::Inherited => {
                command.stdout(Stdio::inherit());
            }
            OutputMode::None => {
                command.stdout(Stdio::null());
                command.stderr(Stdio::null());
            }
        }

        if stdin.is_some() {
            command.stdin(Stdio::piped());
        }
    }
}
