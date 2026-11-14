//! `--graph`: the tasks and what they need, as a tree, Graphviz or Mermaid.
//!
//! Only the file is read: guards don't run, so this shows every task that
//! could run, not what would.

use crate::ast::{Task, TaskFile};
use std::collections::HashSet;
use std::fmt::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Each task with what runs before it underneath, like `cargo tree`.
    Tree,
    /// Graphviz, for `dot -Tsvg`.
    Dot,
    /// A Mermaid flowchart, which GitHub and many editors render.
    Mermaid,
}

/// What runs before a task: its `needs`, then its parent if it's a subtask.
fn before(task: &Task) -> impl Iterator<Item = (&str, bool)> {
    task.needs
        .iter()
        .map(|n| (n.as_str(), false))
        .chain(task.parent.iter().map(|p| (p.as_str(), true)))
}

fn tags(task: &Task) -> String {
    let mut tags = Vec::new();
    if task.manual {
        tags.push("manual");
    }
    if task.interactive {
        tags.push("interactive");
    }
    if tags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", tags.join(", "))
    }
}

/// The graph of the tasks in `order` (from [`super::select`]).
pub fn render(file: &TaskFile, order: &[String], format: Format) -> String {
    let tasks: Vec<&Task> = order.iter().map(|n| &file.tasks[n]).collect();
    match format {
        Format::Tree => tree(file, &tasks),
        Format::Dot => dot(&tasks),
        Format::Mermaid => mermaid(&tasks),
    }
}

fn tree(file: &TaskFile, tasks: &[&Task]) -> String {
    let needed: HashSet<&str> = tasks
        .iter()
        .flat_map(|t| before(t).map(|(n, _)| n))
        .collect();
    let mut out = String::new();
    let mut shown = HashSet::new();
    // Tasks nothing else needs, in run order, so the last to run comes last.
    for task in tasks.iter().filter(|t| !needed.contains(t.name.as_str())) {
        let _ = writeln!(out, "{}{}", task.name, tags(task));
        shown.insert(task.name.as_str());
        branch(file, task, "", &mut shown, &mut out);
    }
    out
}

fn branch<'a>(
    file: &'a TaskFile,
    task: &'a Task,
    prefix: &str,
    shown: &mut HashSet<&'a str>,
    out: &mut String,
) {
    let children: Vec<(&str, bool)> = before(task).collect();
    for (i, (name, parent)) in children.iter().enumerate() {
        let last = i + 1 == children.len();
        let child = &file.tasks[*name];
        let how = if *parent { " (parent, runs first)" } else { "" };
        // A task already shown with its own needs isn't repeated, like `cargo tree`.
        let mark = if shown.contains(name) && before(child).next().is_some() {
            " (*)"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "{prefix}{}{name}{}{how}{mark}",
            if last { "└── " } else { "├── " },
            tags(child)
        );
        if !mark.is_empty() {
            continue;
        }
        shown.insert(name);
        let deeper = format!("{prefix}{}", if last { "    " } else { "│   " });
        branch(file, child, &deeper, shown, out);
    }
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

fn dot(tasks: &[&Task]) -> String {
    let mut out = String::from("digraph tasks {\n    rankdir=LR;\n    node [shape=box];\n");
    for task in tasks {
        let style = if task.manual { " [style=dashed]" } else { "" };
        let _ = writeln!(out, "    {}{style};", quote(&task.name));
    }
    // Arrows point the way tasks run: from what's needed to what needs it.
    for task in tasks {
        for (name, parent) in before(task) {
            let style = if parent {
                " [style=dashed, label=\"if changed\"]"
            } else {
                ""
            };
            let _ = writeln!(out, "    {} -> {}{style};", quote(name), quote(&task.name));
        }
    }
    out.push_str("}\n");
    out
}

fn mermaid(tasks: &[&Task]) -> String {
    // Task names may hold characters Mermaid ids can't, so nodes get numbers.
    let id = |name: &str| {
        let i = tasks.iter().position(|t| t.name == name).unwrap_or(0);
        format!("t{i}")
    };
    let mut out = String::from("flowchart LR\n");
    for (i, task) in tasks.iter().enumerate() {
        let label = format!("{}{}", task.name, tags(task)).replace('"', "#quot;");
        let _ = writeln!(out, "    t{i}[\"{label}\"]");
    }
    for task in tasks {
        for (name, parent) in before(task) {
            let arrow = if parent { "-.->|if changed|" } else { "-->" };
            let _ = writeln!(out, "    {} {arrow} {}", id(name), id(&task.name));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::select;
    use crate::parser::parse_str;
    use std::path::Path;

    const SOURCE: &str = "
        task base {\n run true\n}
        task yay {\n needs base\n run true\n}
        task apps {\n needs yay base\n run true\n}
        subtask fonts of apps {\n run true\n}
        task setup {\n manual\n interactive\n needs yay\n run true\n}
    ";

    fn graph(targets: &[&str], format: Format) -> String {
        let file = parse_str(SOURCE, Path::new("/tmp")).unwrap();
        let targets: Vec<String> = targets.iter().map(|t| t.to_string()).collect();
        render(&file, &select(&file, &targets).unwrap(), format)
    }

    #[test]
    fn trees() {
        assert_eq!(
            graph(&[], Format::Tree),
            "fonts\n\
             └── apps (parent, runs first)\n    \
                 ├── yay\n    \
                 │   └── base\n    \
                 └── base\n"
        );
        assert_eq!(
            graph(&["setup", "apps"], Format::Tree),
            "fonts\n\
             └── apps (parent, runs first)\n    \
                 ├── yay\n    \
                 │   └── base\n    \
                 └── base\n\
             setup [manual, interactive]\n\
             └── yay (*)\n"
        );
    }

    #[test]
    fn dot_and_mermaid() {
        let dot = graph(&["apps"], Format::Dot);
        assert!(dot.starts_with("digraph tasks {"), "{dot}");
        assert!(dot.contains("    \"base\" -> \"yay\";\n"), "{dot}");
        assert!(
            dot.contains("    \"apps\" -> \"fonts\" [style=dashed, label=\"if changed\"];\n"),
            "{dot}"
        );
        let mermaid = graph(&["setup"], Format::Mermaid);
        assert_eq!(
            mermaid,
            "flowchart LR\n    \
                 t0[\"base\"]\n    \
                 t1[\"yay\"]\n    \
                 t2[\"setup [manual, interactive]\"]\n    \
                 t0 --> t1\n    \
                 t1 --> t2\n"
        );
    }
}
