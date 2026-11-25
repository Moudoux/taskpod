//! `--sources`: every file a task file reads, as a tree by the task file that
//! names it: includes, env, secrets and age-key files, then each task's
//! `file` and `directory` sources and the secrets it uses.
//!
//! Only the file is read, like `--graph`: `{{ }}` paths are shown as
//! written, and secrets by name, without decrypting anything.

use crate::ast::{Block, DeclarationKind, FileSource, Guard, Step, Task, TaskFile};
use crate::vars::{Var, Vars, expand_tilde};
use std::collections::{BTreeSet, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

/// Files given on the command line rather than in the task file.
#[derive(Default)]
pub struct Extra<'a> {
    pub secrets: &'a [PathBuf],
    pub age_key: Option<&'a Path>,
}

/// The tree for the tasks in `order`.
pub fn render(file: &TaskFile, order: &[String], extra: &Extra) -> String {
    let sources = Sources {
        file,
        selected: order.iter().map(String::as_str).collect(),
    };
    let mut root = Node::new(sources.show(&file.path));
    for path in extra.secrets {
        let label = format!(
            "secrets {}{} (--secrets)",
            sources.show(path),
            missing(path)
        );
        root.children.push(Node::new(label));
    }
    if let Some(path) = extra.age_key {
        let label = format!(
            "age-key {}{} (--age-key)",
            sources.show(path),
            missing(path)
        );
        root.children.push(Node::new(label));
    }
    let mut shown = HashSet::new();
    root.children
        .extend(sources.file_children(&file.path, &mut shown));
    let mut out = String::new();
    root.write("", true, true, &mut out);
    out
}

struct Node {
    label: String,
    children: Vec<Node>,
}

impl Node {
    fn new(label: impl Into<String>) -> Self {
        Node {
            label: label.into(),
            children: Vec::new(),
        }
    }

    fn write(&self, prefix: &str, last: bool, root: bool, out: &mut String) {
        let (branch, deeper) = match (root, last) {
            (true, _) => ("", String::new()),
            (false, true) => ("└── ", format!("{prefix}    ")),
            (false, false) => ("├── ", format!("{prefix}│   ")),
        };
        let _ = writeln!(out, "{prefix}{branch}{}", self.label);
        for (i, child) in self.children.iter().enumerate() {
            child.write(&deeper, i + 1 == self.children.len(), false, out);
        }
    }
}

fn missing(path: &Path) -> &'static str {
    if path.exists() { "" } else { " (missing)" }
}

struct Sources<'a> {
    file: &'a TaskFile,
    selected: HashSet<&'a str>,
}

impl<'a> Sources<'a> {
    /// A path relative to the main file's directory when it's inside it.
    fn show(&self, path: &Path) -> String {
        path.strip_prefix(&self.file.ctx.dir)
            .unwrap_or(path)
            .display()
            .to_string()
    }

    /// What one task file declares and its tasks, in the order written.
    fn file_children(&self, path: &Path, shown: &mut HashSet<PathBuf>) -> Vec<Node> {
        if !shown.insert(path.to_path_buf()) {
            return vec![Node::new("(shown above)")];
        }
        let mut entries: Vec<(usize, Node)> = Vec::new();
        for declaration in self.file.declarations.iter().filter(|d| d.file == path) {
            let shown_path = self.show(&declaration.path);
            let node = match &declaration.kind {
                DeclarationKind::Include {
                    skipped: Some(guard),
                } => Node::new(format!("include {shown_path} (skipped: {guard})")),
                DeclarationKind::Include { skipped: None } => Node {
                    label: format!("include {shown_path}"),
                    children: self.file_children(&declaration.path, shown),
                },
                DeclarationKind::Env => Node::new(format!("env {shown_path}")),
                DeclarationKind::Vars => Node::new(format!("vars {shown_path}")),
                DeclarationKind::Secrets => Node::new(format!(
                    "secrets {shown_path}{}",
                    missing(&declaration.path)
                )),
                DeclarationKind::AgeKey => Node::new(format!(
                    "age-key {shown_path}{}",
                    missing(&declaration.path)
                )),
            };
            entries.push((declaration.line, node));
        }
        for task in self.file.tasks.values() {
            let Some((task_file, line)) = task.origin.rsplit_once(':') else {
                continue;
            };
            if Path::new(task_file) != path || !self.selected.contains(task.name.as_str()) {
                continue;
            }
            if let Some(node) = self.task(task) {
                entries.push((line.parse().unwrap_or(0), node));
            }
        }
        entries.sort_by_key(|(line, _)| *line);
        entries.into_iter().map(|(_, node)| node).collect()
    }

    /// A task and what it reads, or `None` if it reads nothing.
    fn task(&self, task: &'a Task) -> Option<Node> {
        let layers = [&task.vars, &self.file.vars];
        let mut found = Found::default();
        for text in task.desc.iter().chain(&task.workdir) {
            found.scan(text, &layers);
        }
        self.block(&task.body, &layers, &mut found);
        let manual = if task.manual { " [manual]" } else { "" };
        found.into_node(format!("task {}{manual}", task.name))
    }

    fn block(&self, block: &'a Block, layers: &[&'a Vars], found: &mut Found) {
        for guard in &block.guards {
            match guard {
                Guard::If(check) | Guard::Unless(check) => found.scan(&check.text, layers),
                Guard::Ask(text) => found.scan(text, layers),
                Guard::Changed => {}
            }
        }
        if let Some(dir) = &block.workdir {
            found.scan(dir, layers);
        }
        for step in &block.steps {
            match step {
                Step::Run(text) | Step::Undo(text) | Step::Notice(text) => {
                    found.scan(text, layers);
                }
                Step::Wait(wait) => {
                    for text in wait.message.iter().chain([&wait.check.text]) {
                        found.scan(text, layers);
                    }
                }
                Step::Setting(setting) => {
                    for text in [&setting.value, &setting.get, &setting.set] {
                        found.scan(text, layers);
                    }
                }
                Step::File(file) => {
                    for text in std::iter::once(&file.path).chain(&file.owner) {
                        found.scan(text, layers);
                    }
                    match &file.source {
                        FileSource::Inline(text) => found.scan(text, layers),
                        FileSource::Copy(source) => {
                            found.scan(source, layers);
                            if file.template {
                                for text in self.templated(source, file.dir) {
                                    found.scan(&text, layers);
                                }
                            }
                            let label = self.copy(source, file.dir);
                            found
                                .nodes
                                .push(Node::new(format!("{label} → {}", file.path)));
                        }
                        FileSource::Url { url, sha256 } => {
                            found.scan(url, layers);
                            found.scan(sha256, layers);
                            found
                                .nodes
                                .push(Node::new(format!("{url} → {}", file.path)));
                        }
                        FileSource::Empty => {}
                    }
                }
                Step::Line(line) => {
                    let texts = [&line.path, &line.text]
                        .into_iter()
                        .chain(&line.base)
                        .chain(&line.matcher);
                    for text in texts {
                        found.scan(text, layers);
                    }
                }
                Step::Call { template, args } => {
                    found.scan(args, layers);
                    let template = &self.file.templates[template];
                    let mut inner = Found::default();
                    let layers: Vec<&Vars> = std::iter::once(&template.vars)
                        .chain(layers.iter().copied())
                        .collect();
                    self.block(&template.body, &layers, &mut inner);
                    let call = format!("{} {args}", template.name);
                    let label = format!(
                        "template {} ({})",
                        call.trim(),
                        self.origin(&template.origin)
                    );
                    found.nodes.extend(inner.into_node(label));
                }
                Step::Block(inner) => {
                    self.block(inner, layers, found);
                    if let Some(other) = &inner.otherwise {
                        self.block(other, layers, found);
                    }
                }
            }
        }
    }

    /// A `from` source, with how many files a directory has.
    /// The text of the files a `template` copy step fills in, so the secrets
    /// they use are listed too. Unreadable ones are left out.
    fn templated(&self, source: &str, dir: bool) -> Vec<String> {
        if source.contains("{{") {
            return Vec::new();
        }
        let path = self.file.ctx.dir.join(expand_tilde(source));
        if !dir {
            return std::fs::read_to_string(&path).into_iter().collect();
        }
        let mut names = Vec::new();
        if super::step::list_files(&path, "", &mut names).is_err() {
            return Vec::new();
        }
        names
            .iter()
            .filter_map(|name| std::fs::read_to_string(path.join(name)).ok())
            .collect()
    }

    fn copy(&self, source: &str, dir: bool) -> String {
        if source.contains("{{") {
            return source.to_string();
        }
        let path = PathBuf::from(expand_tilde(source));
        let path = if path.is_absolute() {
            path
        } else {
            self.file.ctx.dir.join(path)
        };
        let shown = self.show(&path);
        if !path.exists() {
            return format!("{shown} (missing)");
        }
        if !dir {
            return shown;
        }
        let mut names = Vec::new();
        match super::step::list_files(&path, "", &mut names) {
            Ok(()) if names.len() == 1 => format!("{shown}/ (1 file)"),
            Ok(()) => format!("{shown}/ ({} files)", names.len()),
            Err(_) => format!("{shown}/ (unreadable)"),
        }
    }

    /// `path:line` with the path shown relative.
    fn origin(&self, origin: &str) -> String {
        match origin.rsplit_once(':') {
            Some((path, line)) => format!("{}:{line}", self.show(Path::new(path))),
            None => origin.to_string(),
        }
    }
}

/// What a task or template call reads.
#[derive(Default)]
struct Found {
    nodes: Vec<Node>,
    secrets: BTreeSet<String>,
}

impl Found {
    /// Note the secrets `text` uses, directly or through variables.
    fn scan(&mut self, text: &str, layers: &[&Vars]) {
        let mut seen = HashSet::new();
        self.scan_inner(text, layers, &mut seen);
    }

    fn scan_inner(&mut self, text: &str, layers: &[&Vars], seen: &mut HashSet<String>) {
        let mut rest = text;
        while let Some(start) = rest.find("{{") {
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                return;
            };
            let name = after[..end].trim();
            rest = &after[end + 2..];
            if let Some(secret) = name.strip_prefix(crate::secrets::PREFIX) {
                self.secrets.insert(secret.to_string());
                continue;
            }
            if !seen.insert(name.to_string()) {
                continue;
            }
            let var = layers.iter().find_map(|vars| vars.get(name));
            if let Some(Var::Template(text) | Var::Shell(text, _)) = var {
                self.scan_inner(text, layers, seen);
            }
        }
    }

    fn into_node(mut self, label: String) -> Option<Node> {
        if !self.secrets.is_empty() {
            let names: Vec<String> = self.secrets.into_iter().collect();
            self.nodes
                .push(Node::new(format!("secrets: {}", names.join(", "))));
        }
        (!self.nodes.is_empty()).then_some(Node {
            label,
            children: self.nodes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::select;
    use crate::parser::parse_file;

    #[test]
    fn tree_by_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("files/fonts/sub")).unwrap();
        for name in [
            "files/ssh_config",
            "files/fonts/a.ttf",
            "files/fonts/sub/b.ttf",
        ] {
            std::fs::write(root.join(name), "x").unwrap();
        }
        std::fs::write(root.join("secrets.yaml"), "").unwrap();
        std::fs::write(root.join(".env"), "A=1\n").unwrap();
        std::fs::write(
            root.join("main.task"),
            "env \".env\"\n\
             secrets \"secrets.yaml\"\n\
             age-key \"key.age\"\n\
             let token = \"Bearer {{ secrets.api.token }}\"\n\
             include \"desktop.task\"\n\
             include \"laptop.task\" if false\n\
             task ssh {\n  file \"~/.ssh/config\" from \"files/ssh_config\"\n  run echo {{ token }}\n}\n\
             task plain {\n  run true\n}\n\
             task wifi {\n  network home\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("desktop.task"),
            "template network {\n  if test -n \"{{ secrets.wifi.password }}\"\n  file \"/etc/{{ 1 }}\" from \"{{ 1 }}.conf\"\n}\n\
             task fonts {\n  directory \"~/.local/share/fonts\" from \"files/fonts\"\n  file \"/x\" from \"nope\"\n}\n",
        )
        .unwrap();
        let file = parse_file(&root.join("main.task")).unwrap();
        let extra = Extra {
            secrets: &[root.join("more.yaml")],
            age_key: None,
        };
        let tree = render(&file, &select(&file, &[]).unwrap(), &extra);
        assert_eq!(
            tree,
            "main.task\n\
                 ├── secrets more.yaml (missing) (--secrets)\n\
                 ├── env .env\n\
                 ├── secrets secrets.yaml\n\
                 ├── age-key key.age (missing)\n\
                 ├── include desktop.task\n\
                 │   └── task fonts\n\
                 │       ├── files/fonts/ (2 files) → ~/.local/share/fonts\n\
                 │       └── nope (missing) → /x\n\
                 ├── include laptop.task (skipped: if false)\n\
                 ├── task ssh\n\
                 │   ├── files/ssh_config → ~/.ssh/config\n\
                 │   └── secrets: api.token\n\
                 └── task wifi\n    \
                     └── template network home (desktop.task:1)\n        \
                         ├── {{ 1 }}.conf → /etc/{{ 1 }}\n        \
                         └── secrets: wifi.password\n"
        );
        // With --run, only those tasks.
        let tree = render(
            &file,
            &select(&file, &["ssh".to_string()]).unwrap(),
            &Extra::default(),
        );
        assert!(!tree.contains("task fonts"), "{tree}");
        assert!(tree.contains("task ssh"), "{tree}");
    }
}
