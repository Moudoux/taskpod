// Live checks: reading `taskpod FILE -D --json` and describing its results.
// No vscode imports here, so this is unit tested with bun.

export type Outcome = "up-to-date" | "would-change" | "skipped" | "failed" | "blocked" | "unchecked";
export type StepKind = "change" | "run" | "ok" | "note" | "unchecked";

export interface StepResult {
  depth: number;
  kind: StepKind;
  label: string;
  diff?: string[];
}

export interface TaskResult {
  name: string;
  /** Absolute path of the file that defines the task (an include, maybe). */
  file: string;
  line: number | null;
  outcome: Outcome;
  reason: string | null;
  /** Steps not checked because they need secrets. */
  unchecked: number;
  steps: StepResult[];
}

export interface CheckResult {
  version: number;
  file: string;
  mode: string;
  pull: boolean;
  /** The file's own `if`/`unless` that stopped everything. */
  skipped: string | null;
  /** The check couldn't run at all, e.g. the file doesn't parse. */
  error: string | null;
  tasks: TaskResult[];
}

export const JSON_VERSION = 1;

/** Parse taskpod's output. Throws if it isn't a document this version understands. */
export function parseResult(stdout: string): CheckResult {
  const value = JSON.parse(stdout) as CheckResult;
  if (typeof value !== "object" || value === null || !Array.isArray(value.tasks)) throw new Error("not a taskpod --json document");
  if (value.version !== JSON_VERSION) throw new Error(`taskpod --json version ${value.version}, the extension reads version ${JSON_VERSION}; update one of them`);
  return value;
}

/** The results of tasks defined in `file`, by task name. */
export function tasksIn(results: Iterable<CheckResult>, file: string): Map<string, TaskResult> {
  const found = new Map<string, TaskResult>();
  for (const result of results) for (const task of result.tasks) if (task.file === file) found.set(task.name, task);
  return found;
}

function shorten(text: string, max = 60): string {
  const line = text.split("\n")[0];
  return line.length > max || line !== text ? `${line.slice(0, max - 1).trimEnd()}…` : line;
}

const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? "" : "s"}`;

/** What would change, counted at the top level of the task: "1 file, 2 commands". */
export function changeSummary(task: TaskResult): string {
  const top = task.steps.filter((s) => s.depth === 0);
  const files = top.filter((s) => s.kind === "change" && s.label.startsWith("file ")).length;
  const other = top.filter((s) => s.kind === "change" && !s.label.startsWith("file ")).length;
  const commands = top.filter((s) => s.kind === "run").length;
  const parts = [files && plural(files, "file"), other && plural(other, "block"), commands && plural(commands, "command")].filter(Boolean);
  return parts.length ? parts.join(", ") : "changes";
}

export const SYMBOLS: Record<Outcome, string> = {
  "up-to-date": "✓",
  "would-change": "~",
  skipped: "⊘",
  failed: "✗",
  blocked: "✗",
  unchecked: "?",
};

/** The CodeLens title for a task's result. */
export function title(task: TaskResult): string {
  const notChecked = task.unchecked ? ` · ${task.unchecked} not checked` : "";
  const reason = shorten(task.reason ?? "");
  switch (task.outcome) {
    case "up-to-date":
      return `✓ up to date${notChecked}`;
    case "would-change":
      return `~ ${changeSummary(task)}${notChecked}`;
    case "skipped":
      return `⊘ skipped: ${reason}`;
    case "failed":
      return `✗ failed: ${reason}`;
    case "blocked":
      return `✗ not checked, ${reason} failed`;
    case "unchecked":
      return "? not checked: needs secrets";
  }
}

const STEP_SYMBOLS: Record<StepKind, string> = { change: "→", run: "$", ok: "✓", note: "·", unchecked: "?" };

function fence(text: string, language = ""): string {
  const longest = Math.max(2, ...[...text.matchAll(/`+/g)].map((m) => m[0].length));
  const ticks = "`".repeat(longest + 1);
  return `${ticks}${language}\n${text}\n${ticks}`;
}

function stepLine(step: StepResult): string {
  const label = step.kind === "unchecked" ? `${step.label} · uses secrets, not checked` : step.label;
  return `${"  ".repeat(step.depth)}${STEP_SYMBOLS[step.kind]} ${label.split("\n").join(`\n${"  ".repeat(step.depth + 1)}`)}`;
}

/** Hover text: the outcome, then the steps and their diffs. */
export function hoverMarkdown(task: TaskResult, checkedAt?: Date): string {
  const when = checkedAt ? ` · checked ${checkedAt.toLocaleTimeString()}` : "";
  const parts = [`**${title(task)}**${when}`];
  if (task.reason && task.reason !== shorten(task.reason)) parts.push(fence(task.reason));
  const shown = task.steps.filter((s) => s.kind !== "ok" || task.outcome === "up-to-date");
  if (shown.length) parts.push(fence(shown.map(stepLine).join("\n")));
  for (const step of task.steps.filter((s) => s.diff?.length)) {
    parts.push(`\`${step.label}\``, fence(step.diff!.join("\n"), "diff"));
  }
  return parts.join("\n\n");
}

/** Everything about a task's result, as plain text for a read-only document. */
export function report(task: TaskResult): string {
  const lines = [`${SYMBOLS[task.outcome]} ${task.name}: ${task.outcome}${task.reason ? `: ${task.reason}` : ""}`, ""];
  if (task.unchecked) lines.push(`${plural(task.unchecked, "step")} not checked, they need secrets (taskpod.checkSecrets is off)`, "");
  for (const step of task.steps) {
    lines.push(stepLine(step));
    if (step.diff) lines.push(...step.diff.map((l) => `${"  ".repeat(step.depth + 1)}${l}`));
  }
  return lines.join("\n") + "\n";
}

/** Counts for the status bar: "3 up to date, 1 would change". */
export function statusSummary(tasks: TaskResult[]): string {
  const count = (o: Outcome[]) => tasks.filter((t) => o.includes(t.outcome)).length;
  const parts = [
    count(["would-change"]) && `${count(["would-change"])} would change`,
    count(["failed", "blocked"]) && `${count(["failed", "blocked"])} failed`,
    count(["unchecked"]) && `${count(["unchecked"])} not checked`,
  ].filter(Boolean);
  if (!parts.length) return tasks.length ? "up to date" : "nothing to check";
  return parts.join(", ");
}
