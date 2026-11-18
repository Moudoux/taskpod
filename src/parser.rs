//! Parser for `.task` files.
//!
//! The format is line based: each statement ends at the end of its line, and
//! a trailing `\` continues it on the next one. Shell commands (`run`, `if`,
//! `unless`) are passed to the shell as written.

use crate::ast::*;
use crate::vars::{Ctx, Prompt, Scope, Var, Vars, expand_tilde};
use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};

const MAX_INCLUDE_DEPTH: usize = 16;

/// Parse a task file and everything it includes.
pub fn parse_file(path: &Path) -> Result<TaskFile> {
    parse_file_on(path, None)
}

/// Parse a task file for configuring `remote`, which `include ... if` checks run on.
pub fn parse_file_on(
    path: &Path,
    remote: Option<std::sync::Arc<crate::remote::Remote>>,
) -> Result<TaskFile> {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut ctx = Ctx::new(dir);
    ctx.remote = remote;
    let mut file = TaskFile::new(ctx, path.clone());
    parse_into(&mut file, &path, 0)?;
    file.link()?;
    Ok(file)
}

/// Parse task file source, used by tests and tools.
pub fn parse_str(source: &str, dir: &Path) -> Result<TaskFile> {
    let path = dir.join("<string>");
    let mut file = TaskFile::new(Ctx::new(dir.to_path_buf()), path.clone());
    Parser::new(source, path).parse(&mut file, 0)?;
    file.link()?;
    Ok(file)
}

fn parse_into(file: &mut TaskFile, path: &Path, depth: usize) -> Result<()> {
    if depth > MAX_INCLUDE_DEPTH {
        bail!("includes nest too deeply at {}", path.display());
    }
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    Parser::new(&source, path.to_path_buf()).parse(file, depth)
}

struct Parser<'s> {
    src: &'s str,
    pos: usize,
    path: PathBuf,
}

/// The parts of a task or template body.
#[derive(Default)]
struct Body {
    desc: Option<String>,
    needs: Vec<String>,
    manual: bool,
    interactive: bool,
    workdir: Option<String>,
    vars: Vars,
    block: Block,
    /// An inline block closed with `} else {`.
    else_follows: bool,
}

#[derive(PartialEq)]
enum BodyKind {
    Task,
    Template,
    /// An inline `if <check> {` block.
    Block,
}

impl<'s> Parser<'s> {
    fn new(src: &'s str, path: PathBuf) -> Self {
        Parser { src, pos: 0, path }
    }

    fn dir(&self) -> PathBuf {
        self.path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }

    // ---- Statements -------------------------------------------------------

    fn parse(&mut self, file: &mut TaskFile, depth: usize) -> Result<()> {
        loop {
            self.skip_trivia();
            if self.eof() {
                return Ok(());
            }
            let start = self.pos;
            let keyword = self.word("a statement")?;
            match keyword.as_str() {
                "let" => {
                    let (name, var) = self.let_value()?;
                    file.vars.set(name, var);
                }
                "env" => {
                    let path = self.string()?;
                    self.end_of_line()?;
                    let path = self.resolve(&path);
                    self.declare(file, start, DeclarationKind::Env, &path);
                    load_env_file(&path, file).map_err(|e| self.error_at(start, e))?;
                }
                "secrets" => {
                    let path = self.string()?;
                    self.end_of_line()?;
                    let path = self.resolve(&path);
                    self.declare(file, start, DeclarationKind::Secrets, &path);
                    file.ctx.secrets.add(path);
                }
                "age-key" => {
                    let path = self.string()?;
                    let plugin = self.optional_plugin()?;
                    self.end_of_line()?;
                    let path = self.resolve(&path);
                    self.declare(file, start, DeclarationKind::AgeKey, &path);
                    file.ctx.secrets.set_age_key(path, plugin);
                }
                "include" => {
                    let path = self.string()?;
                    let guard = self.optional_guard()?;
                    self.end_of_line()?;
                    let path = self.resolve(&path);
                    if let Some(guard) = guard {
                        let scope = Scope::root(&file.vars, &file.ctx);
                        let cwd = file.ctx.default_cwd(&self.dir());
                        if !guard_passes(&guard, &scope, &cwd)? {
                            let skipped = Some(guard.describe());
                            self.declare(file, start, DeclarationKind::Include { skipped }, &path);
                            continue;
                        }
                    }
                    let kind = DeclarationKind::Include { skipped: None };
                    self.declare(file, start, kind, &path);
                    parse_into(file, &path, depth + 1)
                        .map_err(|e| self.error_at(start, format!("in include: {e:#}")))?;
                }
                "shell" => {
                    let shell = self.string()?;
                    self.end_of_line()?;
                    let parts: Vec<String> = shell.split_whitespace().map(String::from).collect();
                    if parts.is_empty() {
                        return Err(self.error_at(start, "shell must not be empty"));
                    }
                    file.ctx.shell = parts;
                }
                "if" | "unless" | "ask" => {
                    let guard = self.guard(&keyword)?;
                    file.guards.push(guard);
                }
                "task" | "subtask" | "template" => {
                    self.definition(file, &keyword, Vec::new(), start)?;
                }
                // `sudo interactive task name {`: flags before a definition.
                flag if is_header_flag(flag) => {
                    let mut flags = vec![(keyword.clone(), start)];
                    let kind = loop {
                        let at = self.pos;
                        let word = self.word("'task', 'subtask', 'template' or another flag")?;
                        match word.as_str() {
                            "task" | "subtask" | "template" => break word,
                            flag if is_header_flag(flag) => {
                                if flags.iter().any(|(f, _)| f == flag) {
                                    return Err(
                                        self.error_at(at, format!("'{flag}' is given twice"))
                                    );
                                }
                                flags.push((word, at));
                            }
                            other => {
                                return Err(self.error_at(
                                    at,
                                    format!("expected 'task', 'subtask' or 'template' after '{keyword}', found '{other}'"),
                                ));
                            }
                        }
                    };
                    self.definition(file, &kind, flags, start)?;
                }
                other => {
                    return Err(self.error_at(start, format!("unknown statement '{other}'")));
                }
            }
        }
    }

    fn body(&mut self, kind: BodyKind, open: usize) -> Result<Body> {
        let mut body = Body::default();
        loop {
            self.skip_trivia();
            if self.eof() {
                return Err(self.error_at(open, "missing closing '}'"));
            }
            if self.peek() == Some('}') {
                self.bump();
                if kind == BodyKind::Block {
                    self.skip_inline_ws();
                    if self.eat_keyword("else") {
                        self.open_brace()?;
                        body.else_follows = true;
                        return Ok(body);
                    }
                }
                self.end_of_line()?;
                return Ok(body);
            }
            let start = self.pos;
            let keyword = self.word("a directive")?;
            let task_only = |p: &Self| {
                if kind != BodyKind::Task {
                    Err(p.error_at(start, format!("'{keyword}' can only be used in tasks")))
                } else {
                    Ok(())
                }
            };
            let not_in_block = |p: &Self| {
                if kind == BodyKind::Block {
                    let message =
                        format!("'{keyword}' can't be used in an inline block, put it in the task");
                    Err(p.error_at(start, message))
                } else {
                    Ok(())
                }
            };
            match keyword.as_str() {
                "desc" => {
                    task_only(self)?;
                    body.desc = Some(self.string()?);
                    self.end_of_line()?;
                }
                "needs" => {
                    not_in_block(self)?;
                    body.needs.extend(self.words("a task name")?);
                }
                "lock" => body.block.locks.extend(self.words("a lock name")?),
                "if" | "unless" | "ask" => {
                    let (guard, opens_block) = self.guard_line(&keyword)?;
                    if opens_block {
                        let inner = self.body(BodyKind::Block, start)?;
                        let mut block = inner.block;
                        if inner.else_follows {
                            if !matches!(guard, Guard::If(_)) {
                                let message = match &guard {
                                    Guard::Unless(_) => {
                                        "`else` only follows an `if` block: \
                                        write `unless X { A } else { B }` as `if X { B } else { A }`"
                                    }
                                    _ => "`else` only follows an `if` block",
                                };
                                return Err(self.error_at(start, message));
                            }
                            let open = self.pos;
                            let other = self.body(BodyKind::Block, open)?;
                            if other.else_follows {
                                return Err(self.error_at(open, "an `else` block has no `else`"));
                            }
                            block.otherwise = Some(Box::new(other.block));
                        }
                        block.guards.insert(0, guard);
                        body.block.steps.push(Step::Block(block));
                    } else {
                        body.block.guards.push(guard);
                    }
                }
                "changed" => {
                    self.skip_inline_ws();
                    // `changed run <command>`: a block with one command.
                    if self.eat_keyword("run") {
                        let script = self.run_script("run")?;
                        body.block.steps.push(Step::Block(Block {
                            guards: vec![Guard::Changed],
                            steps: vec![Step::Run(script)],
                            ..Block::default()
                        }));
                        continue;
                    }
                    let opens_block = self.peek() == Some('{');
                    if opens_block {
                        self.bump();
                    }
                    self.end_of_line()?;
                    if opens_block {
                        let inner = self.body(BodyKind::Block, start)?;
                        let mut block = inner.block;
                        block.guards.insert(0, Guard::Changed);
                        body.block.steps.push(Step::Block(block));
                    } else if kind == BodyKind::Task {
                        let message = "a task's own `changed` has nothing before it: \
                            use `changed {` after the steps it follows, or a subtask";
                        return Err(self.error_at(start, message));
                    } else {
                        body.block.guards.push(Guard::Changed);
                    }
                }
                "sudo" => {
                    self.end_of_line()?;
                    body.block.sudo = true;
                }
                "terminal" => {
                    self.end_of_line()?;
                    body.block.terminal = true;
                }
                "manual" => {
                    task_only(self)?;
                    self.end_of_line()?;
                    body.manual = true;
                }
                "interactive" => {
                    task_only(self)?;
                    self.end_of_line()?;
                    body.interactive = true;
                }
                "workdir" => {
                    task_only(self)?;
                    body.workdir = Some(self.string()?);
                    self.end_of_line()?;
                }
                "let" => {
                    not_in_block(self)?;
                    let (name, var) = self.let_value()?;
                    body.vars.set(name, var);
                }
                "run" => {
                    let script = self.run_script("run")?;
                    body.block.steps.push(Step::Run(script));
                }
                "undo" => {
                    let script = self.run_script("undo")?;
                    body.block.steps.push(Step::Undo(script));
                }
                "keep" => {
                    self.end_of_line()?;
                    body.block.keep = true;
                }
                "notice" => {
                    let text = self.string()?;
                    self.end_of_line()?;
                    body.block.steps.push(Step::Notice(text));
                }
                "setting" => {
                    let setting = self.setting(start)?;
                    body.block.steps.push(Step::Setting(Box::new(setting)));
                }
                "line" => {
                    let line = self.line_step()?;
                    body.block.steps.push(Step::Line(Box::new(line)));
                }
                "file" | "directory" => {
                    let file = self.file_step(keyword == "directory")?;
                    body.block.steps.push(Step::File(Box::new(file)));
                }
                _ => {
                    let line = self.rest_of_line();
                    // Template arguments aren't shell, so strip trailing comments.
                    let args = strip_comment(&line).trim_end().to_string();
                    body.block.steps.push(Step::Call {
                        template: keyword,
                        args,
                    });
                    self.end_of_line()?;
                }
            }
        }
    }

    fn guard(&mut self, keyword: &str) -> Result<Guard> {
        let start = self.pos;
        match self.guard_line(keyword)? {
            (guard, false) => Ok(guard),
            (_, true) => {
                Err(self.error_at(start, "blocks are only allowed inside tasks and templates"))
            }
        }
    }

    /// A guard line, and whether it ends with `{` to open an inline block.
    fn guard_line(&mut self, keyword: &str) -> Result<(Guard, bool)> {
        if keyword == "ask" {
            let guard = Guard::Ask(self.string()?);
            self.skip_inline_ws();
            let opens_block = self.peek() == Some('{');
            if opens_block {
                self.bump();
            }
            self.end_of_line()?;
            return Ok((guard, opens_block));
        }
        let mut command = self.rest_of_line();
        // `unless check { # why` opens a block too, like `run { # why`.
        let code = strip_comment(&command).trim_end();
        let opens_block = code == "{" || code.ends_with(" {") || code.ends_with("\t{");
        if opens_block {
            command = code[..code.len() - 1].trim_end().to_string();
        }
        if command.is_empty() {
            return Err(self.error(format!("'{keyword}' needs a command")));
        }
        self.end_of_line()?;
        let guard = if keyword == "if" {
            Guard::If(command)
        } else {
            Guard::Unless(command)
        };
        Ok((guard, opens_block))
    }

    /// `include "x" if <command>`
    fn optional_guard(&mut self) -> Result<Option<Guard>> {
        self.skip_inline_ws();
        let save = self.pos;
        match self.word("").ok().as_deref() {
            Some(keyword @ ("if" | "unless")) => {
                let command = self.rest_of_line();
                if command.is_empty() {
                    return Err(self.error(format!("'{keyword}' needs a command")));
                }
                Ok(Some(if keyword == "if" {
                    Guard::If(command)
                } else {
                    Guard::Unless(command)
                }))
            }
            _ => {
                self.pos = save;
                Ok(None)
            }
        }
    }

    /// An optional `plugin NAME` after an `age-key` path.
    fn optional_plugin(&mut self) -> Result<Option<String>> {
        self.skip_inline_ws();
        let save = self.pos;
        if self.word("").ok().as_deref() == Some("plugin") {
            return Ok(Some(self.word("an age plugin name")?));
        }
        self.pos = save;
        Ok(None)
    }

    /// A task, subtask or template, after any flags before it:
    /// `[flags] task name [needs a b] [lock l] {`, `subtask name of parent ...`,
    /// `[flags] template name [params] [needs ...] [lock ...] {`.
    fn definition(
        &mut self,
        file: &mut TaskFile,
        keyword: &str,
        flags: Vec<(String, usize)>,
        start: usize,
    ) -> Result<()> {
        let template = keyword == "template";
        let name = self.word(if template {
            "a template name"
        } else {
            "a task name"
        })?;
        if template && is_keyword(&name) {
            return Err(self.error_at(start, format!("'{name}' is a keyword")));
        }
        let parent = if keyword == "subtask" {
            self.keyword("of")?;
            Some(self.word("the parent task name")?)
        } else {
            None
        };
        let origin = self.origin(start);
        let params = if template { self.params()? } else { Vec::new() };
        let (header_needs, header_locks) = self.header_clauses()?;
        self.open_brace()?;
        let kind = if template {
            BodyKind::Template
        } else {
            BodyKind::Task
        };
        let mut body = self.body(kind, start)?;
        for (flag, at) in &flags {
            match flag.as_str() {
                "sudo" => body.block.sudo = true,
                "terminal" => body.block.terminal = true,
                "keep" => body.block.keep = true,
                "manual" | "interactive" if template => {
                    return Err(self.error_at(*at, format!("'{flag}' can only be used on tasks")));
                }
                "manual" => body.manual = true,
                _ => body.interactive = true,
            }
        }
        let mut needs = header_needs;
        needs.extend(body.needs);
        let mut locks = header_locks;
        locks.append(&mut body.block.locks);
        body.block.locks = locks;
        if template {
            if file.templates.contains_key(&name) {
                return Err(self.error_at(start, format!("template '{name}' is defined twice")));
            }
            let template = Template {
                name: name.clone(),
                params,
                needs,
                vars: body.vars,
                body: body.block,
                origin,
            };
            file.templates.insert(name, template);
        } else {
            if file.tasks.contains_key(&name) {
                return Err(self.error_at(start, format!("task '{name}' is defined twice")));
            }
            let task = Task {
                name: name.clone(),
                desc: body.desc,
                needs,
                parent,
                manual: body.manual,
                interactive: body.interactive,
                workdir: body.workdir,
                vars: body.vars,
                body: body.block,
                origin,
            };
            file.tasks.insert(name, task);
        }
        Ok(())
    }

    /// `needs a b` and `lock name` clauses before a definition's `{`.
    fn header_clauses(&mut self) -> Result<(Vec<String>, Vec<String>)> {
        let (mut needs, mut locks) = (Vec::new(), Vec::new());
        loop {
            self.skip_inline_ws();
            if self.peek() == Some('{') || self.at_line_end() {
                return Ok((needs, locks));
            }
            let at = self.pos;
            let clause = self.word("'needs', 'lock' or '{'")?;
            let (list, what) = match clause.as_str() {
                "needs" => (&mut needs, "a task name"),
                "lock" => (&mut locks, "a lock name"),
                other => {
                    return Err(self.error_at(
                        at,
                        format!("expected 'needs', 'lock' or '{{', found '{other}'"),
                    ));
                }
            };
            let before = list.len();
            loop {
                self.skip_inline_ws();
                if self.peek() == Some('{') || self.at_line_end() {
                    break;
                }
                let save = self.pos;
                let word = self.word(what)?;
                if word == "needs" || word == "lock" {
                    self.pos = save;
                    break;
                }
                list.push(word);
            }
            if list.len() == before {
                return Err(self.error_at(at, format!("'{clause}' needs {what}")));
            }
        }
    }

    /// Named template parameters before the `{`, or a `needs` or `lock` clause:
    /// `a b c="default"`.
    fn params(&mut self) -> Result<Vec<Param>> {
        let mut params: Vec<Param> = Vec::new();
        loop {
            self.skip_inline_ws();
            if self.peek() == Some('{') || self.at_line_end() {
                return Ok(params);
            }
            let start = self.pos;
            let name = self.word("a parameter name or '{'")?;
            if name == "needs" || name == "lock" {
                self.pos = start;
                return Ok(params);
            }
            if !name.chars().all(crate::vars::is_var_char)
                || name.chars().all(|c| c.is_ascii_digit())
                || matches!(name.as_str(), "args" | "tmp" | "value")
            {
                return Err(self.error_at(start, format!("'{name}' can't be a parameter name")));
            }
            if params.iter().any(|p| p.name == name) {
                return Err(self.error_at(start, format!("parameter '{name}' is named twice")));
            }
            let default = if self.peek() == Some('=') {
                self.bump();
                Some(match self.peek() {
                    Some('"' | '\'') => self.string()?,
                    _ => self.word("a default value")?,
                })
            } else {
                if params.iter().any(|p| p.default.is_some()) {
                    return Err(self.error_at(
                        start,
                        format!(
                            "'{name}' comes after an optional parameter, so it needs a default too"
                        ),
                    ));
                }
                None
            };
            params.push(Param { name, default });
        }
    }

    /// `setting "value" {` with a `get` and a `set` line (or `{` blocks), and maybe `keep`.
    fn setting(&mut self, start: usize) -> Result<SettingStep> {
        self.skip_inline_ws();
        let value = match self.peek() {
            Some('"' | '\'') => self.string()?,
            _ => self.word("the value it should have")?,
        };
        self.open_brace()?;
        let (mut get, mut set, mut default, mut keep) = (None, None, None, false);
        loop {
            self.skip_trivia();
            if self.eof() {
                return Err(self.error_at(start, "missing closing '}'"));
            }
            if self.peek() == Some('}') {
                self.bump();
                self.end_of_line()?;
                break;
            }
            let at = self.pos;
            match self.word("'get', 'set', 'default' or 'keep'")?.as_str() {
                "get" if get.is_none() => get = Some(self.run_script("get")?),
                "set" if set.is_none() => set = Some(self.run_script("set")?),
                word @ ("get" | "set") => {
                    return Err(self.error_at(at, format!("a setting has one '{word}'")));
                }
                "keep" => {
                    self.end_of_line()?;
                    keep = true;
                }
                "default" if default.is_none() => {
                    self.skip_inline_ws();
                    default = Some(match self.peek() {
                        Some('"' | '\'') => self.string()?,
                        _ => self.word("the value undo sets without a record")?,
                    });
                    self.end_of_line()?;
                }
                other => {
                    return Err(self.error_at(
                        at,
                        format!(
                            "unexpected '{other}' in a setting, it has 'get', 'set' and maybe 'default' or 'keep'"
                        ),
                    ));
                }
            }
        }
        match (get, set) {
            (Some(get), Some(set)) => Ok(SettingStep {
                value,
                get,
                set,
                default,
                keep,
            }),
            (None, _) => Err(self.error_at(
                start,
                "a setting needs a `get` command that prints its value",
            )),
            (_, None) => Err(self.error_at(
                start,
                "a setting needs a `set` command that sets it to {{ value }}",
            )),
        }
    }

    /// `let name = "string" | $(command) | word`
    fn let_value(&mut self) -> Result<(String, Var)> {
        let name = self.word("a variable name")?;
        self.skip_inline_ws();
        if self.peek() != Some('=') {
            return Err(self.error("expected '=' after the variable name"));
        }
        self.bump();
        self.skip_inline_ws();
        if let Some(prompt) = self.prompt()? {
            self.end_of_line()?;
            return Ok((name, Var::Prompt(prompt, Default::default())));
        }
        let var = match self.peek() {
            Some('"' | '\'') => Var::Template(self.string()?),
            Some('$') if self.rest().starts_with("$(") => {
                let command = self.command_substitution()?;
                Var::Shell(command, Default::default())
            }
            _ => Var::Literal(self.word("a value")?),
        };
        self.end_of_line()?;
        Ok((name, var))
    }

    /// `confirm "Question"`, `choose "Question" a b "c d"` or `input "Question" ["default"]`.
    fn prompt(&mut self) -> Result<Option<Prompt>> {
        let save = self.pos;
        let kind = self.word("").ok();
        self.skip_inline_ws();
        let quoted = matches!(self.peek(), Some('"' | '\''));
        let kind = match kind.as_deref() {
            Some(k @ ("confirm" | "choose" | "input")) if quoted => k.to_string(),
            _ => {
                self.pos = save;
                return Ok(None);
            }
        };
        let question = self.string()?;
        let mut values = Vec::new();
        while !self.at_line_end() {
            if matches!(self.peek(), Some('"' | '\'')) {
                values.push(self.string()?);
            } else {
                values.push(self.word("an option")?);
            }
        }
        Ok(Some(match kind.as_str() {
            "confirm" if values.is_empty() => Prompt::Confirm(question),
            "confirm" => return Err(self.error("confirm takes only a question")),
            "choose" if values.len() >= 2 => Prompt::Choose(question, values),
            "choose" => return Err(self.error("choose needs at least two options")),
            _ if values.len() <= 1 => Prompt::Input(question, values.pop()),
            _ => return Err(self.error("input takes a question and an optional default")),
        }))
    }

    /// The command inside `$( ... )`, which may span continued lines.
    fn command_substitution(&mut self) -> Result<String> {
        let start = self.pos;
        let bytes = self.src.as_bytes();
        let mut i = start + 2;
        let mut depth = 0usize;
        let mut quote = None;
        while i < bytes.len() {
            let c = bytes[i];
            match (quote, c) {
                (_, b'\\') => i += 1,
                (Some(q), _) if c == q => quote = None,
                (Some(_), _) => {}
                (None, b'\'' | b'"') => quote = Some(c),
                (None, b'(') => depth += 1,
                (None, b')') if depth == 0 => {
                    let command = self.src[start + 2..i].replace("\\\n", " ");
                    self.pos = i + 1;
                    return Ok(command.trim().to_string());
                }
                (None, b')') => depth -= 1,
                (None, b'\n') => break,
                _ => {}
            }
            i += 1;
        }
        Err(self.error_at(start, "missing ')' for '$('"))
    }

    /// `run <command>` or a `run {` block of shell script ending with `}`
    /// (`keyword` is `run` or `undo`).
    fn run_script(&mut self, keyword: &str) -> Result<String> {
        self.skip_inline_ws();
        if self.peek() == Some('{') {
            let save = self.pos;
            self.bump();
            if self.at_line_end() {
                self.end_of_line()?;
                let script = self.raw_block(save)?;
                self.end_of_line()?;
                return Ok(script);
            }
            self.pos = save;
        }
        let command = self.rest_of_line();
        if command.is_empty() {
            return Err(self.error(format!("'{keyword}' needs a command")));
        }
        self.end_of_line()?;
        Ok(command)
    }

    /// `file "path" [mode 755] [owner user:group] [encoding utf-16le] [sudo] ("content" | from "path")`,
    /// or `directory "path" ... from "dir"` for every file in a directory.
    fn file_step(&mut self, dir: bool) -> Result<FileStep> {
        let path = self.string()?;
        let mut origin = FileOrigin {
            file: self.path.clone(),
            after_path: self.pos,
            ..FileOrigin::default()
        };
        let mut mode = None;
        let mut owner = None;
        let mut encoding = None;
        let mut sudo = false;
        let mut new = false;
        let mut keep = false;
        let source = loop {
            self.skip_inline_ws();
            if dir && self.at_line_end() {
                break FileSource::Empty;
            }
            if let Some('"' | '\'') = self.peek() {
                let start = self.pos;
                let text = self.string()?;
                origin.content = Some(self.span(start));
                break FileSource::Inline(text);
            }
            let start = self.pos;
            match self
                .word("file content, 'from', 'url', 'mode', 'owner', 'encoding', 'sudo', 'new' or 'keep'")?
                .as_str()
            {
                "mode" => {
                    self.skip_inline_ws();
                    let value_start = self.pos;
                    let value = self.word("a file mode")?;
                    origin.mode = Some(self.span(value_start));
                    let parsed = u32::from_str_radix(&value, 8)
                        .ok()
                        .filter(|m| *m <= 0o7777)
                        .ok_or_else(|| {
                            self.error_at(start, format!("invalid octal mode '{value}'"))
                        })?;
                    mode = Some(parsed);
                }
                "owner" => {
                    self.skip_inline_ws();
                    let value_start = self.pos;
                    owner = Some(if let Some('"' | '\'') = self.peek() {
                        self.string()?
                    } else {
                        let user = self.word("a user")?;
                        if self.peek() == Some(':') {
                            self.bump();
                            format!("{user}:{}", self.word("a group")?)
                        } else {
                            user
                        }
                    });
                    origin.owner = Some(self.span(value_start));
                }
                "encoding" => {
                    self.skip_inline_ws();
                    let value_start = self.pos;
                    let name = self.word("an encoding")?;
                    origin.encoding = Some(self.span(value_start));
                    encoding = Some(Encoding::parse(&name).ok_or_else(|| {
                        self.error_at(
                            start,
                            format!(
                                "unknown encoding '{name}', use one of: {}",
                                Encoding::NAMES.join(", ")
                            ),
                        )
                    })?);
                }
                "sudo" => sudo = true,
                "new" => new = true,
                "keep" => keep = true,
                "url" => {
                    let url = self.string()?;
                    self.skip_inline_ws();
                    let at = self.pos;
                    if self.word("'sha256'").ok().as_deref() != Some("sha256") {
                        return Err(self.error_at(
                            at,
                            "a downloaded file needs its checksum: url \"...\" sha256 \"...\"",
                        ));
                    }
                    let at = self.pos;
                    let sha256 = self.string()?.to_ascii_lowercase();
                    if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
                        return Err(self.error_at(at, "sha256 is 64 hexadecimal digits"));
                    }
                    break FileSource::Url { url, sha256 };
                }
                "from" => {
                    let source = self.string()?;
                    let relative = !(source.contains("{{")
                        || source.starts_with('~')
                        || Path::new(&source).is_absolute());
                    break FileSource::Copy(if relative {
                        self.dir().join(&source).display().to_string()
                    } else {
                        source
                    });
                }
                other => {
                    return Err(self.error_at(start, format!("unexpected '{other}' in file step")));
                }
            }
        };
        if dir && let FileSource::Url { .. } = source {
            return Err(self.error_at(
                origin.after_path,
                "a directory can't be downloaded, only a file",
            ));
        }
        if dir && let FileSource::Inline(_) = source {
            return Err(self.error_at(
                origin.after_path,
                "a directory is copied `from` one, like `directory \"/etc/app\" from \"files/app\"`, or has no content",
            ));
        }
        if matches!(source, FileSource::Empty) && encoding.is_some() {
            return Err(self.error_at(
                origin.after_path,
                "a directory without `from` has no encoding",
            ));
        }
        self.end_of_line()?;
        if matches!(source, FileSource::Url { .. }) && encoding.is_some() {
            return Err(self.error_at(
                origin.after_path,
                "a downloaded file is written as it is, it has no encoding",
            ));
        }
        let raw =
            encoding.is_none() && matches!(source, FileSource::Copy(_) | FileSource::Url { .. });
        Ok(FileStep {
            path,
            dir,
            source,
            mode,
            owner,
            encoding: if raw {
                Encoding::Utf8
            } else {
                encoding.unwrap_or_default()
            },
            raw,
            sudo,
            new,
            keep,
            origin,
        })
    }

    /// `line "path" [sudo] [new] [keep] [base "path"] [match "start"] [before "start" | after "start"] "text"`
    fn line_step(&mut self) -> Result<LineStep> {
        let path = self.string()?;
        let mut step = LineStep {
            path,
            text: String::new(),
            place: None,
            base: None,
            matcher: None,
            new: false,
            sudo: false,
            keep: false,
        };
        loop {
            self.skip_inline_ws();
            if let Some('"' | '\'') = self.peek() {
                let start = self.pos;
                step.text = self.string()?;
                if step.text.contains('\n') {
                    return Err(self.error_at(start, "a line can't have a newline in it"));
                }
                if step.text.trim().is_empty() {
                    return Err(self.error_at(start, "the line is empty"));
                }
                break;
            }
            let start = self.pos;
            match self
                .word("the line, 'before', 'after', 'match', 'base', 'sudo', 'new' or 'keep'")?
                .as_str()
            {
                "sudo" => step.sudo = true,
                "keep" => step.keep = true,
                "base" => step.base = Some(self.string()?),
                "new" => step.new = true,
                "match" => {
                    let prefix = self.string()?;
                    if prefix.trim().is_empty() {
                        return Err(self.error_at(start, "'match' needs the start of a line"));
                    }
                    step.matcher = Some(prefix);
                }
                word @ ("before" | "after") => {
                    if step.place.is_some() {
                        return Err(self.error_at(start, "only one of 'before' or 'after'"));
                    }
                    let prefix = self.string()?;
                    step.place = Some(if word == "before" {
                        LinePlace::Before(prefix)
                    } else {
                        LinePlace::After(prefix)
                    });
                }
                other => {
                    return Err(self.error_at(start, format!("unexpected '{other}' in line step")));
                }
            }
        }
        self.end_of_line()?;
        Ok(step)
    }

    /// The source from `start` to here.
    fn span(&self, start: usize) -> Span {
        Span {
            start,
            end: self.pos,
            text: self.src[start..self.pos].to_string(),
        }
    }

    // ---- Lexing -----------------------------------------------------------

    fn rest(&self) -> &'s str {
        &self.src[self.pos..]
    }

    fn eof(&self) -> bool {
        self.pos >= self.src.len()
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) {
        if let Some(c) = self.peek() {
            self.pos += c.len_utf8();
        }
    }

    /// Skip spaces, tabs and `\` line continuations.
    fn skip_inline_ws(&mut self) {
        loop {
            let rest = self.rest();
            if rest.starts_with([' ', '\t']) {
                self.pos += 1;
            } else if rest.starts_with("\\\n") {
                self.pos += 2;
            } else if rest.starts_with("\\\r\n") {
                self.pos += 3;
            } else {
                return;
            }
        }
    }

    /// Skip whitespace, newlines and comment lines.
    fn skip_trivia(&mut self) {
        loop {
            self.skip_inline_ws();
            match self.peek() {
                Some('\n' | '\r') => self.bump(),
                Some('#') => self.skip_to_line_end(),
                _ => return,
            }
        }
    }

    fn skip_to_line_end(&mut self) {
        let len = self.rest().find('\n').unwrap_or(self.rest().len());
        self.pos += len;
    }

    fn at_line_end(&mut self) -> bool {
        self.skip_inline_ws();
        matches!(self.peek(), None | Some('\n' | '\r' | '#'))
    }

    fn end_of_line(&mut self) -> Result<()> {
        if !self.at_line_end() {
            return Err(self.error(format!(
                "unexpected '{}', expected the end of the line",
                self.peek().unwrap_or(' ')
            )));
        }
        if self.peek() == Some('#') {
            self.skip_to_line_end();
        }
        if self.rest().starts_with("\r\n") {
            self.pos += 2;
        } else if self.peek() == Some('\n') {
            self.pos += 1;
        }
        Ok(())
    }

    fn word(&mut self, what: &str) -> Result<String> {
        self.skip_inline_ws();
        let len = self
            .rest()
            .find(|c: char| !is_word_char(c))
            .unwrap_or(self.rest().len());
        if len == 0 {
            return Err(match self.peek() {
                Some(c) => self.error(format!("expected {what} but found '{c}'")),
                None => self.error(format!("expected {what} but the file ended")),
            });
        }
        let word = self.rest()[..len].to_string();
        self.pos += len;
        Ok(word)
    }

    /// One or more words up to the end of the line.
    fn words(&mut self, what: &str) -> Result<Vec<String>> {
        let mut words = vec![self.word(what)?];
        while !self.at_line_end() {
            words.push(self.word(what)?);
        }
        self.end_of_line()?;
        Ok(words)
    }

    /// Move past `keyword` if it's next, as a whole word.
    fn eat_keyword(&mut self, keyword: &str) -> bool {
        let found = self
            .rest()
            .strip_prefix(keyword)
            .is_some_and(|after| !after.starts_with(is_word_char));
        if found {
            self.pos += keyword.len();
        }
        found
    }

    fn keyword(&mut self, expected: &str) -> Result<()> {
        let start = self.pos;
        let word = self.word(&format!("'{expected}'"))?;
        if word != expected {
            return Err(self.error_at(start, format!("expected '{expected}' but found '{word}'")));
        }
        Ok(())
    }

    fn open_brace(&mut self) -> Result<()> {
        self.skip_inline_ws();
        if self.peek() != Some('{') {
            return Err(self.error("expected '{'"));
        }
        self.bump();
        self.end_of_line()
    }

    /// The raw rest of the line, with `\` continuations joined by a space.
    fn rest_of_line(&mut self) -> String {
        self.skip_inline_ws();
        let mut out = String::new();
        loop {
            let rest = self.rest();
            let end = rest.find('\n').unwrap_or(rest.len());
            let line = rest[..end].trim_end_matches('\r');
            if let Some(continued) = line.strip_suffix('\\') {
                out.push_str(continued.trim_end());
                out.push(' ');
                // A `\` on the last line of the file has no newline after it.
                self.pos += (end + 1).min(rest.len());
                self.skip_inline_ws();
            } else {
                out.push_str(line);
                self.pos += line.len();
                return out.trim().to_string();
            }
        }
    }

    /// A shell script ending at the matching `}`. Quotes and comments are
    /// skipped when matching braces.
    fn raw_block(&mut self, open: usize) -> Result<String> {
        let bytes = self.src.as_bytes();
        let start = self.pos;
        let mut i = start;
        let mut depth = 0usize;
        let mut word_start = true;
        let unclosed =
            |p: &Self, what: &str| p.error_at(open, format!("unclosed {what} in run block"));
        while i < bytes.len() {
            let c = bytes[i];
            match c {
                b'\\' => {
                    i += 2;
                    word_start = false;
                    continue;
                }
                b'\'' => {
                    let ansi_c = i > 0 && bytes[i - 1] == b'$';
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'\'' {
                        i += if ansi_c && bytes[i] == b'\\' { 2 } else { 1 };
                    }
                    if i >= bytes.len() {
                        return Err(unclosed(self, "quote"));
                    }
                }
                b'"' => {
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'"' {
                        i += if bytes[i] == b'\\' { 2 } else { 1 };
                    }
                    if i >= bytes.len() {
                        return Err(unclosed(self, "quote"));
                    }
                }
                b'#' if word_start => {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                b'{' => depth += 1,
                b'}' if depth == 0 => {
                    let script = dedent(&self.src[start..i], None);
                    self.pos = i + 1;
                    return Ok(script);
                }
                b'}' => depth -= 1,
                _ => {}
            }
            word_start = matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b';' | b'&' | b'|' | b'(');
            i += 1;
        }
        Err(unclosed(self, "'{'"))
    }

    /// `"..."` with escapes, `'...'` without, or a `"""` block.
    fn string(&mut self) -> Result<String> {
        self.skip_inline_ws();
        let start = self.pos;
        if self.rest().starts_with("\"\"\"") {
            self.pos += 3;
            let Some(end) = self.rest().find("\"\"\"") else {
                return Err(self.error_at(start, "unclosed '\"\"\"'"));
            };
            let raw = &self.rest()[..end];
            self.pos += end + 3;
            return Ok(triple_quoted(raw));
        }
        match self.peek() {
            Some('\'') => {
                self.bump();
                let Some(end) = self.rest().find('\'') else {
                    return Err(self.error_at(start, "unclosed string"));
                };
                let value = self.rest()[..end].to_string();
                self.pos += end + 1;
                Ok(value)
            }
            Some('"') => {
                self.bump();
                let mut value = String::new();
                let mut chars = self.rest().char_indices();
                while let Some((i, c)) = chars.next() {
                    match c {
                        '"' => {
                            self.pos += i + 1;
                            return Ok(value);
                        }
                        '\\' => match chars.next() {
                            Some((_, 'n')) => value.push('\n'),
                            Some((_, 't')) => value.push('\t'),
                            Some((_, c @ ('"' | '\\'))) => value.push(c),
                            Some((_, c)) => {
                                value.push('\\');
                                value.push(c);
                            }
                            None => break,
                        },
                        c => value.push(c),
                    }
                }
                Err(self.error_at(start, "unclosed string"))
            }
            _ => Err(self.error("expected a string")),
        }
    }

    // ---- Errors -----------------------------------------------------------

    fn resolve(&self, path: &str) -> PathBuf {
        let path = PathBuf::from(expand_tilde(path));
        if path.is_absolute() {
            path
        } else {
            self.dir().join(path)
        }
    }

    fn line_col(&self, pos: usize) -> (usize, usize) {
        let before = &self.src[..pos.min(self.src.len())];
        let line = before.matches('\n').count() + 1;
        let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        (line, col)
    }

    fn declare(&self, file: &mut TaskFile, pos: usize, kind: DeclarationKind, path: &Path) {
        file.declarations.push(Declaration {
            file: self.path.clone(),
            line: self.line_col(pos).0,
            kind,
            path: path.to_path_buf(),
        });
    }

    fn origin(&self, pos: usize) -> String {
        let (line, _) = self.line_col(pos);
        format!("{}:{line}", self.path.display())
    }

    fn error(&self, message: impl std::fmt::Display) -> anyhow::Error {
        self.error_at(self.pos, message)
    }

    fn error_at(&self, pos: usize, message: impl std::fmt::Display) -> anyhow::Error {
        let (line, col) = self.line_col(pos);
        anyhow!("{}:{line}:{col}: {message}", self.path.display())
    }
}

/// Flags that can come before `task`, `subtask` or `template`.
fn is_header_flag(word: &str) -> bool {
    matches!(
        word,
        "sudo" | "terminal" | "keep" | "manual" | "interactive"
    )
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '+')
}

fn is_keyword(word: &str) -> bool {
    matches!(
        word,
        "desc"
            | "needs"
            | "lock"
            | "if"
            | "unless"
            | "ask"
            | "changed"
            | "sudo"
            | "terminal"
            | "manual"
            | "interactive"
            | "workdir"
            | "let"
            | "run"
            | "undo"
            | "keep"
            | "notice"
            | "file"
            | "directory"
            | "line"
            | "setting"
            | "else"
    )
}

/// `line` without a trailing `# comment`: a `#` at the start or after
/// whitespace, outside quotes and not escaped, as the shell reads it.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    let mut after_space = true;
    for (i, c) in line.char_indices() {
        if escaped {
            escaped = false;
            after_space = false;
            continue;
        }
        match quote {
            Some('"') if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\\' => escaped = true,
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '#' && after_space => return &line[..i],
            None => {}
        }
        after_space = c.is_whitespace();
    }
    line
}

/// Content of a `"""` string. A leading newline is dropped, and when the
/// closing `"""` is on its own line, its indentation is removed from every line.
fn triple_quoted(raw: &str) -> String {
    let raw = raw
        .strip_prefix("\r\n")
        .or_else(|| raw.strip_prefix('\n'))
        .unwrap_or(raw);
    match raw.rsplit_once('\n') {
        Some((body, last)) if last.trim().is_empty() => dedent(body, Some(last.len())) + "\n",
        _ => dedent(raw, None),
    }
}

/// Remove common indentation (or `indent` columns, if given) from all lines,
/// and blank lines from the start and end.
fn dedent(text: &str, indent: Option<usize>) -> String {
    let lines: Vec<&str> = text.lines().collect();
    // Only spaces and tabs count, so the cut is always at a character boundary.
    let leading = |l: &str| l.len() - l.trim_start_matches([' ', '\t']).len();
    let indent = indent.unwrap_or_else(|| {
        lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| leading(l))
            .min()
            .unwrap_or(0)
    });
    let stripped: Vec<&str> = lines.iter().map(|l| &l[leading(l).min(indent)..]).collect();
    let first = stripped.iter().position(|l| !l.trim().is_empty());
    let last = stripped.iter().rposition(|l| !l.trim().is_empty());
    match (first, last) {
        (Some(first), Some(last)) => stripped[first..=last].join("\n"),
        _ => String::new(),
    }
}

/// The value of one string literal, as the parser reads it.
pub fn string_literal(text: &str) -> Result<String> {
    let mut parser = Parser::new(text, PathBuf::from("<string>"));
    let value = parser.string()?;
    if !parser.rest().trim().is_empty() {
        bail!("more than one string");
    }
    Ok(value)
}

pub fn guard_passes(guard: &Guard, scope: &Scope, cwd: &Path) -> Result<bool> {
    match guard {
        Guard::If(command) => scope.ctx.check(&scope.expand(command)?, cwd),
        Guard::Unless(command) => Ok(!scope.ctx.check(&scope.expand(command)?, cwd)?),
        Guard::Ask(prompt) => crate::exec::ask(&scope.expand(prompt)?),
        // Only a task run knows; see `TaskRun::nested`.
        Guard::Changed => bail!("`changed` is only checked inside a task"),
    }
}

/// Load `KEY=value` lines. Values become variables and are exported to commands.
fn load_env_file(path: &Path, file: &mut TaskFile) -> Result<()> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read env file {}", path.display()))?;
    for (n, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow!("{}:{}: expected KEY=value", path.display(), n + 1))?;
        let key = key.trim();
        // Keys become shell variables (written into remote scripts as is).
        let valid = key.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            bail!(
                "{}:{}: invalid variable name {key:?}",
                path.display(),
                n + 1
            );
        }
        let value = value.trim();
        let value = ['"', '\'']
            .iter()
            .find_map(|q| value.strip_prefix(*q).and_then(|v| v.strip_suffix(*q)))
            .unwrap_or(value);
        file.vars.literal(key, value);
        file.ctx.exports.push((key.to_string(), value.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> TaskFile {
        parse_str(source, Path::new("/tmp")).unwrap_or_else(|e| panic!("{e:#}"))
    }

    fn parse_err(source: &str) -> String {
        match parse_str(source, Path::new("/tmp")) {
            Ok(_) => panic!("expected an error"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn parses_age_key_plugin() {
        let file = parse("age-key \"k.age\" plugin fido2-hmac # comment\n");
        assert!(format!("{:?}", file.ctx.secrets).contains("age_plugin: Some(\"fido2-hmac\")"));
        let file = parse("age-key \"k.age\"\n");
        assert!(format!("{:?}", file.ctx.secrets).contains("age_plugin: None"));
        assert!(parse_err("age-key \"k.age\" plugin\n").contains("expected an age plugin name"));
        assert!(parse_err("age-key \"k.age\" yubikey\n").contains("expected the end of the line"));
    }

    #[test]
    fn parses_notices() {
        let file = parse("task t {\n    notice \"check {{ x }}\" # why\n}\n");
        let steps = &file.tasks["t"].body.steps;
        assert!(matches!(&steps[..], [Step::Notice(text)] if text == "check {{ x }}"));
        assert!(parse_err("task t {\n    notice\n}\n").contains("2:11: expected a string"));
    }

    #[test]
    fn parses_tasks() {
        let file = parse(
            r#"
            # A comment
            let name = "taskpod"
            let bare = value
            let version = $(echo "a)" $(echo 1.0)) # comment

            task build {
                desc "Build {{ name }}"
                needs lint test   # trailing comment
                if test -f Cargo.toml
                unless false
                run cargo build --release --features=a,b \
                    --locked
                run {
                    cd target
                    echo "}" '{' ${HOME} # a } in a comment
                }
            }

            task lint {
                manual
                run cargo clippy
            }
            task test {
                run cargo test
            }
            subtask after of build {
                run echo done
            }
            "#,
        );
        let build = &file.tasks["build"];
        assert_eq!(build.desc.as_deref(), Some("Build {{ name }}"));
        assert_eq!(build.needs, ["lint", "test"]);
        assert_eq!(build.body.guards.len(), 2);
        let scripts: Vec<String> = build.body.steps.iter().map(Step::describe).collect();
        assert_eq!(
            scripts,
            [
                "cargo build --release --features=a,b --locked",
                "cd target\necho \"}\" '{' ${HOME} # a } in a comment",
            ]
        );
        assert!(file.tasks["lint"].manual);
        assert_eq!(file.tasks["after"].parent.as_deref(), Some("build"));
        assert!(matches!(file.vars.get("bare"), Some(Var::Literal(v)) if v == "value"));
        assert!(
            matches!(file.vars.get("version"), Some(Var::Shell(c, _)) if c == "echo \"a)\" $(echo 1.0)")
        );
    }

    #[test]
    fn templates_hoist_needs() {
        let file = parse(
            r#"
            template pkg {
                needs setup
                lock pacman
                unless pacman -Q {{ args }}
                run yay -S {{ args }}
            }
            task setup {
                run true
            }
            task apps {
                pkg a b \
                    c
                pkg d  # a comment
            }
            "#,
        );
        let apps = &file.tasks["apps"];
        assert_eq!(apps.needs, ["setup"]);
        assert!(
            matches!(&apps.body.steps[0], Step::Call { template, args } if template == "pkg" && args == "a b c")
        );
        assert!(matches!(&apps.body.steps[1], Step::Call { args, .. } if args == "d"));
        assert_eq!(
            file.lock_names().into_iter().collect::<Vec<_>>(),
            ["pacman"]
        );
    }

    #[test]
    fn parses_terminal() {
        let file = parse("task t {\n terminal\n if true {\n  terminal\n  run x\n }\n run y\n}\n");
        let body = &file.tasks["t"].body;
        assert!(body.terminal && !body.sudo);
        let Step::Block(inner) = &body.steps[0] else {
            panic!()
        };
        assert!(inner.terminal);
    }

    #[test]
    fn parses_files() {
        let file = parse(
            r#"
            task files {
                file "/etc/a.conf" sudo """
                    line one
                      indented {
                    """
                file "~/b" mode 600 "x\ty\n"
                file "/etc/c" sudo mode 644 from "c.conf"
            }
            "#,
        );
        let steps = &file.tasks["files"].body.steps;
        let Step::File(a) = &steps[0] else { panic!() };
        assert!(a.sudo);
        assert!(matches!(&a.source, FileSource::Inline(s) if s == "line one\n  indented {\n"));
        let Step::File(b) = &steps[1] else { panic!() };
        assert_eq!(b.mode, Some(0o600));
        assert!(matches!(&b.source, FileSource::Inline(s) if s == "x\ty\n"));
        let Step::File(c) = &steps[2] else { panic!() };
        assert!(matches!(&c.source, FileSource::Copy(s) if s == "/tmp/c.conf"));
    }

    #[test]
    fn parses_prompts_blocks_and_file_options() {
        let file = parse(
            r#"
            let de = choose "Desktop?" kde gnome "none at all"
            let host = input "Hostname" "arch"
            let name = input "Name"
            let games = confirm "Games?"
            task t {
                unless test -e x {
                    run touch x
                    pkg a
                }
                ask "Sure?" {
                    run echo yes
                }
                file "/etc/x" owner root:wheel mode 640 encoding UTF-16LE sudo "x"
                file "/etc/y" owner "{{ user }}" from "y.bin"
            }
            template pkg {
                run echo {{ args }}
            }
            "#,
        );
        assert!(
            matches!(file.vars.get("de"), Some(Var::Prompt(Prompt::Choose(_, o), _)) if o.len() == 3 && o[2] == "none at all")
        );
        assert!(
            matches!(file.vars.get("host"), Some(Var::Prompt(Prompt::Input(_, Some(d)), _)) if d == "arch")
        );
        assert!(matches!(
            file.vars.get("name"),
            Some(Var::Prompt(Prompt::Input(_, None), _))
        ));
        assert!(matches!(
            file.vars.get("games"),
            Some(Var::Prompt(Prompt::Confirm(_), _))
        ));
        let steps = &file.tasks["t"].body.steps;
        let Step::Block(block) = &steps[0] else {
            panic!()
        };
        assert!(matches!(&block.guards[0], Guard::Unless(c) if c == "test -e x"));
        assert_eq!(block.steps.len(), 2);
        assert!(matches!(&steps[1], Step::Block(b) if matches!(&b.guards[0], Guard::Ask(_))));
        let Step::File(x) = &steps[2] else { panic!() };
        assert_eq!(
            (x.owner.as_deref(), x.mode, x.encoding, x.raw),
            (Some("root:wheel"), Some(0o640), Encoding::Utf16Le, false)
        );
        let Step::File(y) = &steps[3] else { panic!() };
        assert!(y.raw);
        assert_eq!(y.owner.as_deref(), Some("{{ user }}"));

        assert!(parse_err("let x = choose \"Q\" one\n").contains("at least two"));
        assert!(
            parse_err("task t {\n  file \"/x\" encoding ebcdic \"x\"\n}\n")
                .contains("unknown encoding 'ebcdic'")
        );
        assert!(parse_err("if true {\n}\n").contains("only allowed inside tasks"));
        assert!(parse_err("task t {\n  if true {\n    needs a\n  }\n}\n").contains("inline block"));
    }

    #[test]
    fn comments_after_block_openers_and_calls() {
        let file = parse(
            "task t {\n    unless test -e x { # make x\n        pkg a \"b # c\"\t# why\n    }\n    if grep -q 'a { #b' f\n}\ntemplate pkg {\n    run echo {{ args }}\n}\n",
        );
        let steps = &file.tasks["t"].body.steps;
        let Step::Block(block) = &steps[0] else {
            panic!("{steps:?}")
        };
        assert!(matches!(&block.guards[0], Guard::Unless(c) if c == "test -e x"));
        assert!(
            matches!(&block.steps[0], Step::Call { args, .. } if args == "a \"b # c\""),
            "{:?}",
            block.steps
        );
        // A `{` inside quotes doesn't open a block.
        let guards = &file.tasks["t"].body.guards;
        assert!(matches!(&guards[..], [Guard::If(c)] if c == "grep -q 'a { #b' f"));
        assert_eq!(strip_comment(r"echo a\ #b # c"), r"echo a\ #b ");
    }

    #[test]
    fn handles_unicode() {
        let file = parse("task ünï {\n    desc \"→ done ✓\"\n    run echo ✓\n}\n");
        assert_eq!(file.tasks["ünï"].desc.as_deref(), Some("→ done ✓"));
    }

    #[test]
    fn reports_errors_with_positions() {
        assert!(parse_err("task a {\n  run true\n").contains("<string>:1:1: missing closing '}'"));
        assert!(
            parse_err("task a {\n  frobnicate x\n}\n")
                .contains("unknown directive or template 'frobnicate'")
        );
        assert!(parse_err("task a {\n  needs b\n}\n").contains("needs unknown task 'b'"));
        assert!(parse_err("task a {\n}\ntask a {\n}\n").contains("defined twice"));
        assert!(parse_err("template t {\n  t\n}\ntask a {\n  t\n}\n").contains("calls itself"));
        assert!(parse_err("bogus\n").contains("<string>:1:1: unknown statement 'bogus'"));
        assert!(parse_err("task a {\n  run {\n    echo \"x\n  }\n}\n").contains("unclosed quote"));
    }

    #[test]
    fn env_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "# comment\n\nexport A=1\nB = \"two words\"\nC='x=y'\n",
        )
        .unwrap();
        let source = "env \".env\"\ntask t {\n  run true\n}\n";
        let path = dir.path().join("main.task");
        std::fs::write(&path, source).unwrap();
        let file = parse_file(&path).unwrap();
        let scope = Scope::root(&file.vars, &file.ctx);
        assert_eq!(
            scope.expand("{{ A }}|{{ B }}|{{ C }}").unwrap(),
            "1|two words|x=y"
        );
        assert_eq!(file.ctx.exports.len(), 3);

        std::fs::write(dir.path().join(".env"), "A;touch x=1\n").unwrap();
        let err = format!("{:#}", parse_file(&path).err().unwrap());
        assert!(err.contains("invalid variable name"), "{err}");
    }

    #[test]
    fn odd_input_doesnt_panic() {
        // A continued line at the very end of the file.
        assert!(parse_err("task a {\n  run echo \\").contains("missing closing '}'"));
        // Indentation that isn't only spaces and tabs.
        let file = parse("task a {\n  run {\n \u{3000}x\n  y\n  }\n}\n");
        let steps: Vec<String> = file.tasks["a"]
            .body
            .steps
            .iter()
            .map(Step::describe)
            .collect();
        assert_eq!(steps, ["\u{3000}x\n y"]);
    }

    #[cfg(unix)]
    #[test]
    fn includes_are_relative_and_guarded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/a.task"), "task a {\n run true\n}\n").unwrap();
        std::fs::write(dir.path().join("sub/b.task"), "task b {\n run true\n}\n").unwrap();
        std::fs::write(
            dir.path().join("main.task"),
            "include \"sub/a.task\"\ninclude \"sub/b.task\" if false\n",
        )
        .unwrap();
        let file = parse_file(&dir.path().join("main.task")).unwrap();
        assert!(file.tasks.contains_key("a"));
        assert!(!file.tasks.contains_key("b"));
    }
}
