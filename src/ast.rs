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
    /// `keep`: `--undo` leaves what this block did in place.
    pub keep: bool,
    pub steps: Vec<Step>,
}

impl Block {
    /// Whether `--undo` knows what to do about this block's commands: it has
    /// an `undo`, or says `keep`.
    pub fn undo_covers_commands(&self) -> bool {
        self.keep || self.steps.iter().any(|s| matches!(s, Step::Undo(_)))
    }
}

#[derive(Debug)]
pub enum Step {
    /// A shell script.
    Run(String),
    /// A shell script that undoes the block, only run by `--undo`.
    Undo(String),
    File(Box<FileStep>),
    /// A template call; the template's guards only apply to this step.
    Call {
        template: String,
        args: String,
    },
    /// A block with its own guards, e.g. `unless <check> {`.
    Block(Block),
}

#[derive(Debug)]
pub struct FileStep {
    pub path: String,
    pub source: FileSource,
    /// Permission bits, default 644 (755 for content starting with `#!`).
    pub mode: Option<u32>,
    /// `user` or `user:group`; only checked and set when given.
    pub owner: Option<String>,
    pub encoding: Encoding,
    /// Copy the source's bytes unchanged (a `from` file with no encoding).
    pub raw: bool,
    pub sudo: bool,
    /// `new`: the file didn't exist before taskpod, so `--undo` deletes it
    /// when there's no record of what was there.
    pub new: bool,
    /// `keep`: `--undo` leaves the file as it is.
    pub keep: bool,
    /// Where the step is written, so `--pull` can rewrite it.
    pub origin: FileOrigin,
}

/// Positions in the task file of a file step's parts.
#[derive(Debug, Clone, Default)]
pub struct FileOrigin {
    pub file: std::path::PathBuf,
    /// Right after the path string, where a `mode` can be added.
    pub after_path: usize,
    /// The mode's value, if given.
    pub mode: Option<Span>,
    /// The owner's value, if given.
    pub owner: Option<Span>,
    /// The encoding's name, if given.
    pub encoding: Option<Span>,
    /// The inline content's string literal, quotes included.
    pub content: Option<Span>,
}

/// A piece of source text and where it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// How a file's text is written. Copied files (`from`) are written byte for
/// byte unless an encoding is given, in which case they're read as UTF-8.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Encoding {
    #[default]
    Utf8,
    Utf8Bom,
    /// UTF-16 little endian, with a byte order mark.
    Utf16Le,
    /// UTF-16 big endian, with a byte order mark.
    Utf16Be,
    Latin1,
    Ascii,
}

impl Encoding {
    pub const NAMES: &[&str] = &[
        "utf-8",
        "utf-8-bom",
        "utf-16le",
        "utf-16be",
        "latin1",
        "ascii",
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "utf-8" | "utf8" => Encoding::Utf8,
            "utf-8-bom" | "utf8-bom" => Encoding::Utf8Bom,
            "utf-16le" | "utf16le" | "utf-16" => Encoding::Utf16Le,
            "utf-16be" | "utf16be" => Encoding::Utf16Be,
            "latin1" | "latin-1" | "iso-8859-1" => Encoding::Latin1,
            "ascii" | "us-ascii" => Encoding::Ascii,
            _ => return None,
        })
    }

    /// The name used in task files.
    pub fn name(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf-8",
            Encoding::Utf8Bom => "utf-8-bom",
            Encoding::Utf16Le => "utf-16le",
            Encoding::Utf16Be => "utf-16be",
            Encoding::Latin1 => "latin1",
            Encoding::Ascii => "ascii",
        }
    }

    /// The encoding of a file on the system: from its byte order mark, else
    /// UTF-8 if it's valid UTF-8, else Latin-1, which decodes anything.
    pub fn detect(bytes: &[u8]) -> Self {
        if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
            Encoding::Utf8Bom
        } else if bytes.starts_with(&[0xFF, 0xFE]) {
            Encoding::Utf16Le
        } else if bytes.starts_with(&[0xFE, 0xFF]) {
            Encoding::Utf16Be
        } else if std::str::from_utf8(bytes).is_ok() {
            Encoding::Utf8
        } else {
            Encoding::Latin1
        }
    }

    pub fn encode(self, text: &str) -> Result<Vec<u8>> {
        Ok(match self {
            Encoding::Utf8 => text.as_bytes().to_vec(),
            Encoding::Utf8Bom => [&[0xEF, 0xBB, 0xBF][..], text.as_bytes()].concat(),
            Encoding::Utf16Le => [0xFF, 0xFE]
                .into_iter()
                .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
                .collect(),
            Encoding::Utf16Be => [0xFE, 0xFF]
                .into_iter()
                .chain(text.encode_utf16().flat_map(u16::to_be_bytes))
                .collect(),
            Encoding::Latin1 | Encoding::Ascii => {
                let max = if self == Encoding::Latin1 { 0xFF } else { 0x7F };
                let name = if self == Encoding::Latin1 {
                    "latin1"
                } else {
                    "ascii"
                };
                text.chars()
                    .map(|c| match u8::try_from(c as u32) {
                        Ok(b) if u32::from(b) <= max => Ok(b),
                        _ => Err(anyhow::anyhow!("{c:?} can't be written as {name}")),
                    })
                    .collect::<Result<_>>()?
            }
        })
    }

    /// Decode for showing a diff.
    pub fn decode(self, bytes: &[u8]) -> Option<String> {
        match self {
            Encoding::Utf8 | Encoding::Utf8Bom | Encoding::Ascii => {
                let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
                String::from_utf8(bytes.to_vec()).ok()
            }
            Encoding::Latin1 => Some(bytes.iter().map(|&b| b as char).collect()),
            Encoding::Utf16Le | Encoding::Utf16Be => {
                let bytes = bytes.get(2..)?;
                if bytes.len() % 2 != 0 {
                    return None;
                }
                let units = bytes.chunks(2).map(|c| {
                    if self == Encoding::Utf16Le {
                        u16::from_le_bytes([c[0], c[1]])
                    } else {
                        u16::from_be_bytes([c[0], c[1]])
                    }
                });
                char::decode_utf16(units)
                    .collect::<std::result::Result<String, _>>()
                    .ok()
            }
        }
    }
}

#[derive(Debug)]
pub enum FileSource {
    Inline(String),
    /// Copy of a file. Relative paths are relative to the file that uses them.
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
            Step::Undo(script) => format!("undo {script}"),
            Step::File(file) => format!("file {}", file.path),
            Step::Call { template, args } => format!("{template} {args}").trim().to_string(),
            Step::Block(block) => block
                .guards
                .first()
                .map_or_else(|| "block".to_string(), Guard::describe),
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
            if let Step::Block(inner) = step {
                self.check_calls(inner, stack, origin)?;
            }
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
            if let Step::Block(inner) = step {
                self.template_needs(inner, needs);
            }
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
        let mut names = BTreeSet::new();
        for block in blocks {
            collect_locks(block, &mut names);
        }
        names
    }
}

fn collect_locks(block: &Block, names: &mut BTreeSet<String>) {
    names.extend(block.locks.iter().cloned());
    for step in &block.steps {
        if let Step::Block(inner) = step {
            collect_locks(inner, names);
        }
    }
}
