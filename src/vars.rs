//! Variables and `{{ name }}` templating.

use anyhow::{Result, anyhow, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, OnceLock};

/// How deep variables may reference other variables before we assume a cycle.
const MAX_DEPTH: usize = 32;

/// The value of a lazy variable once it was worked out, shared by copies of
/// the variable that would give the same value.
pub type Cache = Arc<OnceLock<std::result::Result<String, String>>>;

#[derive(Debug)]
pub enum Var {
    /// Used as is, e.g. values loaded from an env file.
    Literal(String),
    /// Templated each time it is used.
    Template(String),
    /// Output of a shell command, run the first time the variable is used.
    Shell(String, Cache),
    /// The answer to a question, asked the first time the variable is used.
    Prompt(Prompt, Cache),
}

impl Var {
    /// A copy for a new template call. Lazy values that use `{{ }}` may
    /// depend on the call's arguments, so they get worked out again; the
    /// others keep the value they have.
    fn for_call(&self) -> Var {
        let fresh = |cache: &Cache, templated: bool| {
            if templated {
                Cache::default()
            } else {
                Arc::clone(cache)
            }
        };
        match self {
            Var::Literal(value) => Var::Literal(value.clone()),
            Var::Template(text) => Var::Template(text.clone()),
            Var::Shell(command, cache) => {
                Var::Shell(command.clone(), fresh(cache, command.contains("{{")))
            }
            Var::Prompt(prompt, cache) => {
                Var::Prompt(prompt.clone(), fresh(cache, prompt.is_templated()))
            }
        }
    }
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

impl Prompt {
    fn is_templated(&self) -> bool {
        let templated = |text: &String| text.contains("{{");
        match self {
            Prompt::Confirm(question) => templated(question),
            Prompt::Choose(question, options) => {
                templated(question) || options.iter().any(templated)
            }
            Prompt::Input(question, default) => {
                templated(question) || default.iter().any(templated)
            }
        }
    }
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

    /// A template's variables for one call of it, see [`Var::for_call`].
    pub fn for_call(&self) -> Self {
        Vars(
            self.0
                .iter()
                .map(|(name, var)| (name.clone(), var.for_call()))
                .collect(),
        )
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
    /// `{{ secrets.name }}` values, decrypted when first used.
    pub secrets: crate::secrets::Secrets,
    /// Run on this machine over SSH instead of locally.
    pub remote: Option<Arc<crate::remote::Remote>>,
    /// Where `--undo`'s journal lives instead of the usual places, for tests.
    pub journal: Option<PathBuf>,
}

/// A command ready to start, with what to write to its stdin.
pub struct Spawn {
    pub command: Command,
    input: Option<Vec<u8>>,
}

impl Spawn {
    /// Start it in a process group of its own (see `crate::tty`), writing
    /// its input (if any) in the background. Without input, stdin is closed.
    pub fn spawn(mut self) -> std::io::Result<(Child, crate::tty::Job)> {
        let stdin = if self.input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        };
        let (mut child, job) = crate::tty::Job::spawn(self.command.stdin(stdin))?;
        if let (Some(input), Some(stdin)) = (self.input, child.stdin.take()) {
            std::thread::spawn(move || crate::remote::write_all(stdin, &input));
        }
        Ok((child, job))
    }
}

impl Ctx {
    pub fn new(dir: PathBuf) -> Self {
        Ctx {
            shell: default_shell(),
            exports: Vec::new(),
            secrets: Default::default(),
            remote: None,
            // Tests never touch the real journal.
            journal: cfg!(test).then(|| dir.join(".taskpod-journal")),
            dir,
        }
    }

    /// A shell command running `script` in `cwd`, on the remote machine if there is one.
    pub fn command(&self, script: &str, cwd: &Path) -> Spawn {
        match &self.remote {
            Some(remote) => {
                let (command, input) = remote.command(&self.remote_script(script, cwd, false), &[]);
                Spawn {
                    command,
                    input: Some(input),
                }
            }
            None => Spawn {
                command: self.local_command(script, cwd),
                input: None,
            },
        }
    }

    /// A command running `script` with the terminal, for interactive tasks.
    pub fn interactive_command(&self, script: &str, cwd: &Path) -> Result<Command> {
        match &self.remote {
            Some(remote) => remote.interactive_command(&self.remote_script(script, cwd, true)),
            None => Ok(self.local_command(script, cwd)),
        }
    }

    fn local_command(&self, script: &str, cwd: &Path) -> Command {
        let mut command = Command::new(&self.shell[0]);
        command
            .args(&self.shell[1..])
            .arg(script)
            .current_dir(cwd)
            .envs(self.exports.iter().map(|(k, v)| (k, v)));
        command
    }

    /// A remote POSIX script that runs `script` with the configured shell.
    fn remote_script(&self, script: &str, cwd: &Path, tty: bool) -> String {
        use crate::remote::quote;
        let mut out = String::new();
        if !cwd.as_os_str().is_empty() {
            out.push_str(&format!(
                "cd -- {} || exit 1\n",
                quote(&cwd.to_string_lossy())
            ));
        }
        for (key, value) in &self.exports {
            out.push_str(&format!("{key}={}; export {key}\n", quote(value)));
        }
        let argv: Vec<String> = self
            .shell
            .iter()
            .map(String::as_str)
            .chain([script])
            .map(quote)
            .collect();
        out.push_str(&argv.join(" "));
        // The script itself came over stdin; don't let commands read the rest of it.
        if !tty {
            out.push_str(" </dev/null");
        }
        out.push('\n');
        out
    }

    /// Where commands run when nothing else is said: `local` here, the home
    /// directory on a remote machine.
    pub fn default_cwd(&self, local: &Path) -> PathBuf {
        match &self.remote {
            Some(remote) => PathBuf::from(&remote.home),
            None => local.to_path_buf(),
        }
    }

    /// The home directory of the machine being configured.
    pub fn home(&self) -> Option<String> {
        match &self.remote {
            Some(remote) => Some(remote.home.clone()),
            None => home_dir(),
        }
    }

    /// The user on the machine being configured.
    pub fn user(&self) -> Option<String> {
        match &self.remote {
            Some(remote) => Some(remote.user.clone()),
            None => std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .ok(),
        }
    }

    /// Expand a leading `~/` in a path on the machine being configured.
    pub fn target_path(&self, path: &str) -> String {
        match &self.remote {
            Some(remote) => match path.strip_prefix("~/") {
                Some(rest) => format!("{}/{rest}", remote.home),
                None if path == "~" => remote.home.clone(),
                None => path.to_string(),
            },
            None => expand_tilde(path),
        }
    }

    /// What a command failing to start is reported as.
    pub fn program(&self) -> &str {
        if self.remote.is_some() {
            "ssh"
        } else {
            &self.shell[0]
        }
    }

    /// Make a new empty directory on the machine being configured, for `{{ tmp }}`.
    pub fn make_temp_dir(&self) -> Result<String> {
        if self.remote.is_some() {
            let dir = self
                .capture("mktemp -d", &self.default_cwd(&self.dir))
                .map_err(|e| anyhow!("failed to make a directory for {{{{ tmp }}}}: {e}"))?;
            return Ok(dir);
        }
        static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let base = std::env::temp_dir();
        for _ in 0..100 {
            let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos());
            let dir = base.join(format!("taskpod.{}.{n}.{nanos:x}", std::process::id()));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            match builder.create(&dir) {
                Ok(()) => return Ok(dir.display().to_string()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => bail!(
                    "failed to make a directory for {{{{ tmp }}}} in {}: {e}",
                    base.display()
                ),
            }
        }
        bail!(
            "failed to make a directory for {{{{ tmp }}}} in {}",
            base.display()
        )
    }

    /// Remove a directory made by [`Ctx::make_temp_dir`], with everything in it.
    pub fn remove_temp_dir(&self, dir: &str) -> Result<()> {
        if self.remote.is_some() {
            let script = format!("rm -rf -- {}", crate::remote::quote(dir));
            self.capture(&script, &self.default_cwd(&self.dir))
                .map_err(|e| anyhow!(e))?;
            return Ok(());
        }
        std::fs::remove_dir_all(dir).map_err(|e| anyhow!("failed to remove {dir}: {e}"))
    }

    /// Run a check and report whether it succeeded. Output is discarded.
    pub fn check(&self, script: &str, cwd: &Path) -> Result<bool> {
        let mut spawn = self.command(script, cwd);
        spawn.command.stdout(Stdio::null()).stderr(Stdio::null());
        let (mut child, job) = spawn
            .spawn()
            .map_err(|e| anyhow!("failed to start {:?}: {e}", self.program()))?;
        let status = child.wait()?;
        if let Some(process) = job.finish() {
            bail!(
                "`{script}`: {}, and checks can't ask",
                crate::tty::wanted(&process)
            );
        }
        Ok(status.success())
    }

    /// Run a command and return its trimmed stdout.
    pub fn capture(&self, script: &str, cwd: &Path) -> std::result::Result<String, String> {
        let mut spawn = self.command(script, cwd);
        spawn.command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let (child, job) = spawn
            .spawn()
            .map_err(|e| format!("failed to start {:?}: {e}", self.program()))?;
        let output = child
            .wait_with_output()
            .map_err(|e| format!("`{script}` failed: {e}"))?;
        if let Some(process) = job.finish() {
            return Err(format!(
                "`{script}`: {}, and checks can't ask",
                crate::tty::wanted(&process)
            ));
        }
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

    /// Whether a variable of this name is defined in the task file (not a
    /// built-in or an environment variable).
    pub fn defines(&self, name: &str) -> bool {
        let mut scope = Some(self);
        while let Some(current) = scope {
            if current.vars.contains(name) {
                return true;
            }
            scope = current.parent;
        }
        false
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
        if let Some(secret) = name.strip_prefix(crate::secrets::PREFIX) {
            return self.ctx.secrets.get(secret).map_err(|e| anyhow!(e));
        }
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
            "home" => self.ctx.home(),
            "user" => self.ctx.user(),
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
                    self.ctx
                        .capture(&command, &self.ctx.default_cwd(&self.ctx.dir))
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

/// Whether `text` uses `{{ name }}` (not escaped as `\{{`).
pub fn mentions(text: &str, name: &str) -> bool {
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let escaped = rest[..start].ends_with('\\');
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return false;
        };
        if !escaped && after[..end].trim() == name {
            return true;
        }
        rest = &after[end + 2..];
    }
    false
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
    fn template_calls_work_out_their_own_values() {
        let ctx = ctx();
        let mut template = Vars::default();
        template.set("up", Var::Shell("echo {{ 1 }}".into(), Cache::default()));
        template.set("fixed", Var::Shell("echo same".into(), Cache::default()));
        let mut seen = Vec::new();
        for arg in ["a", "b"] {
            let args = Vars::for_args(arg);
            let root = Scope::root(&args, &ctx);
            let call = template.for_call();
            seen.push(root.child(&call).expand("{{ up }} {{ fixed }}").unwrap());
        }
        assert_eq!(seen, ["a same", "b same"]);
        // A value without `{{ }}` is worked out once, for every call.
        let Some(Var::Shell(_, cache)) = template.get("fixed") else {
            panic!()
        };
        assert!(cache.get().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn shell_vars_run_once() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path().to_path_buf());
        let mut vars = Vars::default();
        vars.set(
            "n",
            Var::Shell("echo x >> count; wc -l < count".into(), Cache::default()),
        );
        let scope = Scope::root(&vars, &ctx);
        assert_eq!(
            scope.expand("{{ n }} {{ n }}").unwrap().replace(' ', ""),
            "11"
        );
    }
}
