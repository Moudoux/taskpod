// A line-based index of a task file: definitions, references and the context
// of each line. It's forgiving, so it works on files that are half written.

import { KEYWORDS } from "./docs";

export interface Span {
  line: number;
  start: number;
  end: number;
}

export interface TaskDef extends Span {
  name: string;
  kind: "task" | "subtask";
  parent?: string;
  desc?: string;
  needs: string[];
  manual: boolean;
  interactive: boolean;
  endLine: number;
}

export interface TemplateDef extends Span {
  name: string;
  /** Named parameters, `template name a b="default" {`. */
  params: string[];
  needs: string[];
  endLine: number;
}

export interface VarDef extends Span {
  name: string;
  value: string;
  /** The task or template that defines it, or undefined at the top level. */
  owner?: string;
}

export type RefKind = "task" | "template" | "var";

export interface Ref extends Span {
  name: string;
  kind: RefKind;
}

export interface PathRef extends Span {
  path: string;
  /** include/env/secrets/from point at files next to the task file; target is where a file step writes. */
  kind: "include" | "env" | "secrets" | "age-key" | "from" | "target";
}

export type Context = "top" | "body" | "shell" | "string";

export interface LineInfo {
  context: Context;
  /** Enclosing task or template. */
  owner?: string;
  ownerKind?: "task" | "template";
}

export interface Index {
  tasks: TaskDef[];
  templates: TemplateDef[];
  vars: VarDef[];
  refs: Ref[];
  paths: PathRef[];
  lines: LineInfo[];
}

const NAME = "[\\p{L}\\p{N}_.+-]+";
// Flags may come before the keyword: `sudo interactive task name ... {`.
const DEFINITION = new RegExp(
  `^(\\s*)((?:(?:sudo|terminal|keep|manual|interactive)\\s+)*)(task|subtask|template)\\s+(${NAME})(?:\\s+of\\s+(${NAME}))?`,
  "u",
);
/** A word of a definition's header, a quoted default included. */
const HEADER_WORD = /(?:[^\s\\{"']|"[^"]*"|'[^']*')+/g;
const LET = new RegExp(`^(\\s*)let\\s+(${NAME})\\s*=\\s*(.*)$`, "u");
const WORD = new RegExp(NAME, "gu");
const INTERPOLATION = /(?<!\\)\{\{\s*([\p{L}\p{N}_.\-/]+)\s*\}\}/gu;
const STRING = /"((?:[^"\\]|\\.)*)"|'([^']*)'/g;

interface Frame {
  kind: "task" | "template" | "block";
  owner?: string;
  ownerKind?: "task" | "template";
  task?: TaskDef;
  template?: TemplateDef;
}

/** Strip a trailing ` # comment` that isn't inside quotes. */
function stripComment(line: string): string {
  let quote: string | undefined;
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    if (quote) {
      if (c === "\\" && quote === '"') i++;
      else if (c === quote) quote = undefined;
    } else if (c === '"' || c === "'") {
      quote = c;
    } else if (c === "#" && (i === 0 || /\s/.test(line[i - 1]))) {
      return line.slice(0, i);
    }
  }
  return line;
}

/** Strings on a line, with the column of their content. */
function strings(line: string): { value: string; start: number; end: number }[] {
  const found = [];
  for (const m of line.matchAll(STRING)) {
    const value = m[1] ?? m[2] ?? "";
    found.push({ value, start: m.index! + 1, end: m.index! + 1 + value.length });
  }
  return found;
}

/** Count `{` minus `}` outside quotes and comments, for raw shell blocks. */
function braceDelta(line: string): number {
  let delta = 0;
  let quote: string | undefined;
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    if (quote) {
      if (c === "\\" && quote === '"') i++;
      else if (c === quote) quote = undefined;
    } else if (c === "\\") {
      i++;
    } else if (c === '"' || c === "'") {
      quote = c;
    } else if (c === "#" && (i === 0 || /[\s;&|(]/.test(line[i - 1]))) {
      break;
    } else if (c === "{") {
      delta++;
    } else if (c === "}") {
      delta--;
    }
  }
  return delta;
}

export function parse(text: string): Index {
  const index: Index = { tasks: [], templates: [], vars: [], refs: [], paths: [], lines: [] };
  const lines = text.split(/\r?\n/);
  const stack: Frame[] = [];
  let shellDepth = 0; // inside `run {` while > 0
  let inTriple = false;
  let continued = false; // the previous line ended with `\`

  const top = () => stack[stack.length - 1];
  const info = (context: Context): LineInfo => ({ context, owner: top()?.owner, ownerKind: top()?.ownerKind });
  const varRefs = (line: string, number: number) => {
    for (const m of line.matchAll(INTERPOLATION)) {
      const start = m.index! + m[0].indexOf(m[1], 2);
      index.refs.push({ name: m[1], kind: "var", line: number, start, end: start + m[1].length });
    }
  };

  lines.forEach((raw, number) => {
    if (inTriple) {
      index.lines.push(info("string"));
      varRefs(raw, number);
      if (raw.includes('"""')) inTriple = false;
      return;
    }
    if (shellDepth > 0) {
      index.lines.push(info("shell"));
      varRefs(raw, number);
      shellDepth += braceDelta(raw);
      return;
    }
    const line = stripComment(raw);
    const trimmed = line.trim();
    index.lines.push(info(stack.length ? "body" : "top"));
    const isContinuation = continued;
    continued = trimmed.endsWith("\\");
    if (!trimmed) return;
    varRefs(line, number);
    if (isContinuation) {
      if (index.refs.at(-1)?.kind === "task" && /^\s*needs\b/.test(lines[number - 1] ?? "")) {
        for (const m of line.matchAll(WORD)) {
          index.refs.push({ name: m[0], kind: "task", line: number, start: m.index!, end: m.index! + m[0].length });
          top()?.task?.needs.push(m[0]);
        }
      }
      return;
    }
    if ((line.match(/"""/g)?.length ?? 0) % 2 === 1) inTriple = true;

    // `} else {` closes a block and opens the next one.
    if (/^\}\s*else\s*\{$/.test(trimmed)) return;
    if (trimmed === "}" || trimmed.startsWith("} ")) {
      const frame = stack.pop();
      if (frame?.task) frame.task.endLine = number;
      if (frame?.template) frame.template.endLine = number;
      return;
    }

    const definition = !stack.length && line.match(DEFINITION);
    if (definition) {
      const [whole, indent, flags, kind, name, parent] = definition;
      const start = line.indexOf(name, indent.length + flags.length + kind.length);
      const span = { line: number, start, end: start + name.length };
      let template: TemplateDef | undefined;
      let task: TaskDef | undefined;
      if (kind === "template") {
        template = { ...span, name, params: [], needs: [], endLine: number };
        index.templates.push(template);
        stack.push({ kind: "template", owner: name, ownerKind: "template", template });
      } else {
        task = {
          ...span,
          name,
          kind: kind as "task" | "subtask",
          parent,
          needs: [],
          manual: /\bmanual\b/.test(flags),
          interactive: /\binteractive\b/.test(flags),
          endLine: number,
        };
        index.tasks.push(task);
        if (parent) {
          const parentStart = line.lastIndexOf(parent);
          index.refs.push({ name: parent, kind: "task", line: number, start: parentStart, end: parentStart + parent.length });
        }
        stack.push({ kind: "task", owner: name, ownerKind: "task", task });
      }
      // The header after the name: a template's parameters, then `needs` and
      // `lock` clauses, up to the `{`, over `\` continued lines too.
      let mode: "params" | "needs" | "lock" = "params";
      let at = number;
      let from = whole.length;
      for (;;) {
        const text = stripComment(lines[at] ?? "");
        const brace = text.indexOf("{", from);
        const segment = text.slice(from, brace < 0 ? text.length : brace);
        for (const m of segment.matchAll(HEADER_WORD)) {
          const word = m[0];
          const wordStart = from + m.index!;
          if (word === "needs" || word === "lock") {
            mode = word;
          } else if (mode === "needs") {
            index.refs.push({ name: word, kind: "task", line: at, start: wordStart, end: wordStart + word.length });
            task?.needs.push(word);
            template?.needs.push(word);
          } else if (mode === "params" && template) {
            const param = word.match(new RegExp(`^(${NAME})(?:=(.*))?$`, "u"));
            if (!param) continue;
            template.params.push(param[1]);
            const value = param[2]?.replace(/^["']|["']$/g, "");
            index.vars.push({ name: param[1], value: value ?? "(parameter)", line: at, start: wordStart, end: wordStart + param[1].length, owner: name });
          }
        }
        if (brace >= 0 || !text.trim().endsWith("\\")) break;
        at++;
        from = 0;
      }
      return;
    }

    const letMatch = line.match(LET);
    if (letMatch) {
      const [, indent, name, value] = letMatch;
      const start = line.indexOf(name, indent.length + 3);
      index.vars.push({ name, value: value.trim(), line: number, start, end: start + name.length, owner: top()?.owner });
      return;
    }

    const keyword = trimmed.match(/^[\p{L}\p{N}_.+-]+/u)?.[0] ?? "";
    const rest = trimmed.slice(keyword.length);
    const frame = top();
    switch (keyword) {
      case "include":
      case "env":
      case "secrets":
      case "age-key": {
        const path = strings(line)[0];
        if (path) index.paths.push({ path: path.value, kind: keyword, line: number, start: path.start, end: path.end });
        return;
      }
      case "desc":
        if (frame?.task) frame.task.desc = strings(line)[0]?.value;
        return;
      case "manual":
        if (frame?.task) frame.task.manual = true;
        return;
      case "interactive":
        if (frame?.task) frame.task.interactive = true;
        return;
      case "needs": {
        const from = line.indexOf("needs") + 5;
        for (const m of line.slice(from).matchAll(WORD)) {
          const start = from + m.index!;
          index.refs.push({ name: m[0], kind: "task", line: number, start, end: start + m[0].length });
          frame?.task?.needs.push(m[0]);
          frame?.template?.needs.push(m[0]);
        }
        return;
      }
      case "run":
      case "undo":
      case "get":
      case "set":
        if (/^\s*\{\s*$/.test(rest)) shellDepth = 1;
        return;
      case "setting":
        if (stack.length && /\{\s*$/.test(rest)) stack.push({ kind: "block", owner: frame?.owner, ownerKind: frame?.ownerKind });
        return;
      case "changed":
        // `changed run <command>`, or `changed run {` with a shell block.
        if (/^\s+run\b/.test(rest)) {
          if (/^\s+run\s*\{\s*$/.test(rest)) shellDepth = 1;
          return;
        }
        if (stack.length && /\{\s*$/.test(rest)) stack.push({ kind: "block", owner: frame?.owner, ownerKind: frame?.ownerKind });
        return;
      case "if":
      case "unless":
      case "ask":
        if (stack.length && /\{\s*$/.test(rest)) stack.push({ kind: "block", owner: frame?.owner, ownerKind: frame?.ownerKind });
        return;
      case "file":
      case "directory":
      case "line": {
        const found = strings(line);
        if (found[0]) index.paths.push({ path: found[0].value, kind: "target", line: number, start: found[0].start, end: found[0].end });
        const base = keyword === "line" && line.match(/\bbase\s+("[^"]*"|'[^']*')/);
        if (base) {
          const start = base.index! + base[0].indexOf(base[1]) + 1;
          index.paths.push({ path: base[1].slice(1, -1), kind: "target", line: number, start, end: start + base[1].length - 2 });
        }
        const from = line.match(/\bfrom\s+("[^"]*"|'[^']*')/);
        if (from) {
          const start = from.index! + from[0].indexOf(from[1]) + 1;
          index.paths.push({ path: from[1].slice(1, -1), kind: "from", line: number, start, end: start + from[1].length - 2 });
        }
        return;
      }
      default:
        if (stack.length && keyword && !KEYWORDS[keyword]) {
          const start = line.indexOf(keyword);
          index.refs.push({ name: keyword, kind: "template", line: number, start, end: start + keyword.length });
        }
    }
  });
  return index;
}

/** The definition or reference under a position. */
export function symbolAt(index: Index, line: number, character: number): { name: string; kind: RefKind; definition: boolean } | undefined {
  const within = (s: Span) => s.line === line && character >= s.start && character <= s.end;
  for (const t of index.tasks) if (within(t)) return { name: t.name, kind: "task", definition: true };
  for (const t of index.templates) if (within(t)) return { name: t.name, kind: "template", definition: true };
  for (const v of index.vars) if (within(v)) return { name: v.name, kind: "var", definition: true };
  for (const r of index.refs) if (within(r)) return { name: r.name, kind: r.kind, definition: false };
  return undefined;
}

/** The variable definition a reference on `line` resolves to: the enclosing task's own first, then top level. */
export function resolveVar(index: Index, name: string, line: number): VarDef | undefined {
  const owner = index.lines[line]?.owner;
  return index.vars.find((v) => v.name === name && v.owner === owner && owner !== undefined) ?? index.vars.find((v) => v.name === name && v.owner === undefined);
}
