//! Terminal output: live spinners for running tasks, an overall progress bar,
//! and prompts that pause the display while they wait for an answer.
//!
//! When stderr isn't a terminal (or with `--plain`), output is plain lines
//! with each task's output prefixed by its name.

use crate::vars::Prompt;
use anyhow::{Result, bail};
use console::{Style, style};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, RwLock};
use std::time::Duration;

const PREFIX_COLORS: [u8; 6] = [6, 5, 4, 3, 2, 14];
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "⠿"];
/// Output lines kept per task, shown if it fails.
const TAIL: usize = 40;

pub struct Ui {
    multi: MultiProgress,
    /// Live display with spinners.
    pub fancy: bool,
    pub verbose: bool,
    interactive: bool,
    prompt: Mutex<()>,
    total: Mutex<Option<ProgressBar>>,
    width: AtomicUsize,
}

static UI: OnceLock<Ui> = OnceLock::new();

/// Secret values, longest first, hidden from everything taskpod prints.
static REDACTIONS: RwLock<Vec<String>> = RwLock::new(Vec::new());
/// Shorter values aren't hidden, they'd match too much unrelated text.
const MIN_REDACTED: usize = 4;
const REDACTED: &str = "••••";

/// Hide these values from output from now on. Multi-line values are also
/// hidden line by line, since output and diffs are shown a line at a time.
pub fn add_redactions<'a>(values: impl IntoIterator<Item = &'a String>) {
    let mut list = REDACTIONS.write().unwrap_or_else(|e| e.into_inner());
    for value in values {
        let parts = std::iter::once(value.trim()).chain(value.lines().map(str::trim));
        for part in parts {
            if part.chars().count() >= MIN_REDACTED && !list.iter().any(|v| v == part) {
                list.push(part.to_string());
            }
        }
    }
    list.sort_by_key(|v| std::cmp::Reverse(v.len()));
}

/// `text` with every secret value replaced by `••••`.
pub fn redact(text: &str) -> Cow<'_, str> {
    let list = REDACTIONS.read().unwrap_or_else(|e| e.into_inner());
    if !list.iter().any(|v| text.contains(v.as_str())) {
        return Cow::Borrowed(text);
    }
    let mut text = text.to_string();
    for value in list.iter() {
        text = text.replace(value.as_str(), REDACTED);
    }
    Cow::Owned(text)
}

pub fn init(verbose: bool, plain: bool) {
    if std::env::var_os("NO_COLOR").is_some() {
        console::set_colors_enabled(false);
        console::set_colors_enabled_stderr(false);
    }
    let fancy = !plain
        && std::io::stderr().is_terminal()
        && std::env::var("TERM").map_or(true, |t| t != "dumb");
    let _ = UI.set(Ui::new(fancy, verbose));
    let mut config = inquire::ui::RenderConfig::default_colored();
    config.prompt_prefix = inquire::ui::Styled::new("?").with_fg(inquire::ui::Color::LightYellow);
    config.answered_prompt_prefix =
        inquire::ui::Styled::new("✓").with_fg(inquire::ui::Color::LightGreen);
    inquire::set_global_render_config(config);
}

pub fn ui() -> &'static Ui {
    UI.get_or_init(|| Ui::new(false, false))
}

impl Ui {
    fn new(fancy: bool, verbose: bool) -> Self {
        let target = if fancy {
            ProgressDrawTarget::stderr_with_hz(15)
        } else {
            ProgressDrawTarget::hidden()
        };
        Ui {
            multi: MultiProgress::with_draw_target(target),
            fancy,
            verbose,
            interactive: std::io::stdin().is_terminal(),
            prompt: Mutex::new(()),
            total: Mutex::new(None),
            width: AtomicUsize::new(0),
        }
    }

    /// A permanent line, printed above the live display.
    pub fn line(&self, text: impl AsRef<str>) {
        let text = redact(text.as_ref());
        if self.fancy {
            // Lines wider than the terminal would be dropped by the live
            // display, so those are printed with it paused.
            let width = console::Term::stderr().size().1 as usize;
            if console::measure_text_width(text.as_ref()) >= width {
                self.multi.suspend(|| eprintln!("{}", text.as_ref()));
            } else {
                let _ = self.multi.println(text.as_ref());
            }
        } else {
            println!("{}", text.as_ref());
        }
    }

    /// A line of output from a task, prefixed with its name.
    pub fn output(&self, task: &str, text: &str) {
        let width = self.width.load(Ordering::Relaxed);
        let color = PREFIX_COLORS[hash(task) % PREFIX_COLORS.len()];
        let prefix = Style::new()
            .color256(color)
            .apply_to(format!("{task:<width$} │"));
        self.line(format!("{prefix} {text}"));
    }

    /// Start the overall progress bar for `names`.
    pub fn begin(&self, names: &[&str], what: &str) {
        let width = names
            .iter()
            .map(|n| n.chars().count())
            .max()
            .unwrap_or(0)
            .min(24);
        self.width.store(width, Ordering::Relaxed);
        if !self.fancy {
            return;
        }
        let bar = self.multi.add(ProgressBar::new(names.len() as u64));
        bar.set_style(
            ProgressStyle::with_template("{bar:28.cyan/238} {pos}/{len} {msg} {elapsed:.dim}")
                .expect("valid template")
                .progress_chars("━╸━"),
        );
        bar.set_message(what.to_string());
        bar.enable_steady_tick(Duration::from_millis(100));
        *self.total.lock().unwrap_or_else(|e| e.into_inner()) = Some(bar);
    }

    pub fn end(&self) {
        if let Some(bar) = self.total.lock().unwrap_or_else(|e| e.into_inner()).take() {
            bar.finish_and_clear();
        }
    }

    pub fn advance(&self) {
        if let Some(bar) = &*self.total.lock().unwrap_or_else(|e| e.into_inner()) {
            bar.inc(1);
        }
    }

    /// A spinner for a running task.
    pub fn task(&self, name: &str) -> TaskLine {
        let bar = self.fancy.then(|| {
            let bar = ProgressBar::new_spinner();
            let total = self.total.lock().unwrap_or_else(|e| e.into_inner());
            let bar = match &*total {
                Some(total) => self.multi.insert_before(total, bar),
                None => self.multi.add(bar),
            };
            let width = self.width.load(Ordering::Relaxed);
            bar.set_style(
                ProgressStyle::with_template(&format!(
                    "{{spinner:.cyan}} {{prefix:{width}.bold}}  {{wide_msg:.dim}}"
                ))
                .expect("valid template")
                .tick_strings(SPINNER),
            );
            bar.set_prefix(name.to_string());
            bar.enable_steady_tick(Duration::from_millis(80));
            bar
        });
        TaskLine {
            name: name.to_string(),
            bar,
            tail: Mutex::new(VecDeque::new()),
        }
    }

    /// Run `f` with the live display paused and the terminal to itself.
    pub fn suspend<R>(&self, f: impl FnOnce() -> R) -> R {
        let _held = self.hold();
        if self.fancy {
            self.multi.suspend(f)
        } else {
            f()
        }
    }

    fn hold(&self) -> MutexGuard<'_, ()> {
        self.prompt.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ask a yes or no question. Without a terminal, the answer is no.
    pub fn confirm(&self, question: &str) -> Result<bool> {
        if !self.interactive {
            self.line(format!(
                "{} {question} {}",
                style("?").yellow(),
                style("no (not a terminal)").dim()
            ));
            return Ok(false);
        }
        self.suspend(|| inquire::Confirm::new(question).with_default(false).prompt())
            .map_err(prompt_error)
    }

    /// Ask a question for a variable.
    pub fn prompt(&self, prompt: &Prompt) -> Result<String> {
        if !self.interactive {
            let answer = match prompt {
                Prompt::Confirm(_) => "false".to_string(),
                Prompt::Choose(_, options) => options[0].clone(),
                Prompt::Input(_, Some(default)) => default.clone(),
                Prompt::Input(question, None) => {
                    bail!("{question:?} needs an answer, but there's no terminal")
                }
            };
            let question = match prompt {
                Prompt::Confirm(q) | Prompt::Choose(q, _) | Prompt::Input(q, _) => q,
            };
            self.line(format!(
                "{} {question} {}",
                style("?").yellow(),
                style(format!("{answer} (not a terminal)")).dim()
            ));
            return Ok(answer);
        }
        self.suspend(|| match prompt {
            Prompt::Confirm(question) => inquire::Confirm::new(question)
                .with_default(false)
                .prompt()
                .map(|yes| yes.to_string()),
            Prompt::Choose(question, options) => {
                inquire::Select::new(question, options.clone()).prompt()
            }
            Prompt::Input(question, default) => {
                let text = inquire::Text::new(question);
                match default {
                    Some(default) => text.with_default(default).prompt(),
                    None => text.prompt(),
                }
            }
        })
        .map_err(prompt_error)
    }
}

fn prompt_error(error: inquire::InquireError) -> anyhow::Error {
    match error {
        inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted => {
            anyhow::anyhow!("cancelled")
        }
        other => anyhow::anyhow!("prompt failed: {other}"),
    }
}

fn hash(text: &str) -> usize {
    text.bytes()
        .fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize))
}

/// The live line of a running task.
pub struct TaskLine {
    name: String,
    bar: Option<ProgressBar>,
    tail: Mutex<VecDeque<String>>,
}

impl TaskLine {
    /// What the task is doing now.
    pub fn status(&self, message: &str) {
        match &self.bar {
            Some(bar) => bar.set_message(redact(message).into_owned()),
            None => ui().output(&self.name, message),
        }
    }

    /// A line of command output.
    pub fn output(&self, line: &str) {
        let ui = ui();
        if ui.verbose || self.bar.is_none() {
            ui.output(&self.name, line);
        }
        if let Some(bar) = &self.bar
            && !line.trim().is_empty()
        {
            bar.set_message(redact(line).into_owned());
        }
        let mut tail = self.tail.lock().unwrap_or_else(|e| e.into_inner());
        if tail.len() == TAIL {
            tail.pop_front();
        }
        tail.push_back(line.to_string());
    }

    /// Recent output, for showing after a failure. Empty if it was already shown.
    pub fn take_tail(&self) -> Vec<String> {
        if self.bar.is_none() || ui().verbose {
            return Vec::new();
        }
        self.tail
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    pub fn shows_live_output(&self) -> bool {
        self.bar.is_none() || ui().verbose
    }
}

impl Drop for TaskLine {
    fn drop(&mut self) {
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
    }
}

/// Status line symbols and styles.
pub fn done(name: &str, detail: &str) -> String {
    format!(
        "{} {} {}",
        style("✓").green(),
        style(name).bold(),
        style(detail).dim()
    )
}

pub fn would_change(name: &str, detail: &str) -> String {
    format!(
        "{} {} {}",
        style("~").yellow().bold(),
        style(name).bold(),
        style(detail).dim()
    )
}

pub fn up_to_date(name: &str) -> String {
    format!(
        "{} {}",
        style("✓").green().dim(),
        style(format!("{name} · up to date")).dim()
    )
}

pub fn skipped(name: &str, reason: &str) -> String {
    style(format!("○ {name} · skipped: {reason}"))
        .dim()
        .to_string()
}

pub fn failed(name: &str, error: &str) -> String {
    format!(
        "{} {} {}",
        style("✗").red().bold(),
        style(name).bold(),
        style(error).red()
    )
}

pub fn change(text: &str) -> String {
    format!("{} {text}", style("→").yellow())
}

pub fn ok(text: &str) -> String {
    format!("{} {}", style("✓").green(), style(text).dim())
}

pub fn command(text: &str) -> String {
    format!("{} {text}", style("$").dim())
}
