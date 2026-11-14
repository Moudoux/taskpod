// Live checks: `taskpod FILE -D --json --no-secrets` on open and save, for
// files the user turned this on for. The list lives in workspace state, which
// stays on this machine, so a file meant for another system never runs here
// unless someone asks for it here.

import { execFile } from "node:child_process";
import * as path from "node:path";
import * as vscode from "vscode";
import { type CheckResult, type Outcome, type TaskResult, parseResult, report, statusSummary, tasksIn } from "./live";

const ENABLED_KEY = "taskpod.liveChecks";
export const REPORT_SCHEME = "taskpod-check";
const DEBOUNCE_MS = 500;

interface Entry {
  result?: CheckResult;
  checkedAt?: Date;
  /** Why the last check didn't produce results. */
  problem?: string;
  running?: AbortController;
}

const COLORS: Record<Outcome, string> = {
  "up-to-date": "#4caf50",
  "would-change": "#e2b93d",
  skipped: "#8c8c8c",
  failed: "#f14c4c",
  blocked: "#f14c4c",
  unchecked: "#8c8c8c",
};

/** A small filled (or, for skipped and unchecked, hollow) circle for the gutter. */
function gutterIcon(outcome: Outcome): vscode.Uri {
  const color = COLORS[outcome];
  const hollow = outcome === "skipped" || outcome === "unchecked";
  const fill = hollow ? `fill="none" stroke="${color}" stroke-width="1.5"` : `fill="${color}"`;
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><circle cx="8" cy="8" r="4" ${fill}/></svg>`;
  return vscode.Uri.parse(`data:image/svg+xml;base64,${Buffer.from(svg).toString("base64")}`);
}

export class LiveChecks implements vscode.TextDocumentContentProvider, vscode.Disposable {
  private readonly entries = new Map<string, Entry>();
  private readonly timers = new Map<string, NodeJS.Timeout>();
  private readonly status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 50);
  private readonly decorations = new Map<Outcome, vscode.TextEditorDecorationType>();
  private readonly changed = new vscode.EventEmitter<void>();
  private readonly reportChanged = new vscode.EventEmitter<vscode.Uri>();
  /** Fires when results change, so CodeLenses refresh. */
  readonly onDidChangeResults = this.changed.event;
  /** For the report documents (TextDocumentContentProvider). */
  readonly onDidChange = this.reportChanged.event;

  constructor(
    private readonly state: vscode.Memento,
    /** The line of each task in a document, from the extension's own index. */
    private readonly taskLines: (document: vscode.TextDocument) => Map<string, number>,
  ) {
    for (const outcome of Object.keys(COLORS) as Outcome[]) {
      this.decorations.set(
        outcome,
        vscode.window.createTextEditorDecorationType({
          gutterIconPath: gutterIcon(outcome),
          gutterIconSize: "contain",
          overviewRulerColor: outcome === "up-to-date" || outcome === "skipped" || outcome === "unchecked" ? undefined : COLORS[outcome],
          overviewRulerLane: vscode.OverviewRulerLane.Left,
        }),
      );
    }
  }

  // ---- Which files ----

  private enabledFiles(): string[] {
    return this.state.get<string[]>(ENABLED_KEY, []);
  }

  isEnabled(uri: vscode.Uri): boolean {
    return uri.scheme === "file" && this.enabledFiles().includes(uri.fsPath);
  }

  async setEnabled(uri: vscode.Uri, enabled: boolean): Promise<void> {
    const files = new Set(this.enabledFiles());
    if (enabled) files.add(uri.fsPath);
    else files.delete(uri.fsPath);
    await this.state.update(ENABLED_KEY, [...files].sort());
    if (enabled) {
      this.schedule(uri, 0);
    } else {
      clearTimeout(this.timers.get(uri.fsPath));
      this.entries.get(uri.fsPath)?.running?.abort();
      this.entries.delete(uri.fsPath);
      this.refresh();
    }
  }

  // ---- Running ----

  /** Check `uri` soon; a newer request replaces an older one. */
  schedule(uri: vscode.Uri, delay = DEBOUNCE_MS): void {
    if (!this.isEnabled(uri)) return;
    const key = uri.fsPath;
    clearTimeout(this.timers.get(key));
    this.timers.set(
      key,
      setTimeout(() => {
        this.timers.delete(key);
        void this.check(key);
      }, delay),
    );
  }

  /** A saved file: check it if it's enabled, and every enabled file whose last results include its tasks. */
  saved(uri: vscode.Uri): void {
    this.schedule(uri);
    for (const [root, entry] of this.entries) {
      if (root !== uri.fsPath && entry.result?.tasks.some((t) => t.file === uri.fsPath)) this.schedule(vscode.Uri.file(root));
    }
  }

  refreshAll(): void {
    for (const file of this.enabledFiles()) this.schedule(vscode.Uri.file(file), 0);
  }

  private async check(file: string): Promise<void> {
    const entry = this.entries.get(file) ?? {};
    this.entries.set(file, entry);
    entry.running?.abort();
    const controller = new AbortController();
    entry.running = controller;
    this.refresh();

    const config = vscode.workspace.getConfiguration("taskpod");
    const binary = config.get("path", "taskpod");
    const args = [file, "--diff", "--json"];
    if (!config.get("checkSecrets", false)) args.push("--no-secrets");
    const timeout = config.get("checkTimeout", 120) * 1000;

    const outcome = await new Promise<{ result?: CheckResult; problem?: string }>((resolve) => {
      execFile(
        binary,
        args,
        { cwd: path.dirname(file), env: { ...process.env, NO_COLOR: "1" }, timeout, signal: controller.signal, maxBuffer: 64 * 1024 * 1024 },
        (error, stdout, stderr) => {
          if (controller.signal.aborted) return resolve({});
          const code = (error as NodeJS.ErrnoException | null)?.code;
          if (code === "ENOENT") return resolve({ problem: `${binary} not found, set taskpod.path` });
          if (error && (error as { killed?: boolean }).killed) return resolve({ problem: `the check took longer than ${timeout / 1000} s (taskpod.checkTimeout)` });
          try {
            // taskpod exits with 1 when a task fails, and still prints the results.
            resolve({ result: parseResult(stdout) });
          } catch (e) {
            const detail = stderr.trim().split("\n").pop() || (e as Error).message;
            resolve({ problem: detail.includes("--json") || detail.includes("unexpected argument") ? `this taskpod is too old for live checks (${detail})` : detail });
          }
        },
      );
    });
    if (controller.signal.aborted || entry.running !== controller) return;
    entry.running = undefined;
    if (!this.isEnabled(vscode.Uri.file(file))) return;
    if (outcome.result?.error) {
      // Parse errors already show as diagnostics from `--check`.
      entry.problem = outcome.result.error;
      entry.result = undefined;
    } else if (outcome.result) {
      entry.result = outcome.result;
      entry.problem = undefined;
      entry.checkedAt = new Date();
    } else {
      entry.problem = outcome.problem;
    }
    this.refresh();
  }

  // ---- Results ----

  private results(): CheckResult[] {
    return [...this.entries.values()].flatMap((e) => (e.result ? [e.result] : []));
  }

  /** Results for tasks defined in `uri`, from every enabled file that includes it. */
  tasksIn(uri: vscode.Uri): Map<string, TaskResult> {
    return tasksIn(this.results(), uri.fsPath);
  }

  checkedAt(uri: vscode.Uri, task: string): Date | undefined {
    for (const entry of this.entries.values()) {
      if (entry.result?.tasks.some((t) => t.file === uri.fsPath && t.name === task)) return entry.checkedAt;
    }
    return undefined;
  }

  /** A read-only document with everything about one task's result. */
  reportUri(uri: vscode.Uri, task: string): vscode.Uri {
    return vscode.Uri.from({ scheme: REPORT_SCHEME, path: `/${task}.diff`, query: uri.fsPath });
  }

  provideTextDocumentContent(uri: vscode.Uri): string {
    const task = path.basename(uri.path, ".diff");
    const result = this.tasksIn(vscode.Uri.file(uri.query)).get(task);
    return result ? report(result) : `No check result for ${task} yet.\n`;
  }

  // ---- Display ----

  refresh(): void {
    this.changed.fire();
    for (const editor of vscode.window.visibleTextEditors) this.decorate(editor);
    for (const document of vscode.workspace.textDocuments) {
      if (document.uri.scheme === REPORT_SCHEME) this.reportChanged.fire(document.uri);
    }
    this.updateStatus();
  }

  decorate(editor: vscode.TextEditor): void {
    if (editor.document.languageId !== "taskpod") return;
    const results = this.tasksIn(editor.document.uri);
    const lines = results.size ? this.taskLines(editor.document) : new Map<string, number>();
    const byOutcome = new Map<Outcome, vscode.Range[]>();
    for (const [name, result] of results) {
      const line = lines.get(name);
      if (line === undefined) continue;
      byOutcome.set(result.outcome, [...(byOutcome.get(result.outcome) ?? []), new vscode.Range(line, 0, line, 0)]);
    }
    for (const [outcome, type] of this.decorations) editor.setDecorations(type, byOutcome.get(outcome) ?? []);
  }

  updateStatus(): void {
    const document = vscode.window.activeTextEditor?.document;
    if (!document || document.languageId !== "taskpod" || document.uri.scheme !== "file") {
      this.status.hide();
      return;
    }
    const entry = this.entries.get(document.uri.fsPath);
    const status = this.status;
    if (!this.isEnabled(document.uri)) {
      status.text = "$(circle-slash) taskpod";
      status.tooltip = "Live checks are off for this file on this machine. Click to check it against this system on open and save.";
      status.command = { title: "Check this file", command: "taskpod.enableChecks", arguments: [document.uri] };
    } else if (entry?.running) {
      status.text = "$(sync~spin) taskpod: checking";
      status.tooltip = "Comparing this file with the system (taskpod --diff). Click to stop checking this file.";
      status.command = { title: "Stop checking", command: "taskpod.disableChecks", arguments: [document.uri] };
    } else if (entry?.problem) {
      status.text = "$(warning) taskpod";
      status.tooltip = `Live check failed: ${entry.problem}\n\nClick to check again.`;
      status.command = { title: "Check again", command: "taskpod.refreshChecks", arguments: [document.uri] };
    } else {
      const tasks = entry?.result?.tasks ?? [];
      const skipped = entry?.result?.skipped;
      status.text = `$(pass) taskpod: ${skipped ? "skipped" : statusSummary(tasks)}`;
      const when = entry?.checkedAt ? `Checked ${entry.checkedAt.toLocaleTimeString()}` : "Not checked yet";
      status.tooltip = `${skipped ? `Nothing to do on this system: ${skipped}\n\n` : ""}${when}. Click to check again.`;
      status.command = { title: "Check again", command: "taskpod.refreshChecks", arguments: [document.uri] };
    }
    void vscode.commands.executeCommand("setContext", "taskpod.checksEnabled", this.isEnabled(document.uri));
    status.show();
  }

  dispose(): void {
    for (const timer of this.timers.values()) clearTimeout(timer);
    for (const entry of this.entries.values()) entry.running?.abort();
    for (const type of this.decorations.values()) type.dispose();
    this.status.dispose();
    this.changed.dispose();
    this.reportChanged.dispose();
  }
}
