//! Variables and `{{ name }}` templating.

use anyhow::{Result, anyhow, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// How deep variables may reference other variables before we assume a cycle.
const MAX_DEPTH: usize = 32;

#[derive(Debug)]
pub enum Var {
    /// Used as is, e.g. values loaded from an env file.
    Literal(String),
    /// Templated each time it is used.
    Template(String),
    /// Output of a shell command, run the first time the variable is used.
    Shell(String, OnceLock<std::result::Result<String, String>>),
    /// The answer to a question, asked the first time the variable is used.
    Prompt(Prompt, OnceLock<std::result::Result<String, String>>),
}

#[derive(Debug, Clone)]
pub enum Prompt {
    /// Yes or no, the answer is `true` or `false`.
    Confirm(String),
    /// One of a list of options.
    Choose(String, Vec<String>),
    /// Free text, with an optional default.
    Input(String, Option<String>),
}

#[derive(Debug, Default)]
pub struct Vars(HashMap<String, Var>);

impl Vars {
    pub fn set(&mut self, name: impl Into<String>, var: Var) {
        self.0.insert(name.into(), var);
    }

    pub fn literal(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.set(name, Var::Literal(value.into()));
    }

    pub fn get(&self, name: &str) -> Option<&Var> {
        self.0.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    /// Variables for a template call: `args` is the whole argument string,
    /// `1`, `2`, ... are its whitespace separated words.
    pub fn for_args(args: &str) -> Self {
        let mut vars = Vars::default();
        vars.literal("args", args);
        for (i, word) in args.split_whitespace().enumerate() {
            vars.literal((i + 1).to_string(), word);
        }
        vars
    }
}

/// How commands are run: the shell and the environment shared by all tasks.
#[derive(Debug, Clone)]
pub struct Ctx {
    /// Shell argv, the script is appended as the last argument.
    pub shell: Vec<String>,
    /// Directory of the main task file.
    pub dir: PathBuf,
    /// Extra environment variables for commands (from `env` files).
    pub exports: Vec<(String, String)>,
}

impl Ctx {
    pub fn new(dir: PathBuf) -> Self {
        Ctx {
            shell: default_shell(),
            dir,
            exports: Vec::new(),
        }
    }

    pub fn command(&self, script: &str, cwd: &Path) -> Command {
        let mut command = Command::new(&self.shell[0]);
        command
            .args(&self.shell[1..])
            .arg(script)
            .current_dir(cwd)
            .envs(self.exports.iter().map(|(k, v)| (k, v)));
        command
    }

    /// Run a check and report whether it succeeded. Output is discarded.
    pub fn check(&self, script: &str, cwd: &Path) -> Result<bool> {
        let status = self
            .command(script, cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| anyhow!("failed to start shell {:?}: {e}", self.shell[0]))?;
        Ok(status.success())
    }

    /// Run a command and return its trimmed stdout.
    pub fn capture(&self, script: &str, cwd: &Path) -> std::result::Result<String, String> {
        let output = self
            .command(script, cwd)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .output()
            .map_err(|e| format!("failed to start shell {:?}: {e}", self.shell[0]))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "`{script}` failed ({}): {}",
                output.status,
                stderr.trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

pub fn default_shell() -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/C".into()]
    } else {
        vec!["sh".into(), "-ec".into()]
    }
}

pub fn home_dir() -> Option<String> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
}

/// Expand a leading `~/` to the home directory.
pub fn expand_tilde(path: &str) -> String {
    match (path.strip_prefix("~/"), home_dir()) {
        (Some(rest), Some(home)) => format!("{home}/{rest}"),
        _ if path == "~" => home_dir().unwrap_or_else(|| path.to_string()),
        _ => path.to_string(),
    }
}

/// A chain of variable layers, innermost first.
#[derive(Clone, Copy)]
pub struct Scope<'a> {
    pub vars: &'a Vars,
    pub parent: Option<&'a Scope<'a>>,
    pub ctx: &'a Ctx,
}

impl<'a> Scope<'a> {
    pub fn root(vars: &'a Vars, ctx: &'a Ctx) -> Self {
        Scope {
            vars,
            parent: None,
            ctx,
        }
    }

    pub fn child(&'a self, vars: &'a Vars) -> Scope<'a> {
        Scope {
            vars,
            parent: Some(self),
            ctx: self.ctx,
        }
    }

    /// Replace every `{{ name }}` in `text`. `\{{` produces a literal `{{`.
    pub fn expand(&self, text: &str) -> Result<String> {
        self.expand_depth(text, 0)
    }

    fn expand_depth(&self, text: &str, depth: usize) -> Result<String> {
        if depth > MAX_DEPTH {
            bail!("variables nest too deeply, is a variable defined in terms of itself?");
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("{{") {
            if rest[..start].ends_with('\\') {
                out.push_str(&rest[..start - 1]);
                out.push_str("{{");
                rest = &rest[start + 2..];
                continue;
            }
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let end = after
                .find("}}")
                .ok_or_else(|| anyhow!("unclosed '{{{{' in {text:?}"))?;
            let name = after[..end].trim();
            if name.is_empty() || !name.chars().all(is_var_char) {
                bail!("invalid variable name {name:?} in {text:?}");
            }
            out.push_str(&self.lookup(name, depth)?);
            rest = &after[end + 2..];
        }
        out.push_str(rest);
        Ok(out)
    }

    fn lookup(&self, name: &str, depth: usize) -> Result<String> {
        let mut scope = Some(self);
        while let Some(current) = scope {
            if let Some(var) = current.vars.get(name) {
                return current.resolve(name, var, depth);
            }
            scope = current.parent;
        }
        let builtin = match name {
            "pwd" => std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string()),
            "home" => home_dir(),
            "user" => std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .ok(),
            "dir" => Some(self.ctx.dir.display().to_string()),
            _ => None,
        };
        if let Some(value) = builtin.or_else(|| std::env::var(name).ok()) {
            return Ok(value);
        }
        // Missing positional template arguments are empty, like in a shell.
        if name.chars().all(|c| c.is_ascii_digit()) {
            return Ok(String::new());
        }
        bail!("unknown variable '{name}'")
    }

    fn resolve(&self, name: &str, var: &Var, depth: usize) -> Result<String> {
        match var {
            Var::Literal(value) => Ok(value.clone()),
            Var::Template(template) => self.expand_depth(template, depth + 1),
            Var::Shell(command, cache) => cache
                .get_or_init(|| {
                    let command = self
                        .expand_depth(command, depth + 1)
                        .map_err(|e| e.to_string())?;
                    self.ctx.capture(&command, &self.ctx.dir)
                })
                .clone()
                .map_err(|e| anyhow!("variable '{name}': {e}")),
            Var::Prompt(prompt, cache) => cache
                .get_or_init(|| {
                    let prompt = self
                        .expand_prompt(prompt, depth)
                        .map_err(|e| e.to_string())?;
                    crate::ui::ui().prompt(&prompt).map_err(|e| e.to_string())
                })
                .clone()
                .map_err(|e| anyhow!("variable '{name}': {e}")),
        }
    }

    fn expand_prompt(&self, prompt: &Prompt, depth: usize) -> Result<Prompt> {
        let expand = |text: &str| self.expand_depth(text, depth + 1);
        Ok(match prompt {
            Prompt::Confirm(question) => Prompt::Confirm(expand(question)?),
            Prompt::Choose(question, options) => Prompt::Choose(
                expand(question)?,
                options.iter().map(|o| expand(o)).collect::<Result<_>>()?,
            ),
            Prompt::Input(question, default) => Prompt::Input(
                expand(question)?,
                default.as_deref().map(expand).transpose()?,
            ),
        })
    }
}

pub fn is_var_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        Ctx::new(PathBuf::from("."))
    }

    #[test]
    fn expands_with_and_without_spaces() {
        let ctx = ctx();
        let mut vars = Vars::default();
        vars.literal("name", "pod");
        let scope = Scope::root(&vars, &ctx);
        assert_eq!(
            scope.expand("a {{name}} {{ name }} b").unwrap(),
            "a pod pod b"
        );
    }

    #[test]
    fn inner_scopes_shadow_outer() {
        let ctx = ctx();
        let mut outer = Vars::default();
        outer.literal("x", "outer");
        outer.set("y", Var::Template("y={{ x }}".into()));
        let mut inner = Vars::default();
        inner.literal("x", "inner");
        let root = Scope::root(&outer, &ctx);
        let scope = root.child(&inner);
        assert_eq!(scope.expand("{{ x }}").unwrap(), "inner");
        // Variables resolve in the scope that defines them.
        assert_eq!(scope.expand("{{ y }}").unwrap(), "y=outer");
    }

    #[test]
    fn escapes_and_errors() {
        let ctx = ctx();
        let vars = Vars::default();
        let scope = Scope::root(&vars, &ctx);
        assert_eq!(scope.expand(r"\{{ x }}").unwrap(), "{{ x }}");
        assert!(scope.expand("{{ nope_not_defined }}").is_err());
        assert!(scope.expand("{{ x").is_err());
        assert_eq!(scope.expand("[{{ 3 }}]").unwrap(), "[]");
    }

    #[test]
    fn detects_cycles() {
        let ctx = ctx();
        let mut vars = Vars::default();
        vars.set("a", Var::Template("{{ b }}".into()));
        vars.set("b", Var::Template("{{ a }}".into()));
        assert!(Scope::root(&vars, &ctx).expand("{{ a }}").is_err());
    }

    #[test]
    fn template_args() {
        let ctx = ctx();
        let vars = Vars::for_args("one  two");
        let scope = Scope::root(&vars, &ctx);
        assert_eq!(
            scope.expand("{{ args }}|{{ 1 }}|{{ 2 }}").unwrap(),
            "one  two|one|two"
        );
    }

    #[cfg(unix)]
    #[test]
    fn shell_vars_run_once() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path().to_path_buf());
        let mut vars = Vars::default();
        vars.set(
            "n",
            Var::Shell("echo x >> count; wc -l < count".into(), OnceLock::new()),
        );
        let scope = Scope::root(&vars, &ctx);
        assert_eq!(
            scope.expand("{{ n }} {{ n }}").unwrap().replace(' ', ""),
            "11"
        );
    }
}
