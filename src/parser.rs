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
                "vars" => {
                    let path = self.string()?;
                    self.skip_inline_ws();
                    let save = self.pos;
                    let prefix = if self.word("").ok().as_deref() == Some("as") {
                        let at = self.pos;
                        let name = self.word("a name for the variables")?;
                        if !name.chars().all(crate::vars::is_var_char) || name.contains('.') {
                            return Err(self.error_at(at, format!("invalid name '{name}'")));
                        }
                        Some(name)
                    } else {
                        self.pos = save;
                        None
                    };
                    self.end_of_line()?;
                    let path = self.resolve(&path);
                    self.declare(file, start, DeclarationKind::Vars, &path);
                    load_vars_file(&path, prefix.as_deref(), file)
                        .map_err(|e| self.error_at(start, e))?;
                }
                "secrets" => {
                    let path = self.string()?;
                    self.end_of_line()?;
                    let path = self.resolve(&path);
                    for path in glob(&path).map_err(|e| self.error_at(start, e))? {
                        self.declare(file, start, DeclarationKind::Secrets, &path);
                        file.ctx.secrets.add(path);
                    }
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
                    let dir = self.string()?;
                    self.end_of_line()?;
                    let slot = if kind == BodyKind::Task {
                        &mut body.workdir
                    } else {
                        &mut body.block.workdir
                    };
                    if slot.is_some() {
                        return Err(self.error_at(start, "'workdir' is given twice"));
                    }
                    *slot = Some(dir);
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
                "wait" => {
                    let wait = self.wait_step()?;
                    body.block.steps.push(Step::Wait(Box::new(wait)));
                }
                "setting" => {
                    let setting = self.setting(start)?;
                    body.block.steps.push(Step::Setting(Box::new(setting)));
                }
                "line" => {
                    let line = self.line_step()?;
                    body.block.steps.push(Step::Line(Box::new(line)));
                }
                "lines" => {
                    for line in self.lines_block(start)? {
                        body.block.steps.push(Step::Line(Box::new(line)));
                    }
                }
                "file" | "directory" => {
                    let file = self.file_step(keyword == "directory")?;
                    body.block.steps.push(Step::File(Box::new(file)));
                }
                _ => {
                    let line = self.rest_of_line();
                    // Template arguments aren't shell, so strip trailing comments.
                    let args = strip_comment(&line).trim_end();
                    let head = args
                        .strip_suffix('{')
                        .filter(|h| h.is_empty() || h.ends_with([' ', '\t']));
                    let Some(head) = head.map(str::trim_end) else {
                        body.block.steps.push(Step::Call {
                            template: keyword,
                            args: args.to_string(),
                        });
                        self.end_of_line()?;
                        continue;
                    };
                    // `name a {` with lines of more words: a call for each line.
                    let head = head.to_string();
                    self.end_of_line()?;
                    let mut calls = 0;
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
                        let line = self.rest_of_line();
                        let more = strip_comment(&line).trim_end();
                        body.block.steps.push(Step::Call {
                            template: keyword.clone(),
                            args: format!("{head} {more}").trim().to_string(),
                        });
                        calls += 1;
                        self.end_of_line()?;
                    }
                    if calls == 0 {
                        return Err(self.error_at(
                            start,
                            format!(
                                "`{keyword} {{` has no lines: each line is a call of '{keyword}'"
                            ),
                        ));
                    }
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
        let (check, opens_block) = self.check(keyword)?;
        let guard = if keyword == "if" {
            Guard::If(check)
        } else {
            Guard::Unless(check)
        };
        Ok((guard, opens_block))
    }

    /// The check after `if`, `unless` or `until`, to the end of the line, and
    /// whether it ends with `{` (opening a block). A built-in check if it's
    /// one as a whole, otherwise a shell command.
    fn check(&mut self, keyword: &str) -> Result<(Check, bool)> {
        self.skip_inline_ws();
        let start = self.pos;
        if let Some(kind) = self.builtin_check()? {
            let text = self.src[start..self.pos]
                .replace("\\\r\n", " ")
                .replace("\\\n", " ");
            self.skip_inline_ws();
            let opens_block = self.peek() == Some('{');
            if opens_block {
                self.bump();
            }
            self.end_of_line()?;
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            return Ok((Check { text, kind }, opens_block));
        }
        self.pos = start;
        let mut command = self.rest_of_line();
        // `unless check { # why` opens a block too, like `run { # why`.
        let code = strip_comment(&command).trim_end();
        let opens_block = code == "{" || code.ends_with(" {") || code.ends_with("\t{");
        if opens_block {
            command = code[..code.len() - 1].trim_end().to_string();
        }
        if command.is_empty() {
            return Err(self.error(format!("'{keyword}' needs a check")));
        }
        self.end_of_line()?;
        Ok((Check::shell(command), opens_block))
    }

    /// `exists "path"`, `which name` or `$(command) <compare> "value"`, if
    /// the check is one of them; the position is left after it. Anything
    /// else is a shell command, and returns `None`.
    fn builtin_check(&mut self) -> Result<Option<CheckKind>> {
        let start = self.pos;
        if self.rest().starts_with("$(") {
            let command = self.command_substitution()?;
            self.skip_inline_ws();
            let at = self.pos;
            let compare = if self.rest().starts_with("==") {
                self.pos += 2;
                Compare::Equals
            } else if self.rest().starts_with("!=") {
                self.pos += 2;
                Compare::NotEquals
            } else if self.eat_keyword("contains") {
                Compare::Contains
            } else if self.eat_keyword("has-line") {
                Compare::HasLine
            } else if self.rest().starts_with('=') {
                return Err(self.error_at(at, "compare with '==' (or '!=')"));
            } else {
                // `$(cmd) args`: runs what the command prints, in the shell.
                self.pos = start;
                return Ok(None);
            };
            self.skip_inline_ws();
            if !matches!(self.peek(), Some('"' | '\'')) {
                return Err(
                    self.error("expected a string to compare with, like `$(command) == \"value\"`")
                );
            }
            let value = self.string()?;
            if !self.at_check_end() {
                return Err(self.error("expected the end of the check after the value"));
            }
            return Ok(Some(CheckKind::Output {
                command,
                compare,
                value,
            }));
        }
        let kind = match self.word("").ok().as_deref() {
            Some("exists") => {
                self.skip_inline_ws();
                match self.peek() {
                    Some('"' | '\'') => Some(CheckKind::Exists(self.string()?)),
                    _ => None,
                }
            }
            Some("which") => {
                self.skip_inline_ws();
                match self.peek() {
                    Some('"' | '\'') => Some(CheckKind::Which(self.string()?)),
                    _ => self.word("").ok().map(CheckKind::Which),
                }
            }
            _ => None,
        };
        match kind {
            Some(kind) if self.at_check_end() => Ok(Some(kind)),
            _ => {
                self.pos = start;
                Ok(None)
            }
        }
    }

    /// At the end of a check: the line ends, or a `{` opens a block.
    fn at_check_end(&mut self) -> bool {
        if self.at_line_end() {
            return true;
        }
        let save = self.pos;
        self.bump();
        let block = self.src[save..].starts_with('{') && self.at_line_end();
        self.pos = save;
        block
    }

    /// `include "x" if <check>`
    fn optional_guard(&mut self) -> Result<Option<Guard>> {
        self.skip_inline_ws();
        let save = self.pos;
        match self.word("").ok().as_deref() {
            Some(keyword @ ("if" | "unless")) => {
                let start = self.pos;
                let (check, opens_block) = self.check(keyword)?;
                if opens_block {
                    return Err(self.error_at(start, "an include can't open a block"));
                }
                // `check` read the end of the line, which the caller reads again.
                self.pos = self.src[..self.pos].trim_end_matches(['\n', '\r']).len();
                Ok(Some(if keyword == "if" {
                    Guard::If(check)
                } else {
                    Guard::Unless(check)
                }))
            }
            _ => {
                self.pos = save;
                Ok(None)
            }
        }
    }

    /// `wait ["message"] [timeout <seconds>] until <check>`
    fn wait_step(&mut self) -> Result<WaitStep> {
        let mut wait = WaitStep {
            message: None,
            timeout: None,
            check: Check::shell(""),
        };
        loop {
            self.skip_inline_ws();
            if matches!(self.peek(), Some('"' | '\'')) && wait.message.is_none() {
                wait.message = Some(self.string()?);
                continue;
            }
            let at = self.pos;
            match self.word("'until', 'timeout' or a message")?.as_str() {
                "timeout" if wait.timeout.is_none() => {
                    let at = self.pos;
                    let seconds = self.word("a number of seconds")?;
                    wait.timeout = Some(seconds.parse().map_err(|_| {
                        self.error_at(at, format!("'{seconds}' isn't a number of seconds"))
                    })?);
                }
                "until" => break,
                other => {
                    return Err(self.error_at(
                        at,
                        format!("unexpected '{other}', expected `wait [\"message\"] [timeout <seconds>] until <check>`"),
                    ));
                }
            }
        }
        let at = self.pos;
        let (check, opens_block) = self.check("until")?;
        if opens_block {
            return Err(self.error_at(at, "`wait` doesn't open a block"));
        }
        wait.check = check;
        Ok(wait)
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
        let params = if template { self.params()? } else { None };
        // `task name "what it does"`: its description.
        self.skip_inline_ws();
        let header_desc = if matches!(self.peek(), Some('"' | '\'')) {
            if template {
                return Err(self.error("a template has no description, only tasks do"));
            }
            Some(self.string()?)
        } else {
            None
        };
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
            if header_desc.is_some() && body.desc.is_some() {
                return Err(self.error_at(
                    start,
                    format!("task '{name}' has a description in its header and a `desc`"),
                ));
            }
            let task = Task {
                name: name.clone(),
                desc: header_desc.or(body.desc),
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

    /// A template's named parameters, in parentheses right after its name:
    /// `(a, b, c="default")`. Without them, calls take any words.
    fn params(&mut self) -> Result<Option<Vec<Param>>> {
        if self.peek() != Some('(') {
            self.skip_inline_ws();
            let save = self.pos;
            let word = self.word("").ok();
            self.pos = save;
            if let Some(word) = word.filter(|w| w != "needs" && w != "lock") {
                return Err(self.error(format!(
                    "unexpected '{word}': a template's parameters go in parentheses, \
                     like `template name(a, b=\"default\") {{`"
                )));
            }
            return Ok(None);
        }
        self.bump();
        let mut params: Vec<Param> = Vec::new();
        loop {
            self.skip_inline_ws();
            if self.peek() == Some(')') {
                self.bump();
                return Ok(Some(params));
            }
            let start = self.pos;
            let name = self.word("a parameter name or ')'")?;
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
            self.skip_inline_ws();
            match self.peek() {
                Some(',') => self.bump(),
                Some(')') => {}
                _ => return Err(self.error("expected ',' or ')' after a parameter")),
            }
        }
    }

    /// `setting [name =] "value" {` with a `get` and a `set` line (or `{`
    /// blocks), and maybe `default` or `keep`.
    fn setting(&mut self, start: usize) -> Result<SettingStep> {
        self.skip_inline_ws();
        let mut value = match self.peek() {
            Some('"' | '\'') => self.string()?,
            _ => self.word("the value it should have")?,
        };
        self.skip_inline_ws();
        let mut name = None;
        if self.peek() == Some('=') {
            self.bump();
            self.skip_inline_ws();
            name = Some(std::mem::replace(
                &mut value,
                match self.peek() {
                    Some('"' | '\'') => self.string()?,
                    _ => self.word("the value it should have, after `=`")?,
                },
            ));
        }
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
                name,
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
        let mut template = false;
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
                .word("file content, 'from', 'url', 'mode', 'owner', 'encoding', 'sudo', 'new', 'keep' or 'template'")?
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
                "template" => template = true,
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
                    let sha256 = self.string()?;
                    // One from a variable is checked once it's filled in.
                    if !sha256.contains("{{") && !is_sha256(&sha256) {
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
        if template && !matches!(source, FileSource::Copy(_)) {
            return Err(self.error_at(
                origin.after_path,
                "`template` is for content copied `from` a file (inline content is always templated)",
            ));
        }
        if matches!(source, FileSource::Url { .. }) && encoding.is_some() {
            return Err(self.error_at(
                origin.after_path,
                "a downloaded file is written as it is, it has no encoding",
            ));
        }
        let raw = encoding.is_none()
            && !template
            && matches!(source, FileSource::Copy(_) | FileSource::Url { .. });
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
            template,
            sudo,
            new,
            keep,
            origin,
        })
    }

    /// `line "path" [sudo] [new] [keep] [base "path"] [match "start"] [before "start" | after "start"] "text"`
    fn line_step(&mut self) -> Result<LineStep> {
        let path = self.string()?;
        let step = LineStep {
            path,
            text: String::new(),
            place: None,
            base: None,
            matcher: None,
            new: false,
            sudo: false,
            keep: false,
        };
        let (step, block) = self.line_options(step, false)?;
        debug_assert!(!block);
        Ok(step)
    }

    /// `lines "path" [options] {` with a line of `[options] "text"` for each
    /// line step in that file: its options are each line's, which add to them.
    fn lines_block(&mut self, start: usize) -> Result<Vec<LineStep>> {
        let path = self.string()?;
        let header = LineStep {
            path,
            text: String::new(),
            place: None,
            base: None,
            matcher: None,
            new: false,
            sudo: false,
            keep: false,
        };
        let (header, _) = self.line_options(header, true)?;
        let mut steps = Vec::new();
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
            let (step, _) = self.line_options(header.clone(), false)?;
            steps.push(step);
        }
        if steps.is_empty() {
            return Err(self.error_at(start, "`lines` has no lines in it"));
        }
        Ok(steps)
    }

    /// A line step's options and then its text, to the end of the line. In a
    /// `lines` header (`header`), the options end with `{` instead, which
    /// returns true.
    fn line_options(&mut self, mut step: LineStep, header: bool) -> Result<(LineStep, bool)> {
        let mut own_place = false;
        loop {
            self.skip_inline_ws();
            if header && self.peek() == Some('{') {
                self.bump();
                self.end_of_line()?;
                return Ok((step, true));
            }
            if let Some('"' | '\'') = self.peek() {
                let start = self.pos;
                if header {
                    return Err(self.error_at(
                        start,
                        "the lines go inside `lines \"path\" {`, one on each line",
                    ));
                }
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
            let expected = if header {
                "'{', 'before', 'after', 'base', 'sudo', 'new' or 'keep'"
            } else {
                "the line, 'before', 'after', 'match', 'base', 'sudo', 'new' or 'keep'"
            };
            match self.word(expected)?.as_str() {
                "sudo" => step.sudo = true,
                "keep" => step.keep = true,
                "base" => step.base = Some(self.string()?),
                "new" => step.new = true,
                "match" if header => {
                    return Err(self.error_at(start, "'match' is for each line, not all of them"));
                }
                "match" => {
                    let prefix = self.string()?;
                    if prefix.trim().is_empty() {
                        return Err(self.error_at(start, "'match' needs the start of a line"));
                    }
                    step.matcher = Some(prefix);
                }
                word @ ("before" | "after") => {
                    let prefix = self.string()?;
                    // A line's own `before`/`after` replaces its header's.
                    if own_place {
                        return Err(self.error_at(start, "only one of 'before' or 'after'"));
                    }
                    own_place = true;
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
        Ok((step, false))
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

/// The files a path names: itself, or with `*` or `?` in its file name
/// (not its directories), the files in its directory that match, sorted.
fn glob(path: &Path) -> std::result::Result<Vec<PathBuf>, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dir = path.parent().unwrap_or(Path::new("."));
    if dir.to_string_lossy().contains(['*', '?']) {
        return Err(format!(
            "{}: only the file name can have `*` or `?`",
            path.display()
        ));
    }
    if !name.contains(['*', '?']) {
        return Ok(vec![path.to_path_buf()]);
    }
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("can't list {}: {e}", dir.display()))?;
    let pattern: Vec<char> = name.chars().collect();
    let mut found: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| !t.is_dir()))
        .filter(|e| {
            let name: Vec<char> = e.file_name().to_string_lossy().chars().collect();
            // Like the shell, `*` doesn't match a leading dot.
            name.first() != Some(&'.') && glob_matches(&pattern, &name)
        })
        .map(|e| e.path())
        .collect();
    if found.is_empty() {
        return Err(format!("no files match {}", path.display()));
    }
    found.sort();
    Ok(found)
}

/// Whether `name` matches `pattern`, where `*` is any run of characters and `?` one.
fn glob_matches(pattern: &[char], name: &[char]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some(('*', rest)) => (0..=name.len()).any(|i| glob_matches(rest, &name[i..])),
        Some(('?', rest)) => !name.is_empty() && glob_matches(rest, &name[1..]),
        Some((c, rest)) => name.first() == Some(c) && glob_matches(rest, &name[1..]),
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
            | "lines"
            | "setting"
            | "wait"
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
        Guard::If(check) => check_passes(check, scope, cwd),
        Guard::Unless(check) => Ok(!check_passes(check, scope, cwd)?),
        Guard::Ask(prompt) => crate::exec::ask(&scope.expand(prompt)?),
        // Only a task run knows; see `TaskRun::nested`.
        Guard::Changed => bail!("`changed` is only checked inside a task"),
    }
}

/// Whether a check passes, on the machine being configured, with relative
/// paths and commands from `cwd`.
pub fn check_passes(check: &Check, scope: &Scope, cwd: &Path) -> Result<bool> {
    let ctx = scope.ctx;
    match &check.kind {
        CheckKind::Shell => ctx.check(&scope.expand(&check.text)?, cwd),
        CheckKind::Exists(path) => {
            let path = PathBuf::from(ctx.target_path(&scope.expand(path)?));
            let path = if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            };
            crate::exec::target::exists(ctx, "", &path, false)
        }
        CheckKind::Which(name) => ctx.which(&scope.expand(name)?, cwd),
        CheckKind::Output {
            command,
            compare,
            value,
        } => {
            let output = ctx.stdout(&scope.expand(command)?, cwd)?;
            Ok(compare.matches(&output, &scope.expand(value)?))
        }
    }
}

/// 64 hexadecimal digits, either case.
pub(crate) fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.chars().all(|c| c.is_ascii_hexdigit())
}

/// Load a YAML (or JSON) file's values as variables, nested keys joined with
/// dots, under `prefix.` when given. They're used as written, not templated.
fn load_vars_file(path: &Path, prefix: Option<&str>, file: &mut TaskFile) -> Result<()> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read vars file {}", path.display()))?;
    let value: serde_json::Value = serde_norway::from_str(&contents)
        .with_context(|| format!("{} isn't valid YAML", path.display()))?;
    if !value.is_object() {
        bail!("{} must be a mapping of names to values", path.display());
    }
    let mut values = std::collections::BTreeMap::new();
    crate::secrets::flatten(prefix.unwrap_or(""), &value, &mut values);
    for (name, value) in values {
        let valid = !name.is_empty()
            && name.chars().all(crate::vars::is_var_char)
            && !name.starts_with(crate::secrets::PREFIX);
        if !valid {
            bail!("{}: invalid variable name {name:?}", path.display());
        }
        file.vars.literal(name, value);
    }
    Ok(())
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
    fn descriptions_go_in_the_header() {
        let file =
            parse("task t \"Does t\" needs u {\n run true\n}\ntask u {\n desc \"Does u\"\n}\n");
        assert_eq!(file.tasks["t"].desc.as_deref(), Some("Does t"));
        assert_eq!(file.tasks["t"].needs, ["u"]);
        assert_eq!(file.tasks["u"].desc.as_deref(), Some("Does u"));
        let file = parse("task p {\n run true\n}\nsubtask s of p 'After p' {\n run true\n}\n");
        assert_eq!(file.tasks["s"].desc.as_deref(), Some("After p"));
        assert!(parse_err("task t \"a\" {\n desc \"b\"\n}\n").contains("and a `desc`"));
        assert!(parse_err("template x \"a\" {\n run true\n}\n").contains("no description"));
    }

    #[test]
    fn call_blocks_call_once_per_line() {
        let file = parse(
            "template d {\n run true\n}\ntask t {\n d one {  # why\n  a 1\n\n  # skipped\n  b \"2 3\"\n }\n d {\n  x\n }\n}\n",
        );
        let calls: Vec<&str> = file.tasks["t"]
            .body
            .steps
            .iter()
            .map(|s| match s {
                Step::Call { args, .. } => args.as_str(),
                _ => panic!("{s:?}"),
            })
            .collect();
        assert_eq!(calls, ["one a 1", "one b \"2 3\"", "x"]);
        assert!(
            parse_err("template d {\n run true\n}\ntask t {\n d {\n }\n}\n")
                .contains("has no lines")
        );
        assert!(
            parse_err("template d {\n run true\n}\ntask t {\n d {\n  a\n")
                .contains("missing closing")
        );
    }

    #[test]
    fn lines_blocks_share_a_file_and_options() {
        let file = parse(
            "task t {\n lines \"/etc/x\" sudo after \"[a]\" {\n  match \"#A\" \"A\"\n  keep before \"[b]\" \"B\"  # why\n }\n}\n",
        );
        let lines: Vec<&LineStep> = file.tasks["t"]
            .body
            .steps
            .iter()
            .map(|s| match s {
                Step::Line(l) => l.as_ref(),
                _ => panic!("{s:?}"),
            })
            .collect();
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.path == "/etc/x" && l.sudo));
        assert_eq!(lines[0].matcher.as_deref(), Some("#A"));
        assert!(matches!(&lines[0].place, Some(LinePlace::After(a)) if a == "[a]"));
        assert!(!lines[0].keep && lines[1].keep);
        assert!(matches!(&lines[1].place, Some(LinePlace::Before(b)) if b == "[b]"));
        assert!(parse_err("task t {\n lines \"x\" \"A\"\n}\n").contains("go inside"));
        assert!(parse_err("task t {\n lines \"x\" match \"a\" {\n}\n").contains("for each line"));
        assert!(parse_err("task t {\n lines \"x\" {\n }\n}\n").contains("no lines"));
    }

    #[test]
    fn settings_can_have_a_name() {
        let source = "task t {\n setting shields-up = true {\n  get a\n  set b\n }\n setting \"x y\" {\n  get a\n  set b\n }\n}\n";
        let file = parse(source);
        let settings: Vec<&SettingStep> = file.tasks["t"]
            .body
            .steps
            .iter()
            .map(|s| match s {
                Step::Setting(s) => s.as_ref(),
                _ => panic!("{s:?}"),
            })
            .collect();
        assert_eq!(settings[0].name.as_deref(), Some("shields-up"));
        assert_eq!(settings[0].value, "true");
        assert_eq!(
            (settings[1].name.as_deref(), settings[1].value.as_str()),
            (None, "x y")
        );
        assert_eq!(
            file.tasks["t"].body.steps[0].describe(),
            "setting shields-up"
        );
    }

    #[test]
    fn secrets_take_a_glob() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["b.yaml", "a.yaml", ".hidden.yaml", "c.json"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        std::fs::create_dir(dir.path().join("d.yaml")).unwrap();
        let file = parse_str("secrets \"*.yaml\"\n", dir.path()).unwrap();
        let names: Vec<String> = file
            .declarations
            .iter()
            .map(|d| d.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a.yaml", "b.yaml"]);
        let err = parse_str("secrets \"*.toml\"\n", dir.path()).err().unwrap();
        assert!(format!("{err:#}").contains("no files match"), "{err:#}");
        let err = parse_str("secrets \"*/x.yaml\"\n", dir.path())
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("only the file name"), "{err:#}");
        assert!(glob_matches(&['a', '?', '*'], &['a', 'b']));
        assert!(!glob_matches(&['a', '?'], &['a']));
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
        assert!(matches!(&block.guards[0], Guard::Unless(c) if c.text == "test -e x"));
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
        assert!(matches!(&block.guards[0], Guard::Unless(c) if c.text == "test -e x"));
        assert!(
            matches!(&block.steps[0], Step::Call { args, .. } if args == "a \"b # c\""),
            "{:?}",
            block.steps
        );
        // A `{` inside quotes doesn't open a block.
        let guards = &file.tasks["t"].body.guards;
        assert!(matches!(&guards[..], [Guard::If(c)] if c.text == "grep -q 'a { #b' f"));
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
    fn vars_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("v.yaml"),
            "# versions\nuv:\n  version: 0.13.9\n  sha256: \"abc\"\nimages: [a, b]\nnone:\n",
        )
        .unwrap();
        let path = dir.path().join("main.task");
        let parse = |source: &str| {
            std::fs::write(&path, source).unwrap();
            parse_file(&path)
        };
        let file = parse("vars \"v.yaml\" as versions\n").unwrap();
        let scope = Scope::root(&file.vars, &file.ctx);
        assert_eq!(
            scope
                .expand("{{ versions.uv.version }}|{{ versions.uv.sha256 }}|{{ versions.images.1 }}|{{ versions.none }}")
                .unwrap(),
            "0.13.9|abc|b|"
        );
        // Not exported, unlike `env`.
        assert!(file.ctx.exports.is_empty());
        let file = parse("vars \"v.yaml\"\n").unwrap();
        let scope = Scope::root(&file.vars, &file.ctx);
        assert_eq!(scope.expand("{{ uv.version }}").unwrap(), "0.13.9");
        // Values are data: `{{` in one isn't expanded.
        std::fs::write(dir.path().join("v.yaml"), "a: \"{{ b }}\"\n").unwrap();
        let file = parse("vars \"v.yaml\"\n").unwrap();
        let scope = Scope::root(&file.vars, &file.ctx);
        assert_eq!(scope.expand("{{ a }}").unwrap(), "{{ b }}");

        for (yaml, source, error) in [
            ("a: 1\n", "vars \"v.yaml\" as x.y\n", "invalid name"),
            ("a: 1\n", "vars \"v.yaml\" as\n", "a name for the variables"),
            (
                "a: 1\n",
                "vars \"v.yaml\" with x\n",
                "expected the end of the line",
            ),
            ("- a\n", "vars \"v.yaml\"\n", "must be a mapping"),
            ("a: [\n", "vars \"v.yaml\"\n", "isn't valid YAML"),
            ("\"a b\": 1\n", "vars \"v.yaml\"\n", "invalid variable name"),
            (
                "secrets: {a: 1}\n",
                "vars \"v.yaml\"\n",
                "invalid variable name",
            ),
            (
                "a: 1\n",
                "vars \"missing.yaml\"\n",
                "failed to read vars file",
            ),
        ] {
            std::fs::write(dir.path().join("v.yaml"), yaml).unwrap();
            let err = format!("{:#}", parse(source).err().unwrap());
            assert!(err.contains(error), "{source}: {err}");
        }
    }

    #[test]
    fn template_is_only_for_from_files() {
        let file = parse_str(
            "task t {\n  file \"/x\" template from \"/y\"\n  directory \"/d\" template from \"/e\"\n}\n",
            Path::new("/"),
        )
        .unwrap();
        let steps: Vec<_> = file.tasks["t"]
            .body
            .steps
            .iter()
            .map(|s| match s {
                Step::File(f) => (f.template, f.raw),
                _ => panic!(),
            })
            .collect();
        assert_eq!(steps, [(true, false), (true, false)]);
        for source in [
            "task t {\n  file \"/x\" template \"inline\"\n}\n",
            "task t {\n  file \"/x\" template url \"https://x\" sha256 \"0000000000000000000000000000000000000000000000000000000000000000\"\n}\n",
        ] {
            let err = format!("{:#}", parse_str(source, Path::new("/")).err().unwrap());
            assert!(err.contains("`template` is for content copied"), "{err}");
        }
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

        std::fs::write(
            dir.path().join("main.task"),
            "include \"sub/a.task\" if exists \"sub/b.task\" # why\n\
             include \"sub/b.task\" unless $(echo x) == \"x\"\n",
        )
        .unwrap();
        let file = parse_file(&dir.path().join("main.task")).unwrap();
        assert!(file.tasks.contains_key("a"));
        assert!(!file.tasks.contains_key("b"));
    }

    /// The guards of the first block in task `t`.
    fn block_checks(source: &str) -> Vec<Check> {
        let file = parse(source);
        let mut checks = Vec::new();
        for step in &file.tasks["t"].body.steps {
            if let Step::Block(block) = step {
                for guard in &block.guards {
                    if let Guard::If(check) | Guard::Unless(check) = guard {
                        checks.push(check.clone());
                    }
                }
            }
        }
        checks
    }

    #[test]
    fn parses_builtin_checks() {
        let checks = block_checks(
            r#"task t {
                if exists "~/x" {
                }
                unless which tool { # why
                }
                if which "two words" {
                }
                if $(echo "a)" | wc -l) == "1" {
                }
                unless $(a \
                    b) has-line 'x' {
                }
                if $(x) != "" {
                }
                if exists x {
                }
                if exists "x" y {
                }
                if which a b {
                }
                if $(cmd) {
                }
                if $(cmd) args {
                }
            }"#,
        );
        let kinds: Vec<&CheckKind> = checks.iter().map(|c| &c.kind).collect();
        assert_eq!(kinds[0], &CheckKind::Exists("~/x".into()));
        assert_eq!(kinds[1], &CheckKind::Which("tool".into()));
        assert_eq!(kinds[2], &CheckKind::Which("two words".into()));
        let output = |command: &str, compare, value: &str| CheckKind::Output {
            command: command.into(),
            compare,
            value: value.into(),
        };
        assert_eq!(
            kinds[3],
            &output("echo \"a)\" | wc -l", Compare::Equals, "1")
        );
        let CheckKind::Output { command, .. } = kinds[4] else {
            panic!("{:?}", kinds[4]);
        };
        assert_eq!(command.split_whitespace().collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(checks[4].text, "$(a b) has-line 'x'");
        assert_eq!(kinds[5], &output("x", Compare::NotEquals, ""));
        // Anything else is shell.
        for (check, text) in checks[6..].iter().zip([
            "exists x",
            "exists \"x\" y",
            "which a b",
            "$(cmd)",
            "$(cmd) args",
        ]) {
            assert_eq!(check.kind, CheckKind::Shell, "{text}");
            assert_eq!(check.text, text);
        }
        assert_eq!(kinds.len(), 11);
    }

    #[test]
    fn builtin_check_errors() {
        let err = parse_err("task t {\n if $(x) = \"y\"\n}\n");
        assert!(err.contains("2:10: compare with '=='"), "{err}");
        let err = parse_err("task t {\n if $(x) == y\n}\n");
        assert!(err.contains("expected a string to compare with"), "{err}");
        let err = parse_err("task t {\n if $(x) == \"y\" z\n}\n");
        assert!(err.contains("expected the end of the check"), "{err}");
        let err = parse_err("task t {\n if $(x == \"y\"\n}\n");
        assert!(err.contains("missing ')'"), "{err}");
    }

    #[test]
    fn parses_waits() {
        let file = parse(
            "task t {\n wait \"for {{ x }}\" timeout 30 until exists \"/run/a\"\n wait until test -e b\n}\n",
        );
        let steps = &file.tasks["t"].body.steps;
        let [Step::Wait(a), Step::Wait(b)] = &steps[..] else {
            panic!("{steps:?}");
        };
        assert_eq!(a.message.as_deref(), Some("for {{ x }}"));
        assert_eq!(a.timeout, Some(30));
        assert_eq!(a.check.kind, CheckKind::Exists("/run/a".into()));
        assert_eq!(a.label(), "wait: for {{ x }}");
        assert_eq!(b.check.kind, CheckKind::Shell);
        assert_eq!(b.label(), "wait until test -e b");

        let err = parse_err("task t {\n wait \"x\"\n}\n");
        assert!(err.contains("expected 'until'"), "{err}");
        let err = parse_err("task t {\n wait timeout soon until true\n}\n");
        assert!(err.contains("isn't a number of seconds"), "{err}");
        let err = parse_err("task t {\n wait until true {\n }\n}\n");
        assert!(err.contains("doesn't open a block"), "{err}");
        let err = parse_err("task t {\n wait until\n}\n");
        assert!(err.contains("'until' needs a check"), "{err}");
        let err = parse_err("template wait {\n run true\n}\n");
        assert!(err.contains("'wait' is a keyword"), "{err}");
    }

    #[test]
    fn workdir_in_templates_and_blocks() {
        let file = parse(
            "template t {\n workdir \"a\"\n run true\n}\ntask x {\n if true {\n  workdir \"{{ tmp }}\"\n }\n}\n",
        );
        assert_eq!(file.templates["t"].body.workdir.as_deref(), Some("a"));
        let Step::Block(block) = &file.tasks["x"].body.steps[0] else {
            panic!();
        };
        assert_eq!(block.workdir.as_deref(), Some("{{ tmp }}"));
        let err = parse_err("task x {\n workdir \"a\"\n workdir \"b\"\n}\n");
        assert!(err.contains("'workdir' is given twice"), "{err}");
    }
}
