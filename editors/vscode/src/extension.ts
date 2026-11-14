import { execFile } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import * as vscode from "vscode";
import { BUILTIN_VARS, ENCODINGS, KEYWORDS, keywordMarkdown, type Place } from "./docs";
import { type Index, type RefKind, parse, resolveVar, symbolAt } from "./index";

const LANGUAGE = "taskpod";
const SELECTOR: vscode.DocumentSelector = { language: LANGUAGE };

/** A parsed file and the files it includes. */
interface FileIndex {
  uri: vscode.Uri;
  index: Index;
}

const cache = new Map<string, { version: number; index: Index }>();

function indexOf(document: vscode.TextDocument): Index {
  const key = document.uri.toString();
  const cached = cache.get(key);
  if (cached?.version === document.version) return cached.index;
  const index = parse(document.getText());
  cache.set(key, { version: document.version, index });
  return index;
}

function readIndex(uri: vscode.Uri): Index | undefined {
  const open = vscode.workspace.textDocuments.find((d) => d.uri.toString() === uri.toString());
  if (open) return indexOf(open);
  try {
    return parse(fs.readFileSync(uri.fsPath, "utf8"));
  } catch {
    return undefined;
  }
}

/** This file and everything it includes, recursively. */
function project(document: vscode.TextDocument): FileIndex[] {
  const files: FileIndex[] = [];
  const seen = new Set<string>();
  const visit = (uri: vscode.Uri, index: Index | undefined) => {
    if (!index || seen.has(uri.fsPath) || seen.size > 32) return;
    seen.add(uri.fsPath);
    files.push({ uri, index });
    for (const p of index.paths) {
      if (p.kind !== "include" || p.path.includes("{{")) continue;
      const target = vscode.Uri.file(resolvePath(p.path, uri));
      visit(target, readIndex(target));
    }
  };
  visit(document.uri, indexOf(document));
  return files;
}

function expandHome(p: string): string {
  return p === "~" ? os.homedir() : p.startsWith("~/") ? path.join(os.homedir(), p.slice(2)) : p;
}

/** A path from a task file: `~` is home, relative paths are relative to the file. */
function resolvePath(p: string, from: vscode.Uri): string {
  const expanded = expandHome(p);
  return path.isAbsolute(expanded) ? expanded : path.join(path.dirname(from.fsPath), expanded);
}

function range(span: { line: number; start: number; end: number }): vscode.Range {
  return new vscode.Range(span.line, span.start, span.line, span.end);
}

function definitions(files: FileIndex[], name: string, kind: RefKind, document: vscode.TextDocument, line: number): vscode.Location[] {
  const found: vscode.Location[] = [];
  for (const { uri, index } of files) {
    if (kind === "task") for (const t of index.tasks) if (t.name === name) found.push(new vscode.Location(uri, range(t)));
    if (kind === "template") for (const t of index.templates) if (t.name === name) found.push(new vscode.Location(uri, range(t)));
    if (kind === "var") {
      const local = uri.toString() === document.uri.toString() ? resolveVar(index, name, line) : index.vars.find((v) => v.name === name && !v.owner);
      if (local) found.push(new vscode.Location(uri, range(local)));
    }
  }
  return found;
}

function references(files: FileIndex[], name: string, kind: RefKind): vscode.Location[] {
  const found: vscode.Location[] = [];
  for (const { uri, index } of files) {
    for (const r of index.refs) if (r.kind === kind && r.name === name) found.push(new vscode.Location(uri, range(r)));
  }
  return found;
}

function templateDoc(document: vscode.TextDocument, files: FileIndex[], name: string): string | undefined {
  for (const { uri, index } of files) {
    const template = index.templates.find((t) => t.name === name);
    if (!template) continue;
    const text = uri.toString() === document.uri.toString() ? document.getText() : fs.readFileSync(uri.fsPath, "utf8");
    const body = text.split(/\r?\n/).slice(template.line, template.endLine + 1).join("\n");
    return `**template ${name}**\n\n\`\`\`taskpod\n${body}\n\`\`\``;
  }
  return undefined;
}

function taskDoc(files: FileIndex[], name: string): string | undefined {
  for (const { index } of files) {
    const task = index.tasks.find((t) => t.name === name);
    if (!task) continue;
    const lines = [`**${task.kind} ${name}**${task.desc ? `: ${task.desc}` : ""}`];
    if (task.parent) lines.push(`Runs after **${task.parent}**, if it changed something.`);
    if (task.needs.length) lines.push(`Needs: ${task.needs.map((n) => `\`${n}\``).join(", ")}`);
    const flags = [task.manual && "manual", task.interactive && "interactive"].filter(Boolean);
    if (flags.length) lines.push(`_${flags.join(", ")}_`);
    return lines.join("\n\n");
  }
  return undefined;
}

class Provider
  implements
    vscode.HoverProvider,
    vscode.DefinitionProvider,
    vscode.ReferenceProvider,
    vscode.RenameProvider,
    vscode.DocumentSymbolProvider,
    vscode.DocumentLinkProvider,
    vscode.CompletionItemProvider,
    vscode.CodeLensProvider
{
  provideHover(document: vscode.TextDocument, position: vscode.Position): vscode.Hover | undefined {
    const files = project(document);
    const index = files[0].index;
    const symbol = symbolAt(index, position.line, position.character);
    if (symbol) {
      let markdown: string | undefined;
      if (symbol.kind === "task") markdown = taskDoc(files, symbol.name);
      if (symbol.kind === "template") markdown = templateDoc(document, files, symbol.name);
      if (symbol.kind === "var") {
        const def = resolveVar(index, symbol.name, position.line) ?? files.slice(1).flatMap((f) => f.index.vars).find((v) => v.name === symbol.name && !v.owner);
        if (def) markdown = `**${symbol.name}** = \`${def.value}\`${def.owner ? `\n\n_in ${def.owner}_` : ""}`;
        else if (symbol.name.startsWith("secrets.")) markdown = `**${symbol.name}**: secret \`${symbol.name.slice(8)}\`, from a \`secrets\` file or \`--secrets\``;
        else if (BUILTIN_VARS[symbol.name]) markdown = `**${symbol.name}** (built in): ${BUILTIN_VARS[symbol.name]}`;
        else if (/^\d+$/.test(symbol.name)) markdown = `**${symbol.name}**: word ${symbol.name} of the template call's arguments.`;
        else if (process.env[symbol.name] !== undefined) markdown = `**${symbol.name}**: environment variable`;
        else markdown = `**${symbol.name}**: not defined`;
      }
      if (markdown) return new vscode.Hover(new vscode.MarkdownString(markdown));
    }
    const pathRef = index.paths.find((p) => p.line === position.line && position.character >= p.start && position.character <= p.end);
    if (pathRef && !pathRef.path.includes("{{")) {
      const target = pathRef.kind === "target" ? expandHome(pathRef.path) : resolvePath(pathRef.path, document.uri);
      const exists = fs.existsSync(target);
      const what = pathRef.kind === "target" ? "Writes" : "Reads";
      return new vscode.Hover(new vscode.MarkdownString(`${what} \`${target}\`${exists ? "" : " _(doesn't exist yet)_"}`));
    }
    const word = document.getWordRangeAtPosition(position);
    const markdown = word && keywordMarkdown(document.getText(word));
    return markdown ? new vscode.Hover(new vscode.MarkdownString(markdown), word) : undefined;
  }

  provideDefinition(document: vscode.TextDocument, position: vscode.Position): vscode.Definition | undefined {
    const files = project(document);
    const symbol = symbolAt(files[0].index, position.line, position.character);
    if (symbol) return definitions(files, symbol.name, symbol.kind, document, position.line);
    const pathRef = files[0].index.paths.find((p) => p.line === position.line && position.character >= p.start && position.character <= p.end);
    if (pathRef && !pathRef.path.includes("{{")) {
      const target = pathRef.kind === "target" ? expandHome(pathRef.path) : resolvePath(pathRef.path, document.uri);
      if (fs.existsSync(target)) return new vscode.Location(vscode.Uri.file(target), new vscode.Position(0, 0));
    }
    return undefined;
  }

  provideReferences(document: vscode.TextDocument, position: vscode.Position, context: vscode.ReferenceContext): vscode.Location[] {
    const files = project(document);
    const symbol = symbolAt(files[0].index, position.line, position.character);
    if (!symbol) return [];
    const refs = references(files, symbol.name, symbol.kind);
    return context.includeDeclaration ? [...definitions(files, symbol.name, symbol.kind, document, position.line), ...refs] : refs;
  }

  prepareRename(document: vscode.TextDocument, position: vscode.Position): vscode.Range {
    const index = indexOf(document);
    const symbol = symbolAt(index, position.line, position.character);
    if (!symbol) throw new Error("Only tasks, templates and variables can be renamed");
    return document.getWordRangeAtPosition(position, /[\p{L}\p{N}_.+\-/]+/u)!;
  }

  provideRenameEdits(document: vscode.TextDocument, position: vscode.Position, newName: string): vscode.WorkspaceEdit | undefined {
    const files = project(document);
    const symbol = symbolAt(files[0].index, position.line, position.character);
    if (!symbol) return undefined;
    const edit = new vscode.WorkspaceEdit();
    for (const location of [...definitions(files, symbol.name, symbol.kind, document, position.line), ...references(files, symbol.name, symbol.kind)]) {
      edit.replace(location.uri, location.range, newName);
    }
    return edit;
  }

  provideDocumentSymbols(document: vscode.TextDocument): vscode.DocumentSymbol[] {
    const index = indexOf(document);
    const symbols: vscode.DocumentSymbol[] = [];
    const whole = (start: number, end: number) => new vscode.Range(start, 0, end, document.lineAt(Math.min(end, document.lineCount - 1)).text.length);
    for (const v of index.vars.filter((v) => !v.owner)) {
      symbols.push(new vscode.DocumentSymbol(v.name, v.value, vscode.SymbolKind.Variable, whole(v.line, v.line), range(v)));
    }
    for (const t of index.templates) {
      symbols.push(new vscode.DocumentSymbol(t.name, "template", vscode.SymbolKind.Function, whole(t.line, t.endLine), range(t)));
    }
    for (const t of index.tasks) {
      const detail = t.desc ?? (t.parent ? `after ${t.parent}` : "");
      const symbol = new vscode.DocumentSymbol(t.name, detail, vscode.SymbolKind.Event, whole(t.line, t.endLine), range(t));
      for (const v of index.vars.filter((v) => v.owner === t.name)) {
        symbol.children.push(new vscode.DocumentSymbol(v.name, v.value, vscode.SymbolKind.Variable, whole(v.line, v.line), range(v)));
      }
      symbols.push(symbol);
    }
    return symbols.sort((a, b) => a.range.start.line - b.range.start.line);
  }

  provideDocumentLinks(document: vscode.TextDocument): vscode.DocumentLink[] {
    const links: vscode.DocumentLink[] = [];
    for (const p of indexOf(document).paths) {
      if (p.path.includes("{{")) continue;
      const target = p.kind === "target" ? expandHome(p.path) : resolvePath(p.path, document.uri);
      if (!fs.existsSync(target)) continue;
      const link = new vscode.DocumentLink(range(p), vscode.Uri.file(target));
      link.tooltip = p.kind === "target" ? "Open the file on this system" : "Open file";
      links.push(link);
    }
    return links;
  }

  provideCompletionItems(document: vscode.TextDocument, position: vscode.Position): vscode.CompletionItem[] {
    const files = project(document);
    const index = files[0].index;
    const info = index.lines[position.line] ?? { context: "top" };
    const before = document.lineAt(position.line).text.slice(0, position.character);
    const items: vscode.CompletionItem[] = [];
    const taskNames = [...new Set(files.flatMap((f) => f.index.tasks.map((t) => t.name)))];
    const templateNames = [...new Set(files.flatMap((f) => f.index.templates.map((t) => t.name)))];

    // Variables inside {{ }}, anywhere.
    if (/\{\{\s*[\p{L}\p{N}_.\-/]*$/u.test(before)) {
      const names = new Map<string, string>();
      for (const { index: i } of files) for (const v of i.vars) if (!v.owner || v.owner === info.owner) names.set(v.name, v.value);
      for (const [name, value] of names) {
        const item = new vscode.CompletionItem(name, vscode.CompletionItemKind.Variable);
        item.detail = value;
        items.push(item);
      }
      for (const [name, doc] of Object.entries(BUILTIN_VARS)) {
        if (name === "args" && info.ownerKind !== "template") continue;
        const item = new vscode.CompletionItem(name, vscode.CompletionItemKind.Constant);
        item.detail = "built in";
        item.documentation = doc;
        items.push(item);
      }
      return items;
    }
    if (info.context === "shell" || info.context === "string") return [];

    const task = (name: string, detail?: string) => {
      const item = new vscode.CompletionItem(name, vscode.CompletionItemKind.Event);
      item.detail = detail;
      return item;
    };
    if (/^\s*needs\s.*$/.test(before) || /^\s*subtask\s+\S+\s+of\s+\S*$/.test(before)) {
      return taskNames.filter((n) => n !== info.owner).map((n) => task(n, files.flatMap((f) => f.index.tasks).find((t) => t.name === n)?.desc));
    }
    if (/^\s*subtask\s+\S+\s+$/.test(before)) return [new vscode.CompletionItem("of", vscode.CompletionItemKind.Keyword)];
    if (/^\s*file\s.*\bencoding\s+\S*$/.test(before)) {
      return ENCODINGS.map((e) => new vscode.CompletionItem(e, vscode.CompletionItemKind.EnumMember));
    }
    if (/^\s*file\s+("[^"]*"|'[^']*')\s.*$/.test(before) || /^\s*file\s+("[^"]*"|'[^']*')\s*$/.test(before)) {
      return this.keywordItems("file");
    }
    if (/^\s*let\s+\S+\s*=\s*$/.test(before)) {
      return [
        snippet("choose", 'choose "${1:Question?}" ${2:one} ${3:two}', "Ask to pick one of the options"),
        snippet("input", 'input "${1:Question}" "${2:default}"', "Ask for text"),
        snippet("confirm", 'confirm "${1:Question?}"', "Ask yes or no: true or false"),
        snippet("$(command)", "\\$(${1:command})", "Output of a shell command"),
      ];
    }
    if (!/^\s*[\p{L}\p{N}_.+-]*$/u.test(before)) return [];

    const place: Place = info.context === "top" ? "top" : "body";
    items.push(...this.keywordItems(place));
    if (place === "body") {
      for (const name of templateNames) {
        const item = new vscode.CompletionItem(name, vscode.CompletionItemKind.Function);
        item.detail = "template";
        const doc = templateDoc(document, files, name);
        if (doc) item.documentation = new vscode.MarkdownString(doc);
        items.push(item);
      }
    }
    return items;
  }

  private keywordItems(place: Place): vscode.CompletionItem[] {
    return Object.entries(KEYWORDS)
      .filter(([, doc]) => doc.places.includes(place))
      .map(([name, doc]) => {
        const item = snippet(name, doc.snippet ?? name, doc.summary);
        item.kind = vscode.CompletionItemKind.Keyword;
        item.documentation = new vscode.MarkdownString(keywordMarkdown(name));
        return item;
      });
  }

  provideCodeLenses(document: vscode.TextDocument): vscode.CodeLens[] {
    if (!vscode.workspace.getConfiguration("taskpod").get("codeLens", true)) return [];
    return indexOf(document).tasks.flatMap((t) => {
      const at = new vscode.Range(t.line, 0, t.line, 0);
      const args = [document.uri, t.name];
      return [
        new vscode.CodeLens(at, { title: "$(play) Run", command: "taskpod.runTask", arguments: args }),
        new vscode.CodeLens(at, { title: "Dry run", command: "taskpod.dryRunTask", arguments: args }),
        new vscode.CodeLens(at, { title: "Diff", command: "taskpod.diffTask", arguments: args }),
      ];
    });
  }
}

function snippet(label: string, body: string, detail: string): vscode.CompletionItem {
  const item = new vscode.CompletionItem(label);
  item.insertText = new vscode.SnippetString(body);
  item.detail = detail;
  return item;
}

// ---- Diagnostics -------------------------------------------------------------

const KNOWN_VAR = (name: string) => name.startsWith("secrets.") || BUILTIN_VARS[name] !== undefined || /^\d+$/.test(name) || process.env[name] !== undefined;

function lint(document: vscode.TextDocument): vscode.Diagnostic[] {
  const files = project(document);
  const index = files[0].index;
  const tasks = new Set(files.flatMap((f) => f.index.tasks.map((t) => t.name)));
  const templates = new Set(files.flatMap((f) => f.index.templates.map((t) => t.name)));
  const globals = new Set(files.flatMap((f) => f.index.vars.filter((v) => !v.owner).map((v) => v.name)));
  const diagnostics: vscode.Diagnostic[] = [];
  const add = (span: { line: number; start: number; end: number }, message: string, severity = vscode.DiagnosticSeverity.Error) =>
    diagnostics.push(Object.assign(new vscode.Diagnostic(range(span), message, severity), { source: "taskpod" }));

  for (const ref of index.refs) {
    if (ref.kind === "task" && !tasks.has(ref.name)) add(ref, `Unknown task '${ref.name}'`);
    if (ref.kind === "template" && !templates.has(ref.name)) add(ref, `Unknown directive or template '${ref.name}'`);
    if (ref.kind === "var") {
      const owner = index.lines[ref.line]?.owner;
      const local = index.vars.some((v) => v.name === ref.name && v.owner === owner && owner !== undefined);
      const inTemplate = index.lines[ref.line]?.ownerKind === "template" && ref.name === "args";
      if (!local && !globals.has(ref.name) && !KNOWN_VAR(ref.name) && !inTemplate) {
        add(ref, `Unknown variable '${ref.name}', it isn't defined and isn't an environment variable here`, vscode.DiagnosticSeverity.Warning);
      }
    }
  }
  const seen = new Map<string, number>();
  for (const t of index.tasks) {
    if (seen.has(t.name)) add(t, `Task '${t.name}' is already defined on line ${seen.get(t.name)! + 1}`);
    seen.set(t.name, t.line);
  }
  for (const p of index.paths) {
    if (p.kind === "target" || p.path.includes("{{")) continue;
    if (!fs.existsSync(resolvePath(p.path, document.uri))) add(p, `${resolvePath(p.path, document.uri)} doesn't exist`);
  }
  return diagnostics;
}

/** Errors from `taskpod --check`, which knows the full language. */
function check(document: vscode.TextDocument): Promise<vscode.Diagnostic[]> {
  const binary = vscode.workspace.getConfiguration("taskpod").get("path", "taskpod");
  return new Promise((resolve) => {
    execFile(binary, [document.uri.fsPath, "--check"], { env: { ...process.env, NO_COLOR: "1" }, timeout: 10_000 }, (error, _stdout, stderr) => {
      if (!error) return resolve([]);
      if ((error as NodeJS.ErrnoException).code === "ENOENT") return resolve([]);
      const diagnostics: vscode.Diagnostic[] = [];
      for (const line of stderr.split("\n")) {
        const m = line.match(/^error: (.+?):(\d+)(?::(\d+))?: (.*)$/);
        if (!m) continue;
        const [, file, row, col, message] = m;
        if (path.resolve(file) !== path.resolve(document.uri.fsPath)) {
          const at = new vscode.Range(0, 0, 0, 0);
          diagnostics.push(Object.assign(new vscode.Diagnostic(at, `${file}:${row}: ${message}`), { source: "taskpod --check" }));
          continue;
        }
        const r = Number(row) - 1;
        const c = col ? Number(col) - 1 : 0;
        const end = document.lineAt(Math.min(r, document.lineCount - 1)).text.length;
        const diagnostic = new vscode.Diagnostic(new vscode.Range(r, c, r, Math.max(c + 1, end)), message);
        diagnostic.source = "taskpod --check";
        diagnostics.push(diagnostic);
      }
      resolve(diagnostics);
    });
  });
}

// ---- Commands ----------------------------------------------------------------

function terminal(): vscode.Terminal {
  return vscode.window.terminals.find((t) => t.name === "taskpod") ?? vscode.window.createTerminal({ name: "taskpod" });
}

function quote(arg: string): string {
  return /^[\w./~:=+-]+$/.test(arg) ? arg : `'${arg.replace(/'/g, "'\\''")}'`;
}

async function runTaskpod(uri: vscode.Uri | undefined, flags: string[]): Promise<void> {
  const target = uri ?? vscode.window.activeTextEditor?.document.uri;
  if (!target) return;
  const document = vscode.workspace.textDocuments.find((d) => d.uri.toString() === target.toString());
  if (document?.isDirty) await document.save();
  const binary = vscode.workspace.getConfiguration("taskpod").get("path", "taskpod");
  const t = terminal();
  t.show();
  t.sendText([binary, target.fsPath, ...flags].map(quote).join(" "));
}

export function activate(context: vscode.ExtensionContext): void {
  const provider = new Provider();
  const lintDiagnostics = vscode.languages.createDiagnosticCollection("taskpod");
  const checkDiagnostics = vscode.languages.createDiagnosticCollection("taskpod-check");
  const timers = new Map<string, NodeJS.Timeout>();

  const relint = (document: vscode.TextDocument) => {
    if (document.languageId !== LANGUAGE) return;
    const key = document.uri.toString();
    clearTimeout(timers.get(key));
    timers.set(key, setTimeout(() => lintDiagnostics.set(document.uri, lint(document)), 250));
  };
  const recheck = async (document: vscode.TextDocument) => {
    if (document.languageId !== LANGUAGE || document.uri.scheme !== "file") return;
    if (!vscode.workspace.getConfiguration("taskpod").get("checkOnSave", true)) return;
    checkDiagnostics.set(document.uri, await check(document));
  };

  context.subscriptions.push(
    lintDiagnostics,
    checkDiagnostics,
    vscode.languages.registerHoverProvider(SELECTOR, provider),
    vscode.languages.registerDefinitionProvider(SELECTOR, provider),
    vscode.languages.registerReferenceProvider(SELECTOR, provider),
    vscode.languages.registerRenameProvider(SELECTOR, provider),
    vscode.languages.registerDocumentSymbolProvider(SELECTOR, provider),
    vscode.languages.registerDocumentLinkProvider(SELECTOR, provider),
    vscode.languages.registerCompletionItemProvider(SELECTOR, provider, "{", " "),
    vscode.languages.registerCodeLensProvider(SELECTOR, provider),
    vscode.workspace.onDidOpenTextDocument((d) => {
      relint(d);
      void recheck(d);
    }),
    vscode.workspace.onDidChangeTextDocument((e) => relint(e.document)),
    vscode.workspace.onDidSaveTextDocument((d) => void recheck(d)),
    vscode.workspace.onDidCloseTextDocument((d) => {
      lintDiagnostics.delete(d.uri);
      checkDiagnostics.delete(d.uri);
      cache.delete(d.uri.toString());
    }),
    vscode.commands.registerCommand("taskpod.runFile", (uri?: vscode.Uri) => runTaskpod(uri, [])),
    vscode.commands.registerCommand("taskpod.dryRunFile", (uri?: vscode.Uri) => runTaskpod(uri, ["--dry-run"])),
    vscode.commands.registerCommand("taskpod.diffFile", (uri?: vscode.Uri) => runTaskpod(uri, ["--diff"])),
    vscode.commands.registerCommand("taskpod.runTask", (uri: vscode.Uri, task: string) => runTaskpod(uri, ["--run", task])),
    vscode.commands.registerCommand("taskpod.dryRunTask", (uri: vscode.Uri, task: string) => runTaskpod(uri, ["--dry-run", "--run", task])),
    vscode.commands.registerCommand("taskpod.diffTask", (uri: vscode.Uri, task: string) => runTaskpod(uri, ["--diff", "--run", task])),
  );
  for (const document of vscode.workspace.textDocuments) {
    relint(document);
    void recheck(document);
  }
}

export function deactivate(): void {}
