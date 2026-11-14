//! The parsed form of a task file.

use crate::vars::{Ctx, Vars};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub struct TaskFile {
    pub ctx: Ctx,
    pub vars: Vars,
    /// Guards that apply to the whole file.
    pub guards: Vec<Guard>,
    pub tasks: BTreeMap<String, Task>,
    pub templates: HashMap<String, Template>,
}

#[derive(Debug)]
pub struct Task {
    pub name: String,
    pub desc: Option<String>,
    /// Tasks that must finish first, including those needed by called templates.
    pub needs: Vec<String>,
    /// For subtasks: the task this one follows. It only runs if the parent changed something.
    pub parent: Option<String>,
    /// Only runs when selected with `--run` or needed by another task.
    pub manual: bool,
    /// Gets the terminal to itself; nothing else runs at the same time.
    pub interactive: bool,
    pub workdir: Option<String>,
    pub vars: Vars,
    pub body: Block,
    pub origin: String,
}

#[derive(Debug)]
pub struct Template {
    pub name: String,
    pub needs: Vec<String>,
    pub vars: Vars,
    pub body: Block,
    pub origin: String,
}

#[derive(Debug, Default)]
pub struct Block {
    pub guards: Vec<Guard>,
    pub locks: Vec<String>,
    /// Ask for sudo credentials before running.
    pub sudo: bool,
    pub steps: Vec<Step>,
}

#[derive(Debug)]
pub enum Step {
    /// A shell script.
    Run(String),
    File(FileStep),
    /// A template call; the template's guards only apply to this step.
    Call {
        template: String,
        args: String,
    },
}

#[derive(Debug)]
pub struct FileStep {
    pub path: String,
    pub source: FileSource,
    pub mode: Option<u32>,
    pub sudo: bool,
}

#[derive(Debug)]
pub enum FileSource {
    Inline(String),
    /// Copy of a file, relative to the task file's directory.
    Copy(String),
}

#[derive(Debug, Clone)]
pub enum Guard {
    /// Run only if the command succeeds.
    If(String),
    /// Skip if the command succeeds.
    Unless(String),
    /// Ask the user.
    Ask(String),
}

impl Guard {
    pub fn describe(&self) -> String {
        match self {
            Guard::If(cmd) => format!("if {cmd}"),
            Guard::Unless(cmd) => format!("unless {cmd}"),
            Guard::Ask(prompt) => format!("ask {prompt:?}"),
        }
    }
}

impl Step {
    pub fn describe(&self) -> String {
        match self {
            Step::Run(script) => script.clone(),
            Step::File(file) => format!("file {}", file.path),
            Step::Call { template, args } => format!("{template} {args}").trim().to_string(),
        }
    }
}

impl TaskFile {
    pub fn new(ctx: Ctx) -> Self {
        TaskFile {
            ctx,
            vars: Vars::default(),
            guards: Vec::new(),
            tasks: BTreeMap::new(),
            templates: HashMap::new(),
        }
    }

    /// Check references and add the `needs` of called templates to each task.
    pub fn link(&mut self) -> Result<()> {
        for template in self.templates.values() {
            let mut stack = vec![template.name.as_str()];
            self.check_calls(&template.body, &mut stack, &template.origin)?;
        }
        let mut hoisted = Vec::new();
        for task in self.tasks.values() {
            self.check_calls(&task.body, &mut Vec::new(), &task.origin)?;
            let mut needs = BTreeSet::new();
            self.template_needs(&task.body, &mut needs);
            needs.remove(task.name.as_str());
            hoisted.push((
                task.name.clone(),
                needs.into_iter().map(String::from).collect::<Vec<_>>(),
            ));
        }
        for (name, needs) in hoisted {
            let task = self.tasks.get_mut(&name).expect("task exists");
            for need in needs {
                if !task.needs.contains(&need) {
                    task.needs.push(need);
                }
            }
        }
        for task in self.tasks.values() {
            for need in task.needs.iter().chain(&task.parent) {
                if !self.tasks.contains_key(need) {
                    bail!(
                        "{}: task '{}' needs unknown task '{need}'",
                        task.origin,
                        task.name
                    );
                }
            }
        }
        Ok(())
    }

    fn check_calls<'a>(
        &'a self,
        block: &'a Block,
        stack: &mut Vec<&'a str>,
        origin: &str,
    ) -> Result<()> {
        for step in &block.steps {
            if let Step::Call { template, .. } = step {
                let Some(called) = self.templates.get(template) else {
                    bail!("{origin}: unknown directive or template '{template}'");
                };
                if stack.contains(&template.as_str()) {
                    bail!("{origin}: template '{template}' calls itself");
                }
                stack.push(template);
                self.check_calls(&called.body, stack, &called.origin)?;
                stack.pop();
            }
        }
        Ok(())
    }

    fn template_needs<'a>(&'a self, block: &'a Block, needs: &mut BTreeSet<&'a str>) {
        for step in &block.steps {
            if let Step::Call { template, .. } = step {
                let template = &self.templates[template];
                needs.extend(template.needs.iter().map(String::as_str));
                self.template_needs(&template.body, needs);
            }
        }
    }

    /// Names of every lock used anywhere in the file.
    pub fn lock_names(&self) -> BTreeSet<String> {
        let blocks = self
            .tasks
            .values()
            .map(|t| &t.body)
            .chain(self.templates.values().map(|t| &t.body));
        blocks.flat_map(|b| b.locks.iter().cloned()).collect()
    }
}
