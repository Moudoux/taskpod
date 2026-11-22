//! The parsed form of a task file.

use crate::vars::{Ctx, Vars};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

pub struct TaskFile {
    pub ctx: Ctx,
    /// The main task file.
    pub path: PathBuf,
    /// `include`, `env`, `secrets` and `age-key` statements, in the order read.
    pub declarations: Vec<Declaration>,
    pub vars: Vars,
    /// Guards that apply to the whole file.
    pub guards: Vec<Guard>,
    pub tasks: BTreeMap<String, Task>,
    pub templates: HashMap<String, Template>,
}

/// A file-level statement that reads another file, for `--sources`.
#[derive(Debug, Clone)]
pub struct Declaration {
    /// The task file it's in.
    pub file: PathBuf,
    pub line: usize,
    pub kind: DeclarationKind,
    /// The file it reads, resolved.
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DeclarationKind {
    /// `include`; `skipped` is the guard that kept it out.
    Include {
        skipped: Option<String>,
    },
    Env,
    Secrets,
    AgeKey,
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
    /// Named parameters, `template name(a, b, c="default") {`. Without
    /// parentheses, calls take any words, as `{{ args }}` and `{{ 1 }}`, `{{ 2 }}`, ...
    pub params: Option<Vec<Param>>,
    pub needs: Vec<String>,
    pub vars: Vars,
    pub body: Block,
    pub origin: String,
}

/// A template parameter: `name`, or `name="default"` for an optional one.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub default: Option<String>,
}

/// One word of a template call, as written: `{{ }}` and quotes keep spaces
/// in it, and `name=value` sets a named parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct CallWord {
    /// The parameter it names, for `name=value`.
    pub name: Option<String>,
    /// The value, without its quotes.
    pub value: String,
    /// Quoted (all of it, or the value after `name=`): it stays one word
    /// once its variables are filled in.
    pub quoted: bool,
}

/// Split a template call's arguments into words, as written (before
/// variables are filled in). `'...'` and `"..."` quote, with `\"` and
/// `\\` in double quotes, and `{{ ... }}` is part of the word it's in.
/// `name=` is only taken for a parameter's name when `params` has it, so a
/// template without named parameters gets `x=y` as a word.
pub fn split_call(
    args: &str,
    params: Option<&[Param]>,
) -> std::result::Result<Vec<CallWord>, String> {
    let mut words = Vec::new();
    let mut chars = args.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut value = String::new();
        let mut quoted = false;
        let mut name = None;
        while let Some(&(i, c)) = chars.peek() {
            if c.is_whitespace() {
                break;
            }
            chars.next();
            match c {
                '\'' | '"' => {
                    quoted = true;
                    let mut closed = false;
                    while let Some((_, d)) = chars.next() {
                        if d == c {
                            closed = true;
                            break;
                        }
                        if c == '"' && d == '\\' {
                            match chars.peek() {
                                Some(&(_, e @ ('"' | '\\'))) => {
                                    chars.next();
                                    value.push(e);
                                }
                                _ => value.push(d),
                            }
                        } else {
                            value.push(d);
                        }
                    }
                    if !closed {
                        return Err(format!("unclosed quote in `{}`", &args[start..]));
                    }
                }
                '{' if args[i..].starts_with("{{") => {
                    let Some(len) = args[i..].find("}}") else {
                        return Err(format!("missing '}}}}' in `{}`", &args[start..]));
                    };
                    value.push_str(&args[i..i + len + 2]);
                    while chars.peek().is_some_and(|&(j, _)| j < i + len + 2) {
                        chars.next();
                    }
                }
                '=' if name.is_none() && !quoted && value.chars().all(crate::vars::is_var_char) => {
                    if params.is_some_and(|ps| ps.iter().any(|p| p.name == value)) {
                        name = Some(std::mem::take(&mut value));
                    } else {
                        value.push(c);
                    }
                }
                c => value.push(c),
            }
        }
        words.push(CallWord {
            name,
            value,
            quoted,
        });
    }
    Ok(words)
}

/// A call's arguments with their variables filled in.
#[derive(Debug, Default)]
pub struct CallArgs {
    /// `{{ args }}`: all of them, as written, filled in.
    pub text: String,
    /// The words given by position: `{{ 1 }}`, `{{ 2 }}`, ...
    pub words: Vec<String>,
    /// `name=value`s.
    pub named: Vec<(String, String)>,
}

impl CallArgs {
    /// Fill in `raw`'s variables with `expand`. An unquoted word whose
    /// variables hold spaces becomes several words, as it always has.
    pub fn new(
        template: &Template,
        raw: &str,
        expand: impl Fn(&str) -> Result<String>,
    ) -> Result<Self> {
        let mut args = CallArgs {
            text: expand(raw)?,
            ..CallArgs::default()
        };
        let words = split_call(raw, template.params.as_deref())
            .map_err(|e| anyhow::anyhow!("`{} {raw}`: {e}", template.name))?;
        for word in words {
            let value = expand(&word.value)?;
            match word.name {
                Some(name) => args.named.push((name, value)),
                None if word.quoted => args.words.push(value),
                None => args
                    .words
                    .extend(value.split_whitespace().map(String::from)),
            }
        }
        Ok(args)
    }
}

impl Template {
    /// The usage line: `name a [b]`.
    fn usage(&self, params: &[Param]) -> String {
        let names: Vec<String> = params
            .iter()
            .map(|p| match &p.default {
                Some(_) => format!("[{}]", p.name),
                None => p.name.clone(),
            })
            .collect();
        format!("{} {}", self.name, names.join(" "))
    }

    /// Check that a call gives each required parameter once, if the template
    /// names its parameters. Optional ones come last.
    pub fn check_call(&self, args: &CallArgs) -> std::result::Result<(), String> {
        let Some(params) = &self.params else {
            return Ok(());
        };
        let usage = self.usage(params);
        if args.words.len() > params.len() {
            return Err(format!(
                "template '{}' takes at most {}: {usage}",
                self.name,
                plural(params.len(), "argument")
            ));
        }
        for (i, (name, _)) in args.named.iter().enumerate() {
            if args.named[..i].iter().any(|(n, _)| n == name) {
                return Err(format!("`{name}=` is given twice"));
            }
            if params.iter().position(|p| &p.name == name) < Some(args.words.len()) {
                return Err(format!("`{name}` is given by position and as `{name}=`"));
            }
        }
        let missing: Vec<&str> = params
            .iter()
            .enumerate()
            .filter(|(i, p)| {
                p.default.is_none()
                    && *i >= args.words.len()
                    && !args.named.iter().any(|(n, _)| n == &p.name)
            })
            .map(|(_, p)| p.name.as_str())
            .collect();
        if !missing.is_empty() {
            let required = params.iter().filter(|p| p.default.is_none()).count();
            return Err(format!(
                "template '{}' needs {} ({} missing): {usage}",
                self.name,
                plural(required, "argument"),
                missing.join(", ")
            ));
        }
        Ok(())
    }

    /// Variables for one call: `{{ args }}`, `{{ 1 }}`, `{{ 2 }}`, ... and
    /// each named parameter, with its default when the call leaves it out.
    pub fn arg_vars(&self, args: &CallArgs) -> Vars {
        let mut vars = Vars::for_args(&args.text, &args.words);
        for (i, param) in self.params.iter().flatten().enumerate() {
            let named = args.named.iter().find(|(n, _)| n == &param.name);
            match (args.words.get(i).or(named.map(|(_, v)| v)), &param.default) {
                (Some(word), _) => vars.literal(param.name.clone(), word.as_str()),
                (None, Some(default)) => vars.set(
                    param.name.clone(),
                    crate::vars::Var::Template(default.clone()),
                ),
                (None, None) => {}
            }
        }
        vars
    }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

#[derive(Debug, Default)]
pub struct Block {
    pub guards: Vec<Guard>,
    pub locks: Vec<String>,
    /// Ask for sudo credentials before running. Implies `terminal`.
    pub sudo: bool,
    /// `terminal`: a command that wants the terminal (a password, a key
    /// touch, a question) is given it, one at a time, instead of failing.
    pub terminal: bool,
    /// `keep`: `--undo` leaves what this block did in place.
    pub keep: bool,
    /// `workdir` in a template or an inline block: where its checks and
    /// commands run, relative to the directory around it. A task's own is
    /// [`Task::workdir`].
    pub workdir: Option<String>,
    pub steps: Vec<Step>,
    /// `} else {` after an `if` block: run instead when its guard fails.
    pub otherwise: Option<Box<Block>>,
}

impl Block {
    /// A `changed` block that only runs commands, with no `undo` and no
    /// `keep`, like a restart after a config file changed. `--undo` runs it
    /// again once the steps before it are undone, if that changed something.
    pub fn is_reaction(&self) -> bool {
        self.guards.iter().any(|g| matches!(g, Guard::Changed))
            && !self.keep
            && !self.steps.is_empty()
            && self.steps.iter().all(|s| matches!(s, Step::Run(_)))
    }

    /// The inline blocks directly in this one, `else` blocks included.
    pub fn inner_blocks(&self) -> impl Iterator<Item = &Block> {
        self.steps.iter().flat_map(|step| match step {
            Step::Block(inner) => std::iter::once(inner)
                .chain(inner.otherwise.as_deref())
                .collect(),
            _ => Vec::new(),
        })
    }

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
    /// One line in a file that's otherwise left as it is.
    Line(Box<LineStep>),
    /// A template call; the template's guards only apply to this step.
    Call {
        template: String,
        args: String,
    },
    /// A block with its own guards, e.g. `unless <check> {`.
    Block(Block),
    /// `notice "text"`: a message for me, shown under the task and again
    /// at the end of the run. Changes nothing.
    Notice(String),
    /// `setting "value" { get ...; set ... }`: a value read and written by commands.
    Setting(Box<SettingStep>),
    /// `wait "message" until <check>`: poll the check until it passes.
    Wait(Box<WaitStep>),
}

/// `wait ["message"] [timeout <seconds>] until <check>`: checked every
/// second until it passes. Changes nothing.
#[derive(Debug, Clone)]
pub struct WaitStep {
    pub message: Option<String>,
    pub timeout: Option<u64>,
    pub check: Check,
}

impl WaitStep {
    /// What it's shown as: the message, or the check.
    pub fn label(&self) -> String {
        match &self.message {
            Some(message) => format!("wait: {message}"),
            None => format!("wait until {}", self.check.text),
        }
    }
}

/// `setting "value" { get <command> set <command> }`: `get` prints the
/// current value, `set` changes it to `{{ value }}`. The journal keeps the
/// value from before taskpod first set it, so `--undo` sets that again.
#[derive(Debug, Clone)]
pub struct SettingStep {
    /// `setting name = value {`: what it's called in the output.
    pub name: Option<String>,
    pub value: String,
    pub get: String,
    pub set: String,
    /// `default "value"`: what `--undo` sets when the journal has no record,
    /// like a setting made before taskpod kept one.
    pub default: Option<String>,
    /// `--undo` leaves it as it is.
    pub keep: bool,
}

#[derive(Debug, Clone)]
pub struct FileStep {
    pub path: String,
    /// `directory`: each file in the `from` directory is written at the same
    /// place under `path`, as if it had its own step.
    pub dir: bool,
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

/// `line "path" [sudo] [new] [keep] [base "path"] [match "start"]
/// [before|after "start"] "text"`: the file has this line (compared ignoring
/// spacing). With `match`, a line starting with that is this one, and is
/// replaced. Missing, it's added: before the first other line starting with
/// `before`, after the last starting with `after` (both ignoring
/// indentation), or at the end; on the wrong side, it's moved. `--undo` puts
/// back what taskpod changed, from the journal.
#[derive(Debug, Clone)]
pub struct LineStep {
    pub path: String,
    pub text: String,
    pub place: Option<LinePlace>,
    /// A file on the target machine to start from when the file is missing,
    /// like an override of a vendor file. `--undo` deletes the file once it's
    /// the same as the base again.
    pub base: Option<String>,
    /// `match "start"`: a line starting with this (ignoring indentation)
    /// counts as this step's line, and is replaced by the text.
    pub matcher: Option<String>,
    /// `new`: with no journal record, `--undo` takes it that taskpod added
    /// the line (and created the file, if it has a base).
    pub new: bool,
    /// Read and write as root; the file keeps its owner and mode.
    pub sudo: bool,
    /// `--undo` leaves the line in.
    pub keep: bool,
}

#[derive(Debug, Clone)]
pub enum LinePlace {
    Before(String),
    After(String),
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

#[derive(Debug, Clone)]
pub enum FileSource {
    Inline(String),
    /// Copy of a file. Relative paths are relative to the file that uses them.
    Copy(String),
    /// `url "https://..." sha256 "..."`: downloaded, and checked against the hash.
    Url {
        url: String,
        sha256: String,
    },
    /// A `directory` with no `from`: only the directory itself.
    Empty,
}

/// What `if`, `unless` and `wait ... until` check: a shell command, or one
/// of the built-in checks, which need no shell.
#[derive(Debug, Clone)]
pub struct Check {
    /// As written, with `\` continuations joined: a shell command is run as this.
    pub text: String,
    pub kind: CheckKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CheckKind {
    /// Passes if the command (`Check::text`) succeeds.
    Shell,
    /// `exists "path"`: something is at the path, on the machine being configured.
    Exists(String),
    /// `which name`: an executable of that name is on the PATH.
    Which(String),
    /// `$(command) == "value"`: compares what the command prints, trimmed,
    /// whether or not it succeeds.
    Output {
        command: String,
        compare: Compare,
        value: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Compare {
    /// `==`
    Equals,
    /// `!=`
    NotEquals,
    /// `contains`
    Contains,
    /// `has-line`: one of its lines is the value, ignoring spaces at either end.
    HasLine,
}

impl Compare {
    pub fn matches(self, output: &str, value: &str) -> bool {
        let value = value.trim();
        match self {
            Compare::Equals => output.trim() == value,
            Compare::NotEquals => output.trim() != value,
            Compare::Contains => output.contains(value),
            Compare::HasLine => output.lines().any(|line| line.trim() == value),
        }
    }
}

impl Check {
    /// A check that runs `command` in the shell.
    pub fn shell(command: impl Into<String>) -> Self {
        Check {
            text: command.into(),
            kind: CheckKind::Shell,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Guard {
    /// Run only if the check passes.
    If(Check),
    /// Skip if the check passes.
    Unless(Check),
    /// Ask the user.
    Ask(String),
    /// `changed`: run only if an earlier step in the task changed something.
    /// Only in templates and inline blocks, where something can come before.
    Changed,
}

impl Guard {
    pub fn describe(&self) -> String {
        match self {
            Guard::If(check) => format!("if {}", check.text),
            Guard::Unless(check) => format!("unless {}", check.text),
            Guard::Ask(prompt) => format!("ask {prompt:?}"),
            Guard::Changed => "changed".to_string(),
        }
    }
}

impl Step {
    pub fn describe(&self) -> String {
        match self {
            Step::Run(script) => script.clone(),
            Step::Undo(script) => format!("undo {script}"),
            Step::Notice(text) => format!("notice {text}"),
            Step::Wait(wait) => wait.label(),
            Step::Setting(setting) => match &setting.name {
                Some(name) => format!("setting {name}"),
                None => format!("setting {}", setting.get),
            },
            Step::File(file) if file.dir => format!("directory {}", file.path),
            Step::File(file) => format!("file {}", file.path),
            Step::Line(line) => format!("line {}", line.path),
            Step::Call { template, args } => format!("{template} {args}").trim().to_string(),
            Step::Block(block) => block
                .guards
                .first()
                .map_or_else(|| "block".to_string(), Guard::describe),
        }
    }
}

impl TaskFile {
    pub fn new(ctx: Ctx, path: PathBuf) -> Self {
        TaskFile {
            ctx,
            path,
            declarations: Vec::new(),
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
        for inner in block.inner_blocks() {
            self.check_calls(inner, stack, origin)?;
        }
        for step in &block.steps {
            if let Step::Call { template, args } = step {
                let Some(called) = self.templates.get(template) else {
                    bail!("{origin}: unknown directive or template '{template}'");
                };
                // Arguments with variables are checked once they're worked out.
                if !args.contains("{{") {
                    let call = format!("{template} {args}");
                    let checked = CallArgs::new(called, args, |s| Ok(s.to_string()))
                        .map_err(|e| e.to_string())
                        .and_then(|a| called.check_call(&a));
                    if let Err(e) = checked {
                        bail!("{origin}: `{}`: {e}", call.trim());
                    }
                }
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
        for inner in block.inner_blocks() {
            self.template_needs(inner, needs);
        }
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
        let mut names = BTreeSet::new();
        for block in blocks {
            collect_locks(block, &mut names);
        }
        names
    }
}

fn collect_locks(block: &Block, names: &mut BTreeSet<String>) {
    names.extend(block.locks.iter().cloned());
    for inner in block.inner_blocks() {
        collect_locks(inner, names);
    }
}
