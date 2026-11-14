//! `--json`: the result of a check as one JSON document, for editors.
//!
//! Every string is plain text (no colors) with secret values redacted.

use super::{Mode, Outcome, Runner, StepKind};
use crate::ui::redact;
use serde_json::{Value, json};
use std::path::Path;

/// Bumped when the shape changes in a way readers must know about.
pub const VERSION: u32 = 1;

fn absolute(file: &Path) -> String {
    std::path::absolute(file)
        .unwrap_or_else(|_| file.to_path_buf())
        .display()
        .to_string()
}

fn clean(text: &str) -> String {
    redact(&console::strip_ansi_codes(text)).into_owned()
}

fn mode(mode: Mode) -> &'static str {
    match mode {
        Mode::Diff => "diff",
        Mode::DryRun => "dry-run",
        Mode::Run => "run",
    }
}

/// The document for a check that couldn't start: the file didn't parse, say.
pub fn error(file: &Path, check: Mode, pull: bool, error: &str) -> Value {
    json!({
        "version": VERSION,
        "file": absolute(file),
        "mode": mode(check),
        "pull": pull,
        "skipped": null,
        "error": clean(error),
        "tasks": [],
    })
}

/// The document for a check of `file`; `skipped` is the file's own guard
/// that stopped everything, if one did.
pub fn document(
    file: &Path,
    runner: &Runner,
    outcomes: &[(String, Outcome)],
    skipped: Option<&str>,
) -> Value {
    let reports = runner.reports.lock().unwrap_or_else(|e| e.into_inner());
    let tasks: Vec<Value> = outcomes
        .iter()
        .map(|(name, outcome)| {
            let task = &runner.file.tasks[name];
            let (path, line) = match task.origin.rsplit_once(':') {
                Some((path, line)) => (path, line.parse::<u64>().ok()),
                None => (task.origin.as_str(), None),
            };
            let (outcome, reason) = match outcome {
                Outcome::Ran => ("would-change", None),
                Outcome::UpToDate => ("up-to-date", None),
                Outcome::Skipped(r) => ("skipped", Some(r)),
                Outcome::Failed(r) => ("failed", Some(r)),
                Outcome::Blocked(r) => ("blocked", Some(r)),
                Outcome::Unchecked(r) => ("unchecked", Some(r)),
            };
            let report = reports.get(name);
            let steps: Vec<Value> = report
                .map(|r| r.steps.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|step| {
                    let kind = match step.kind {
                        StepKind::Change => "change",
                        StepKind::Run => "run",
                        StepKind::Ok => "ok",
                        StepKind::Note => "note",
                        StepKind::Unchecked => "unchecked",
                    };
                    let mut value = json!({
                        "depth": step.depth,
                        "kind": kind,
                        "label": clean(&step.label),
                    });
                    if let Some(diff) = &step.diff {
                        value["diff"] = diff.iter().map(|l| clean(l)).collect();
                    }
                    value
                })
                .collect();
            json!({
                "name": name,
                "file": path,
                "line": line,
                "outcome": outcome,
                "reason": reason.map(|r| clean(r)),
                "unchecked": report.map_or(0, |r| r.unchecked),
                "steps": steps,
            })
        })
        .collect();
    json!({
        "version": VERSION,
        "file": absolute(file),
        "mode": mode(runner.opts.mode),
        "pull": runner.opts.pull,
        "undo": runner.opts.undo,
        "skipped": skipped.map(clean),
        "error": null,
        "tasks": tasks,
    })
}
