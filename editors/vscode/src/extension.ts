import { execFile } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import * as vscode from "vscode";
import { BUILTIN_VARS, ENCODINGS, KEYWORDS, keywordMarkdown, type Place } from "./docs";
import { LiveChecks, REPORT_SCHEME } from "./checks";
import { type Index, type RefKind, parse, resolveVar, symbolAt } from "./index";
import { hoverMarkdown, title } from "./live";
import { IncludeGraph } from "./project";
import { type SecretKey, globFiles, secretKeys } from "./secrets";

const LANGUAGE = "taskpod";
const SELECTOR: vscode.DocumentSelector = { language: LANGUAGE };

/** A parsed file and the files it includes. */
interface FileIndex {
  uri: vscode.Uri;
  index: Index;
}

const cache = new Map<string, { version: number; index: Index }>();
let checks: LiveChecks | undefined;

/** The line of each task in a document. */
function taskLines(document: vscode.TextDocument): Map<string, number> {
  return new Map(indexOf(document).tasks.map((t) => [t.name, t.line]));
}

function indexOf(document: vscode.TextDocument): Index {
  const key = document.uri.toString();
  const cached = cache.get(key);
  if (cached?.version === document.version) return cached.index;
  const index = parse(document.getText());
  cache.set(key, { version: document.version, index });
  if (document.uri.scheme === "file") recordIncludes(document.uri, index);
  return index;
}

const diskCache = new Map<string, { mtime: number; index: Index }>();

function readIndex(uri: vscode.Uri): Index | undefined {
  const open = vscode.workspace.textDocuments.find((d) => d.uri.toString() === uri.toString());
  return open ? indexOf(open) : readDiskIndex(uri);
}

function readDiskIndex(uri: vscode.Uri): Index | undefined {
  try {
    const mtime = fs.statSync(uri.fsPath).mtimeMs;
    const cached = diskCache.get(uri.fsPath);
    const index = cached?.mtime === mtime ? cached.index : parse(fs.readFileSync(uri.fsPath, "utf8"));
    diskCache.set(uri.fsPath, { mtime, index });
    recordIncludes(uri, index);
    return index;
  } catch {
    diskCache.delete(uri.fsPath);
    return undefined;
  }
}

/** Which files include which, so an included file also sees the files above it. */
const graph = new IncludeGraph();
/** Called when a file's includes change, since other open files may now resolve differently. */
let onIncludesChanged = () => {};

function includePaths(uri: vscode.Uri, index: Index): string[] {
  return index.paths.filter((p) => p.kind === "include" && !p.path.includes("{{")).map((p) => resolvePath(p.path, uri));
}

function recordIncludes(uri: vscode.Uri, index: Index | undefined): void {
  const changed = index ? graph.set(uri.fsPath, includePaths(uri, index)) : graph.delete(uri.fsPath);
  if (changed) onIncludesChanged();
}

/**
 * This file first, then everything it includes, then the files that include it
 * (up to the files taskpod runs on) and everything those include: all of it is
 * one file to taskpod, so tasks, templates and variables resolve across it.
 */
function project(document: vscode.TextDocument): FileIndex[] {
  const files: FileIndex[] = [];
  const seen = new Set<string>();
  const visit = (uri: vscode.Uri, index: Index | undefined) => {
    if (!index || seen.has(uri.fsPath) || seen.size > 128) return;
    seen.add(uri.fsPath);
    files.push({ uri, index });
    for (const target of includePaths(uri, index)) {
      const targetUri = vscode.Uri.file(target);
      visit(targetUri, readIndex(targetUri));
    }
  };
  visit(document.uri, indexOf(document));
  for (const root of graph.roots(document.uri.fsPath)) {
    const uri = vscode.Uri.file(root);
    visit(uri, readIndex(uri));
  }
  return files;
}

function expandHome(p: string): string {
  return p === "~" ? os.homedir() : p.startsWith("~/") ? path.join(os.homedir(), p.slice(2)) : p;
}

function isFile(path: string): boolean {
  try {
    return fs.statSync(path).isFile();
  } catch {
    return false;
  }
}

/** A path from a task file: `~` is home, relative paths are relative to the file. */
function resolvePath(p: string, from: vscode.Uri): string {
  const expanded = expandHome(p);
  return path.isAbsolute(expanded) ? expanded : path.join(path.dirname(from.fsPath), expanded);
}

/** A `secrets` path with `*` or `?` in its file name, which taskpod globs. */
const isGlob = (p: { kind: string; path: string }) => p.kind === "secrets" && /[*?]/.test(path.basename(p.path));

// ---- Secrets -----------------------------------------------------------------

const SECRETS = "secrets.";
const isSecret = (name: string) => name.startsWith(SECRETS);

interface SecretFile {
  uri: vscode.Uri;
  keys: SecretKey[];
}

const secretCache = new Map<string, { mtime: number; keys: SecretKey[] }>();

/** Keys of a sops file, read from disk: sops keeps keys readable, values are never used. */
function readSecretKeys(file: string): SecretKey[] | undefined {
  try {
    const mtime = fs.statSync(file).mtimeMs;
    const cached = secretCache.get(file);
    if (cached?.mtime === mtime) return cached.keys;
    const keys = secretKeys(fs.readFileSync(file, "utf8"), file);
    secretCache.set(file, { mtime, keys });
    return keys;
  } catch {
    return undefined;
  }
}

/** Files from `secrets` lines in this file and its includes (not `--secrets`, which only the command line knows). */
function secretFiles(files: FileIndex[]): SecretFile[] {
  const found: SecretFile[] = [];
  const seen = new Set<string>();
  for (const { uri, index } of files) {
    for (const p of index.paths) {
      if (p.kind !== "secrets" || p.path.includes("{{")) continue;
      for (const target of globFiles(resolvePath(p.path, uri))) {
        if (seen.has(target)) continue;
        seen.add(target);
        const keys = readSecretKeys(target);
        if (keys) found.push({ uri: vscode.Uri.file(target), keys });
      }
    }
  }
  return found;
}

function secretDefinitions(files: FileIndex[], name: string): vscode.Location[] {
  const key = name.slice(SECRETS.length);
  return secretFiles(files).flatMap((f) => f.keys.filter((k) => k.name === key).map((k) => new vscode.Location(f.uri, range(k))));
}

function secretDoc(files: FileIndex[], name: string): string {
  const key = name.slice(SECRETS.length);
  const secrets = secretFiles(files);
  const base = (uri: vscode.Uri) => `\`${path.basename(uri.fsPath)}\``;
  const where = secrets.filter((f) => f.keys.some((k) => k.name === key)).map((f) => base(f.uri));
  if (where.length) return `**${name}**: secret from ${where.join(", ")}`;
  if (secrets.length) return `**${name}**: not in ${secrets.map((f) => base(f.uri)).join(", ")}, so it must come from \`--secrets\``;
  return `**${name}**: secret, from a \`secrets\` file or \`taskpod --secrets FILE\``;
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
  readonly onDidChangeCodeLenses = checks?.onDidChangeResults;

  provideHover(document: vscode.TextDocument, position: vscode.Position): vscode.Hover | undefined {
    const files = project(document);
    const index = files[0].index;
    const symbol = symbolAt(index, position.line, position.character);
    if (symbol) {
      let markdown: string | undefined;
      if (symbol.kind === "task") {
        markdown = taskDoc(files, symbol.name);
        const result = symbol.definition ? checks?.tasksIn(document.uri).get(symbol.name) : undefined;
        if (result) markdown = `${markdown ?? ""}\n\n---\n\n${hoverMarkdown(result, checks?.checkedAt(document.uri, symbol.name))}`;
      }
      if (symbol.kind === "template") markdown = templateDoc(document, files, symbol.name);
      if (symbol.kind === "var") {
        const def = resolveVar(index, symbol.name, position.line) ?? files.slice(1).flatMap((f) => f.index.vars).find((v) => v.name === symbol.name && !v.owner);
        if (def) markdown = `**${symbol.name}** = \`${def.value}\`${def.owner ? `\n\n_in ${def.owner}_` : ""}`;
        else if (isSecret(symbol.name)) markdown = secretDoc(files, symbol.name);
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
      const what = pathRef.kind === "target" ? "Writes" : "Reads";
      if (isGlob(pathRef)) {
        const found = globFiles(target);
        const list = found.map((f) => `- \`${path.basename(f)}\``).join("\n");
        return new vscode.Hover(new vscode.MarkdownString(`${what} \`${target}\`${found.length ? `:\n\n${list}` : " _(no files match)_"}`));
      }
      const exists = fs.existsSync(target);
      return new vscode.Hover(new vscode.MarkdownString(`${what} \`${target}\`${exists ? "" : " _(doesn't exist yet)_"}`));
    }
    const word = document.getWordRangeAtPosition(position);
    const markdown = word && keywordMarkdown(document.getText(word));
    return markdown ? new vscode.Hover(new vscode.MarkdownString(markdown), word) : undefined;
  }

  provideDefinition(document: vscode.TextDocument, position: vscode.Position): vscode.Definition | undefined {
    const files = project(document);
    const symbol = symbolAt(files[0].index, position.line, position.character);
    if (symbol && symbol.kind === "var" && isSecret(symbol.name)) return secretDefinitions(files, symbol.name);
    if (symbol) return definitions(files, symbol.name, symbol.kind, document, position.line);
    const pathRef = files[0].index.paths.find((p) => p.line === position.line && position.character >= p.start && position.character <= p.end);
    if (pathRef && !pathRef.path.includes("{{")) {
      const target = pathRef.kind === "target" ? expandHome(pathRef.path) : resolvePath(pathRef.path, document.uri);
      if (isGlob(pathRef)) return globFiles(target).map((f) => new vscode.Location(vscode.Uri.file(f), new vscode.Position(0, 0)));
      if (isFile(target)) return new vscode.Location(vscode.Uri.file(target), new vscode.Position(0, 0));
    }
    return undefined;
  }

  provideReferences(document: vscode.TextDocument, position: vscode.Position, context: vscode.ReferenceContext): vscode.Location[] {
    const files = project(document);
    const symbol = symbolAt(files[0].index, position.line, position.character);
    if (!symbol) return [];
    const refs = references(files, symbol.name, symbol.kind);
    if (!context.includeDeclaration) return refs;
    const secret = symbol.kind === "var" && isSecret(symbol.name);
    return [...(secret ? secretDefinitions(files, symbol.name) : definitions(files, symbol.name, symbol.kind, document, position.line)), ...refs];
  }

  prepareRename(document: vscode.TextDocument, position: vscode.Position): vscode.Range {
    const index = indexOf(document);
    const symbol = symbolAt(index, position.line, position.character);
    if (!symbol) throw new Error("Only tasks, templates and variables can be renamed");
    // sops ties each value to its key, so renaming a key in the file would break it.
    if (isSecret(symbol.name)) throw new Error("Rename secrets with sops (sops edit), then update the references");
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
      // Directories (`directory "path" from "dir"`) can't be opened in an editor.
      if (!isFile(target)) continue;
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
      for (const { uri, keys } of secretFiles(files)) {
        for (const key of keys.filter((k) => k.leaf)) {
          const item = new vscode.CompletionItem(SECRETS + key.name, vscode.CompletionItemKind.Field);
          item.detail = `secret · ${path.basename(uri.fsPath)}`;
          items.push(item);
        }
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
    if (/^\s*(file|directory)\s.*\bencoding\s+\S*$/.test(before)) {
      return ENCODINGS.map((e) => new vscode.CompletionItem(e, vscode.CompletionItemKind.EnumMember));
    }
    if (/^\s*(file|directory)\s+("[^"]*"|'[^']*')\s.*$/.test(before)) {
      return this.keywordItems("file");
    }
    if (/^\s*line\s+("[^"]*"|'[^']*')\s.*$/.test(before)) return this.keywordItems("line");
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
    const actions = vscode.workspace.getConfiguration("taskpod").get("codeLens", true);
    const results = checks?.tasksIn(document.uri);
    return indexOf(document).tasks.flatMap((t) => {
      const at = new vscode.Range(t.line, 0, t.line, 0);
      const args = [document.uri, t.name];
      const lenses: vscode.CodeLens[] = [];
      const result = results?.get(t.name);
      if (result) {
        const tooltip = "Result of the last live check on this system. Click for the full diff.";
        lenses.push(new vscode.CodeLens(at, { title: title(result), tooltip, command: "taskpod.showCheck", arguments: args }));
      }
      if (actions) {
        lenses.push(
          new vscode.CodeLens(at, { title: "$(play) Run", command: "taskpod.runTask", arguments: args }),
          new vscode.CodeLens(at, { title: "Dry run", command: "taskpod.dryRunTask", arguments: args }),
          new vscode.CodeLens(at, { title: "Diff", command: "taskpod.diffTask", arguments: args }),
        );
      }
      return lenses;
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

const KNOWN_VAR = (name: string) => isSecret(name) || BUILTIN_VARS[name] !== undefined || /^\d+$/.test(name) || process.env[name] !== undefined;

function lint(document: vscode.TextDocument): vscode.Diagnostic[] {
  const files = project(document);
  const index = files[0].index;
  const tasks = new Set(files.flatMap((f) => f.index.tasks.map((t) => t.name)));
  const templates = new Set(files.flatMap((f) => f.index.templates.map((t) => t.name)));
  const globals = new Set(files.flatMap((f) => f.index.vars.filter((v) => !v.owner).map((v) => v.name)));
  const secrets = secretFiles(files);
  const secretNames = new Set(secrets.flatMap((f) => f.keys.filter((k) => k.leaf).map((k) => SECRETS + k.name)));
  const diagnostics: vscode.Diagnostic[] = [];
  const add = (span: { line: number; start: number; end: number }, message: string, severity = vscode.DiagnosticSeverity.Error) =>
    diagnostics.push(Object.assign(new vscode.Diagnostic(range(span), message, severity), { source: "taskpod" }));

  for (const ref of index.refs) {
    if (ref.kind === "task" && !tasks.has(ref.name)) add(ref, `Unknown task '${ref.name}'`);
    if (ref.kind === "template" && !templates.has(ref.name)) add(ref, `Unknown directive or template '${ref.name}'`);
    if (ref.kind === "var" && isSecret(ref.name) && secrets.length && !secretNames.has(ref.name)) {
      const where = secrets.map((f) => path.basename(f.uri.fsPath)).join(", ");
      add(ref, `No secret '${ref.name.slice(SECRETS.length)}' in ${where}`, vscode.DiagnosticSeverity.Warning);
    }
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
    const target = resolvePath(p.path, document.uri);
    if (isGlob(p)) {
      if (!globFiles(target).length) add(p, `no files match ${target}`);
    } else if (!fs.existsSync(target)) add(p, `${target} doesn't exist`);
  }
  return diagnostics;
}

/** `taskpod --check` errors that only mean a name is defined in a file that includes this one. */
const NEEDS_INCLUDER = /needs unknown task '|unknown directive or template '/;

/**
 * Errors from `taskpod --check`, which knows the full language. An included file is
 * also checked from the files that include it, since on its own it may use tasks and
 * templates they define: of its own check, only errors that don't depend on them count.
 */
async function check(document: vscode.TextDocument): Promise<vscode.Diagnostic[]> {
  const roots = graph.roots(document.uri.fsPath);
  const own = await checkFrom(document.uri.fsPath, document, true);
  if (!roots.length) return own;
  const fromRoots = (await Promise.all(roots.map((root) => checkFrom(root, document, false)))).flat();
  const seen = new Set<string>();
  return [...own.filter((d) => !NEEDS_INCLUDER.test(d.message)), ...fromRoots].filter((d) => {
    const key = `${d.range.start.line}:${d.message}`;
    return !seen.has(key) && seen.add(key);
  });
}

/** Errors from checking `target`, placed in `document`; errors in other files at its top if `others`. */
function checkFrom(target: string, document: vscode.TextDocument, others: boolean): Promise<vscode.Diagnostic[]> {
  const binary = vscode.workspace.getConfiguration("taskpod").get("path", "taskpod");
  return new Promise((resolve) => {
    execFile(binary, [target, "--check"], { env: { ...process.env, NO_COLOR: "1" }, timeout: 10_000 }, (error, _stdout, stderr) => {
      if (!error) return resolve([]);
      if ((error as NodeJS.ErrnoException).code === "ENOENT") return resolve([]);
      const diagnostics: vscode.Diagnostic[] = [];
      for (const line of stderr.split("\n")) {
        const m = line.match(/^error: (.+?):(\d+)(?::(\d+))?: (.*)$/);
        if (!m) continue;
        const [, file, row, col, message] = m;
        if (path.resolve(file) !== path.resolve(document.uri.fsPath)) {
          if (!others) continue;
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

function activeTaskFile(uri?: vscode.Uri): vscode.Uri | undefined {
  const target = uri ?? vscode.window.activeTextEditor?.document.uri;
  return target?.scheme === "file" ? target : undefined;
}

export function activate(context: vscode.ExtensionContext): void {
  const live = new LiveChecks(context.workspaceState, taskLines);
  checks = live;
  const provider = new Provider();
  const lintDiagnostics = vscode.languages.createDiagnosticCollection("taskpod");
  const checkDiagnostics = vscode.languages.createDiagnosticCollection("taskpod-check");
  const timers = new Map<string, NodeJS.Timeout>();
  const watcher = vscode.workspace.createFileSystemWatcher("**/*.task");

  const relint = (document: vscode.TextDocument) => {
    if (document.languageId !== LANGUAGE) return;
    const key = document.uri.toString();
    clearTimeout(timers.get(key));
    timers.set(
      key,
      setTimeout(() => {
        lintDiagnostics.set(document.uri, lint(document));
        for (const editor of vscode.window.visibleTextEditors) if (editor.document === document) live.decorate(editor);
      }, 250),
    );
  };
  const recheck = async (document: vscode.TextDocument) => {
    if (document.languageId !== LANGUAGE || document.uri.scheme !== "file") return;
    if (!vscode.workspace.getConfiguration("taskpod").get("checkOnSave", true)) return;
    checkDiagnostics.set(document.uri, await check(document));
  };

  context.subscriptions.push(
    live,
    vscode.workspace.registerTextDocumentContentProvider(REPORT_SCHEME, live),
    vscode.window.onDidChangeActiveTextEditor(() => live.updateStatus()),
    vscode.window.onDidChangeVisibleTextEditors((editors) => editors.forEach((e) => live.decorate(e))),
    vscode.commands.registerCommand("taskpod.enableChecks", async (uri?: vscode.Uri) => {
      const target = activeTaskFile(uri);
      if (target) await live.setEnabled(target, true);
    }),
    vscode.commands.registerCommand("taskpod.disableChecks", async (uri?: vscode.Uri) => {
      const target = activeTaskFile(uri);
      if (target) await live.setEnabled(target, false);
    }),
    vscode.commands.registerCommand("taskpod.refreshChecks", (uri?: vscode.Uri) => {
      const target = activeTaskFile(uri);
      if (target && live.isEnabled(target)) live.schedule(target, 0);
      else live.refreshAll();
    }),
    vscode.commands.registerCommand("taskpod.showCheck", async (uri: vscode.Uri, task: string) => {
      const document = await vscode.workspace.openTextDocument(live.reportUri(uri, task));
      await vscode.languages.setTextDocumentLanguage(document, "diff");
      await vscode.window.showTextDocument(document, { preview: true, viewColumn: vscode.ViewColumn.Beside });
    }),
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
      if (d.languageId === LANGUAGE) live.schedule(d.uri, 0);
    }),
    vscode.workspace.onDidChangeTextDocument((e) => relint(e.document)),
    vscode.workspace.onDidSaveTextDocument((d) => {
      void recheck(d);
      if (d.languageId === LANGUAGE) live.saved(d.uri);
    }),
    vscode.workspace.onDidCloseTextDocument((d) => {
      lintDiagnostics.delete(d.uri);
      checkDiagnostics.delete(d.uri);
      cache.delete(d.uri.toString());
      // Unsaved include lines may have been recorded; go back to what's on disk.
      if (d.languageId === LANGUAGE && d.uri.scheme === "file") recordIncludes(d.uri, readDiskIndex(d.uri));
    }),
    watcher,
    watcher.onDidCreate((uri) => void readIndex(uri)),
    watcher.onDidChange((uri) => void readIndex(uri)),
    watcher.onDidDelete((uri) => {
      diskCache.delete(uri.fsPath);
      recordIncludes(uri, undefined);
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
    if (document.languageId === LANGUAGE) live.schedule(document.uri, 0);
  }
  live.updateStatus();

  // Find which workspace files include which, so included files see the files above them.
  let includesTimer: NodeJS.Timeout | undefined;
  const relintAll = () => vscode.workspace.textDocuments.forEach(relint);
  void Promise.resolve(vscode.workspace.findFiles("**/*.task", "**/node_modules/**")).then((uris) => {
    for (const uri of uris) readIndex(uri);
    onIncludesChanged = () => {
      clearTimeout(includesTimer);
      includesTimer = setTimeout(relintAll, 300);
    };
    // The first diagnostics came before the files above each open file were known.
    relintAll();
    for (const document of vscode.workspace.textDocuments) if (graph.roots(document.uri.fsPath).length) void recheck(document);
  });
  context.subscriptions.push({ dispose: () => clearTimeout(includesTimer) });
}

export function deactivate(): void {}
