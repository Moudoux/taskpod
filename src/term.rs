//! Terminal output shared by tasks running in parallel.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

const COLORS: [&str; 6] = ["36", "35", "34", "33", "32", "96"];

pub struct Term {
    lock: Mutex<()>,
    pub color: bool,
    pub verbose: bool,
    /// Width of the task name column for prefixed output.
    width: AtomicUsize,
}

static TERM: OnceLock<Term> = OnceLock::new();

pub fn init(verbose: bool) {
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let _ = TERM.set(Term {
        lock: Mutex::new(()),
        color,
        verbose,
        width: AtomicUsize::new(0),
    });
}

pub fn term() -> &'static Term {
    TERM.get_or_init(|| Term {
        lock: Mutex::new(()),
        color: false,
        verbose: false,
        width: AtomicUsize::new(0),
    })
}

/// Align task output prefixes to the longest of these names.
pub fn set_width<'a>(names: impl IntoIterator<Item = &'a str>) {
    let width = names
        .into_iter()
        .map(|n| n.chars().count())
        .max()
        .unwrap_or(0);
    term().width.store(width.min(24), Ordering::Relaxed);
}

impl Term {
    /// Hold the terminal, e.g. while asking a question.
    pub fn hold(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn task_color(name: &str) -> &'static str {
        let hash = name
            .bytes()
            .fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize));
        COLORS[hash % COLORS.len()]
    }

    /// A line of output from a task.
    pub fn output(&self, task: &str, line: &str) {
        let width = self.width.load(Ordering::Relaxed);
        let prefix = format!("{task:<width$} │");
        let prefix = self.paint(Self::task_color(task), &prefix);
        let _held = self.hold();
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{prefix} {line}");
    }

    /// A status line, e.g. a task starting or finishing.
    pub fn status(&self, symbol: &str, code: &str, task: &str, message: &str) {
        let symbol = self.paint(code, symbol);
        let name = self.paint("1", task);
        let message = if message.is_empty() {
            String::new()
        } else {
            format!(" {}", self.paint("2", message))
        };
        let _held = self.hold();
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{symbol} {name}{message}");
    }

    pub fn info(&self, message: &str) {
        let _held = self.hold();
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{}", self.paint("2", message));
    }
}
