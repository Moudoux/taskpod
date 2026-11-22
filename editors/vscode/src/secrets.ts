// Where each secret's key is in a sops file. sops encrypts values but leaves
// keys readable, so this works on encrypted files without decrypting them.
// Names match taskpod's: nested keys joined with dots, list items numbered from 0.
// Values are never read.

import * as fs from "node:fs";
import * as path from "node:path";
import type { Span } from "./index";

export interface SecretKey extends Span {
  name: string;
  /** Holds a value, so `{{ secrets.name }}` can use it (not just a parent of other keys). */
  leaf: boolean;
}

/** The keys in a sops file, by its format (from the extension, like sops). */
export function secretKeys(text: string, fileName: string): SecretKey[] {
  const ext = fileName.toLowerCase().split(".").pop() ?? "";
  const keys = ext === "json" ? json(text) : ext === "env" ? dotenv(text) : ext === "ini" ? ini(text) : yaml(text);
  const names = new Set(keys.map((k) => k.name));
  return keys
    .filter((k) => k.name !== "sops" && !k.name.startsWith("sops.") && !(ext === "env" && k.name.startsWith("sops_")))
    .map((k) => ({ ...k, leaf: ![...names].some((n) => n.startsWith(`${k.name}.`)) }));
}

/**
 * The files a `secrets` path names: like taskpod's parser, `*` and `?` in the
 * file name (not the directory) match files there, sorted, and `*` doesn't
 * match a leading dot. A path without them is itself, whether it exists or not.
 */
export function globFiles(file: string): string[] {
  const name = path.basename(file);
  if (!/[*?]/.test(name)) return [file];
  const dir = path.dirname(file);
  const pattern = new RegExp(`^${name.replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*").replace(/\?/g, ".")}$`, "s");
  try {
    return fs
      .readdirSync(dir, { withFileTypes: true })
      .filter((e) => !e.isDirectory() && !e.name.startsWith(".") && pattern.test(e.name))
      .map((e) => path.join(dir, e.name))
      .sort();
  } catch {
    return [];
  }
}

type Key = Omit<SecretKey, "leaf">;

function unquote(key: string): string {
  return /^(["']).*\1$/.test(key) ? key.slice(1, -1) : key;
}

interface Frame {
  indent: number;
  path: string;
  item: boolean;
  items: number;
}

function yaml(text: string): Key[] {
  const keys: Key[] = [];
  const stack: Frame[] = [];
  let block: number | undefined;
  const join = (parent: Frame | undefined, name: string) => (parent ? `${parent.path}.${name}` : name);

  // `key: value` at `col`; returns false if the text isn't a key.
  const mapping = (line: number, col: number, rest: string, owner?: Frame): boolean => {
    const m = rest.match(/^("[^"]*"|'[^']*'|[^\s#:"'][^:#]*?)\s*:(\s+|$)(.*)$/);
    if (!m) return false;
    while (stack.length && stack[stack.length - 1].indent >= col && stack[stack.length - 1] !== owner) stack.pop();
    const name = join(stack[stack.length - 1], unquote(m[1]));
    keys.push({ name, line, start: col, end: col + m[1].length });
    stack.push({ indent: col, path: name, item: false, items: 0 });
    if (/^[|>]/.test(m[3])) block = col;
    return true;
  };

  text.split(/\r?\n/).forEach((raw, line) => {
    const indent = raw.length - raw.trimStart().length;
    const content = raw.trim();
    if (block !== undefined) {
      if (!content || indent > block) return;
      block = undefined;
    }
    if (!content || content.startsWith("#") || content === "---" || content === "...") return;

    const dash = content.match(/^-(\s+|$)/);
    if (!dash) {
      mapping(line, indent, content);
      return;
    }
    // A list item: its parent may be a key at the same indent (`key:` then `- a`).
    while (stack.length) {
      const top = stack[stack.length - 1];
      if (top.indent > indent || (top.indent === indent && top.item)) stack.pop();
      else break;
    }
    const parent = stack[stack.length - 1];
    const index = parent ? parent.items++ : 0;
    const name = join(parent, String(index));
    const itemFrame: Frame = { indent, path: name, item: true, items: 0 };
    stack.push(itemFrame);
    const col = indent + dash[0].length;
    const rest = content.slice(dash[0].length);
    if (!rest || !mapping(line, col, rest, itemFrame)) keys.push({ name, line, start: indent, end: indent + 1 });
  });
  return keys;
}

function json(text: string): Key[] {
  const keys: Key[] = [];
  let pos = 0;
  let line = 0;
  let lineStart = 0;
  const ws = () => {
    while (pos < text.length && /\s/.test(text[pos])) {
      if (text[pos] === "\n") {
        line++;
        lineStart = pos + 1;
      }
      pos++;
    }
  };
  const string = (): string => {
    let out = "";
    pos++;
    while (pos < text.length && text[pos] !== '"') {
      if (text[pos] === "\\") {
        const next = text[pos + 1];
        if (next === "u") {
          out += String.fromCharCode(parseInt(text.slice(pos + 2, pos + 6), 16));
          pos += 6;
          continue;
        }
        out += ({ n: "\n", t: "\t", r: "\r", b: "\b", f: "\f" } as Record<string, string>)[next] ?? next;
        pos += 2;
        continue;
      }
      out += text[pos++];
    }
    pos++;
    return out;
  };
  const value = (path: string, depth: number) => {
    ws();
    if (depth > 64) throw new Error("too deep");
    const join = (k: string) => (path ? `${path}.${k}` : k);
    if (text[pos] === "{") {
      pos++;
      for (;;) {
        ws();
        if (text[pos] === "}") {
          pos++;
          return;
        }
        if (text[pos] !== '"') throw new Error("expected a key");
        const start = pos - lineStart;
        const keyLine = line;
        const key = string();
        keys.push({ name: join(key), line: keyLine, start, end: pos - lineStart });
        ws();
        if (text[pos++] !== ":") throw new Error("expected :");
        value(join(key), depth + 1);
        ws();
        if (text[pos] === ",") pos++;
      }
    }
    if (text[pos] === "[") {
      pos++;
      for (let i = 0; ; i++) {
        ws();
        if (text[pos] === "]") {
          pos++;
          return;
        }
        const start = pos - lineStart;
        keys.push({ name: join(String(i)), line, start, end: start + 1 });
        value(join(String(i)), depth + 1);
        ws();
        if (text[pos] === ",") pos++;
      }
    }
    if (text[pos] === '"') {
      string();
      return;
    }
    const scalar = text.slice(pos).match(/^[^\s,\]}]+/);
    if (!scalar) throw new Error("expected a value");
    pos += scalar[0].length;
  };
  try {
    value("", 0);
  } catch {
    // Keep what was found before the error, the file may be half written.
  }
  return keys;
}

function dotenv(text: string): Key[] {
  const keys: Key[] = [];
  text.split(/\r?\n/).forEach((raw, line) => {
    const m = raw.match(/^(\s*(?:export\s+)?)([^\s#=]+)\s*=/);
    if (m) keys.push({ name: m[2], line, start: m[1].length, end: m[1].length + m[2].length });
  });
  return keys;
}

function ini(text: string): Key[] {
  const keys: Key[] = [];
  let section = "";
  text.split(/\r?\n/).forEach((raw, line) => {
    const header = raw.match(/^(\s*\[)([^\]]+)\]/);
    if (header) {
      section = header[2].trim();
      keys.push({ name: section, line, start: header[1].length, end: header[1].length + header[2].length });
      return;
    }
    const m = raw.match(/^(\s*)([^\s#;=[][^=]*?)\s*=/);
    if (m) keys.push({ name: section ? `${section}.${m[2]}` : m[2], line, start: m[1].length, end: m[1].length + m[2].length });
  });
  return keys;
}
