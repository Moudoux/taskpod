//! `--pull`: bring changes made on the system back into the task files.
//!
//! Only file steps can be pulled. A `from` file gets the system's copy; an
//! inline string is rewritten in the task file. When inline content uses
//! `{{ variables }}`, system changes to plain lines are mapped back onto the
//! template, and changes to lines with a variable are refused, so values
//! (secrets in particular) never end up in the task files.

use crate::ui;
use crate::vars::Scope;
use anyhow::{Result, bail};
use similar::DiffOp;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// A change to make once every task was looked at.
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    /// Replace `original` at `start..end` in a task file.
    Source {
        file: PathBuf,
        start: usize,
        end: usize,
        original: String,
        text: String,
    },
    /// Replace a file copied with `from`.
    Copy { file: PathBuf, bytes: Vec<u8> },
}

/// Inline content, mapped back from what's on the system.
pub struct Mapped {
    /// The new template text.
    pub text: String,
    /// Diff lines of changes that couldn't be mapped back.
    pub refused: Vec<String>,
}

/// Map `system` back onto `template`, which renders (in `scope`) to what
/// taskpod would write.
pub fn map_back(template: &str, scope: &Scope, system: &str) -> Result<Mapped> {
    if !template.contains("{{") {
        let refused = secret_lines(system);
        let text = if refused.is_empty() {
            escape(system)
        } else {
            template.to_string()
        };
        return Ok(Mapped { text, refused });
    }
    let template_lines: Vec<&str> = template.split_inclusive('\n').collect();
    let mut rendered = Vec::with_capacity(template_lines.len());
    for (n, line) in template_lines.iter().enumerate() {
        let text = scope.expand(line)?;
        if text.trim_end_matches('\n').contains('\n') {
            bail!(
                "line {} of the content renders to several lines, so changes can't be mapped back",
                n + 1
            );
        }
        rendered.push(text);
    }
    let rendered: Vec<&str> = rendered.iter().map(String::as_str).collect();
    let system_lines: Vec<&str> = system.split_inclusive('\n').collect();
    let ops = similar::capture_diff_slices(similar::Algorithm::Myers, &rendered, &system_lines);
    let mut text = String::new();
    let mut refused = Vec::new();
    for op in ops {
        let (old, new) = (op.old_range(), op.new_range());
        if let DiffOp::Equal { .. } = op {
            text.extend(template_lines[old].iter().copied());
            continue;
        }
        // Pair changed lines up one to one, so each is judged on its own.
        for i in 0..old.len().max(new.len()) {
            let old_line = (i < old.len()).then(|| old.start + i);
            let new_line = system_lines.get(new.start + i).filter(|_| i < new.len());
            let templated = old_line.is_some_and(|o| template_lines[o].contains("{{"));
            let secret = new_line.is_some_and(|l| has_secret(l));
            if templated || secret {
                if let Some(o) = old_line {
                    text.push_str(template_lines[o]);
                }
                let why = if templated {
                    "uses a variable, change the task file by hand"
                } else {
                    "holds a secret value"
                };
                refused.push(
                    console::style(format!("not pulled, {why}:"))
                        .yellow()
                        .to_string(),
                );
                if let Some(o) = old_line {
                    refused.push(format!("  -{}", rendered[o].trim_end_matches('\n')));
                }
                if let Some(line) = new_line {
                    refused.push(format!("  +{}", line.trim_end_matches('\n')));
                }
            } else if let Some(line) = new_line {
                text.push_str(&escape(line));
            }
        }
    }
    Ok(Mapped { text, refused })
}

/// Content from a `from` file or without variables can't be checked line by
/// line against a template, so lines holding a secret value are refused.
fn secret_lines(text: &str) -> Vec<String> {
    if ui::redact(text) == text {
        return Vec::new();
    }
    vec![
        console::style("not pulled, the system's file holds a secret value")
            .yellow()
            .to_string(),
    ]
}

/// Whether `text` holds a known secret value.
pub fn has_secret(text: &str) -> bool {
    ui::redact(text) != text
}

/// Text written into a template, so `{{` stays literal.
fn escape(text: &str) -> String {
    text.replace("{{", "\\{{")
}

/// A string literal for `content`, in the style of `original` where possible.
pub fn literal(original: &str, content: &str) -> String {
    let parses_back =
        |literal: &str| crate::parser::string_literal(literal).is_ok_and(|value| value == content);
    if original.starts_with("\"\"\"") && !content.contains("\"\"\"") {
        // Keep the indentation of the closing quotes, when they're on their own line.
        let last = original.rsplit('\n').next().unwrap_or_default();
        let indent = last
            .strip_suffix("\"\"\"")
            .filter(|ws| original.contains('\n') && ws.trim().is_empty())
            .unwrap_or("");
        let mut out = String::from("\"\"\"\n");
        for line in content.lines() {
            if !line.is_empty() {
                out.push_str(indent);
            }
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(indent);
        out.push_str("\"\"\"");
        if parses_back(&out) {
            return out;
        }
    }
    let mut out = String::from("\"");
    for c in content.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// What applying the edits did.
#[derive(Debug, Default)]
pub struct Applied {
    pub written: Vec<PathBuf>,
    pub problems: Vec<String>,
}

/// Write the edits. Identical edits (a template called twice) count once;
/// different edits to the same place are a conflict, and the first one wins.
pub fn apply(edits: &[Edit]) -> Applied {
    let mut applied = Applied::default();
    let mut sources: BTreeMap<&PathBuf, Vec<(usize, usize, &str, &str)>> = BTreeMap::new();
    let mut copies: BTreeMap<&PathBuf, &[u8]> = BTreeMap::new();
    for edit in edits {
        match edit {
            Edit::Source {
                file,
                start,
                end,
                original,
                text,
            } => {
                let list = sources.entry(file).or_default();
                if list
                    .iter()
                    .any(|e| (e.0, e.1, e.3) == (*start, *end, text.as_str()))
                {
                    continue;
                }
                let overlaps = list
                    .iter()
                    .any(|e| *start < e.1 && e.0 < *end || (*start == e.0 && *end == e.1));
                if overlaps {
                    applied.problems.push(format!(
                        "{}: two calls want different content at the same place, kept the first",
                        file.display()
                    ));
                    continue;
                }
                list.push((*start, *end, original, text));
            }
            Edit::Copy { file, bytes } => match copies.get(file) {
                Some(existing) if existing != bytes => applied.problems.push(format!(
                    "{}: two files want different content, kept the first",
                    file.display()
                )),
                Some(_) => {}
                None => {
                    copies.insert(file, bytes);
                }
            },
        }
    }
    for (file, mut list) in sources {
        let result = (|| -> Result<()> {
            let mut source = std::fs::read_to_string(file)?;
            for (start, end, original, _) in &list {
                if source.get(*start..*end) != Some(*original) {
                    bail!("changed since it was read, not pulled");
                }
            }
            list.sort_by_key(|e| std::cmp::Reverse(e.0));
            for (start, end, _, text) in list {
                source.replace_range(start..end, text);
            }
            std::fs::write(file, source)?;
            Ok(())
        })();
        match result {
            Ok(()) => applied.written.push(file.clone()),
            Err(e) => applied.problems.push(format!("{}: {e:#}", file.display())),
        }
    }
    for (file, bytes) in copies {
        let result = file
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(file, bytes));
        match result {
            Ok(()) => applied.written.push(file.clone()),
            Err(e) => applied.problems.push(format!("{}: {e}", file.display())),
        }
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vars::{Ctx, Vars};

    #[test]
    fn maps_plain_lines_back() {
        let ctx = Ctx::new(PathBuf::from("."));
        let mut vars = Vars::default();
        vars.literal("name", "pod");
        let scope = Scope::root(&vars, &ctx);
        let template = "a\nname={{ name }}\nb\n";
        // A plain line changed, one added, and the templated line kept.
        let mapped = map_back(template, &scope, "A\nname=pod\nb\n{{ x }}\n").unwrap();
        assert_eq!(mapped.text, "A\nname={{ name }}\nb\n\\{{ x }}\n");
        assert!(mapped.refused.is_empty());
        // Next to a refused change, a plain change is still pulled.
        let mapped = map_back(template, &scope, "A\nname=other\nb\n").unwrap();
        assert_eq!(mapped.text, "A\nname={{ name }}\nb\n");
        assert_eq!(mapped.refused.len(), 3);
        // Changing the templated line is refused.
        let mapped = map_back(template, &scope, "a\nname=other\nb\n").unwrap();
        assert_eq!(mapped.text, template);
        assert!(!mapped.refused.is_empty());
        // Without variables, the system's content is taken as is.
        let mapped = map_back("x\n", &scope, "y {{\n").unwrap();
        assert_eq!(mapped.text, "y \\{{\n");
    }

    #[test]
    fn writes_literals() {
        let original = "\"\"\"\n        old\n        \"\"\"";
        assert_eq!(
            literal(original, "one\n\n  two\n"),
            "\"\"\"\n        one\n\n          two\n        \"\"\""
        );
        // Without a final newline, a triple-quoted string can't hold it.
        assert_eq!(literal(original, "a\"b"), r#""a\"b""#);
        assert_eq!(literal("'x'", "a\\{{ b\n"), r#""a\\{{ b\n""#);
        for content in ["a\\{{ b\n", "tab\there\n", "\n\nlead\n"] {
            let lit = literal(original, content);
            assert_eq!(
                crate::parser::string_literal(&lit).unwrap(),
                content,
                "{lit}"
            );
        }
    }

    #[test]
    fn applies_edits() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.task");
        std::fs::write(&file, "file \"a\" \"old\"\n").unwrap();
        let edit = |start, end, original: &str, text: &str| Edit::Source {
            file: file.clone(),
            start,
            end,
            original: original.into(),
            text: text.into(),
        };
        let edits = [
            edit(9, 14, "\"old\"", "\"new\""),
            edit(9, 14, "\"old\"", "\"new\""),
            edit(9, 14, "\"old\"", "\"other\""),
            edit(8, 8, "", " mode 600"),
            Edit::Copy {
                file: dir.path().join("files/x"),
                bytes: b"x".to_vec(),
            },
        ];
        let applied = apply(&edits);
        assert_eq!(applied.problems.len(), 1, "{:?}", applied.problems);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "file \"a\" mode 600 \"new\"\n"
        );
        assert_eq!(std::fs::read(dir.path().join("files/x")).unwrap(), b"x");
        // A file changed since it was read is left alone.
        let applied = apply(&[edit(9, 14, "\"old\"", "\"new\"")]);
        assert_eq!(applied.problems.len(), 1);
    }
}
