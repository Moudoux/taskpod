import { expect, test } from "bun:test";
import * as fs from "node:fs";
import * as path from "node:path";
import * as oniguruma from "vscode-oniguruma";
import * as textmate from "vscode-textmate";

const root = path.join(import.meta.dir, "..");
const wasm = fs.readFileSync(path.join(root, "node_modules/vscode-oniguruma/release/onig.wasm"));

async function tokenizer() {
  await oniguruma.loadWASM(wasm.buffer.slice(wasm.byteOffset, wasm.byteOffset + wasm.byteLength) as ArrayBuffer);
  const registry = new textmate.Registry({
    onigLib: Promise.resolve({
      createOnigScanner: (s: string[]) => new oniguruma.OnigScanner(s),
      createOnigString: (s: string) => new oniguruma.OnigString(s),
    }),
    loadGrammar: async (scope) => {
      if (scope === "source.taskpod") {
        const file = path.join(root, "syntaxes/taskpod.tmLanguage.json");
        return textmate.parseRawGrammar(fs.readFileSync(file, "utf8"), file);
      }
      // Stand-in for VS Code's shell grammar.
      return textmate.parseRawGrammar(JSON.stringify({ scopeName: "source.shell", patterns: [] }), "shell.json");
    },
  });
  const grammar = (await registry.loadGrammar("source.taskpod"))!;
  return (text: string) => {
    let stack = textmate.INITIAL;
    const tokens: [string, string][] = [];
    for (const line of text.split("\n")) {
      const result = grammar.tokenizeLine(line, stack);
      for (const t of result.tokens) tokens.push([line.slice(t.startIndex, t.endIndex), t.scopes.join(" ")]);
      stack = result.ruleStack;
    }
    return tokens;
  };
}

const scopeOf = (tokens: [string, string][], text: string) => tokens.find(([t]) => t === text)?.[1] ?? `<no token ${text}>`;

test("highlights the language", async () => {
  const tokenize = await tokenizer();
  const tokens = tokenize(`let name = "pod {{ x }}"   # comment
task build {
    needs lint test
    terminal
    unless test -e out {
        run make {{ name }}
    }
    run {
        if [ -n "\${A}" ]; then echo "}"; fi
    }
    pkg zsh
    file "/etc/x" mode 640 owner root:wheel encoding utf-16le sudo from "files/x"
    file "/etc/y" """
        raw {{ name }}
        """
}
subtask notify of build {
}`);
  expect(scopeOf(tokens, "let")).toContain("storage.type.let");
  expect(scopeOf(tokens, "name")).toContain("variable.other.definition");
  expect(scopeOf(tokens, "x")).toContain("variable.other.readwrite");
  expect(scopeOf(tokens, " comment")).toContain("comment.line");
  expect(scopeOf(tokens, "task")).toContain("storage.type.task");
  expect(scopeOf(tokens, "build")).toContain("entity.name.function");
  expect(scopeOf(tokens, "lint")).toContain("entity.name.function");
  expect(scopeOf(tokens, "terminal")).toContain("storage.modifier");
  expect(scopeOf(tokens, " test -e out")).toContain("meta.embedded.line.shell");
  expect(scopeOf(tokens, "{")).toContain("punctuation.section.block");
  expect(scopeOf(tokens, '        if [ -n "${A}" ]; then echo "}"; fi')).toContain("meta.embedded.block.shell");
  expect(scopeOf(tokens, "pkg")).toContain("entity.name.function.call");
  expect(scopeOf(tokens, "640")).toContain("constant.numeric.octal");
  expect(scopeOf(tokens, "root:wheel")).toContain("variable.other.constant.owner");
  expect(scopeOf(tokens, "utf-16le")).toContain("constant.language.encoding");
  expect(scopeOf(tokens, "        raw ")).toContain("string.quoted.triple");
  expect(scopeOf(tokens, "of")).toContain("keyword.other.of");
  expect(scopeOf(tokens, "notify")).toContain("entity.name.function");
});

test("highlights vars files and the template option", async () => {
  const tokenize = await tokenizer();
  const tokens = tokenize(`vars "versions.yaml" as versions
task t {
    file "/x" template from "files/x"
}`);
  expect(scopeOf(tokens, "vars")).toContain("keyword.control.import");
  expect(scopeOf(tokens, "as")).toContain("entity.other.attribute-name");
  expect(scopeOf(tokens, "versions")).toContain("variable.other.constant.vars");
  expect(scopeOf(tokens, "template")).toContain("entity.other.attribute-name");
});

test("highlights an age-key plugin", async () => {
  const tokenize = await tokenizer();
  const tokens = tokenize(`age-key "k.age" plugin fido2-hmac # comment`);
  expect(scopeOf(tokens, "age-key")).toContain("keyword.control.import");
  expect(scopeOf(tokens, '"k.age"')).toContain("string.quoted");
  expect(scopeOf(tokens, "plugin")).toContain("entity.other.attribute-name");
  expect(scopeOf(tokens, "fido2-hmac")).toContain("variable.other.constant.plugin");
  expect(scopeOf(tokens, " comment")).toContain("comment.line");
});

test("tokenizes the example files without leaking state", async () => {
  const tokenize = await tokenizer();
  for (const file of ["../../spec.task", "../../build.task"]) {
    const tokens = tokenize(fs.readFileSync(path.join(root, file), "utf8"));
    // The last task's closing brace must be back at the top level.
    const last = tokens.filter(([t]) => t === "}").pop()!;
    expect(last[1]).not.toContain("embedded");
    expect(last[1]).not.toContain("string");
  }
});

test("continued template calls stay arguments", async () => {
  const tokenize = await tokenizer();
  const tokens = tokenize("task a {\n    pkg one \\\n        two\n    needs x \\\n        y\n}");
  expect(scopeOf(tokens, "two")).not.toContain("entity.name.function.call");
  expect(scopeOf(tokens, "y")).toContain("entity.name.function");
});

test("highlights undo", async () => {
  const tokenize = await tokenizer();
  const tokens = tokenize(`task app {
    keep
    file "/etc/x" new from "files/x"
    file "/etc/y" keep "y"
    undo rm -f /tmp/marker
    undo {
        systemctl disable --now app
    }
}`);
  expect(scopeOf(tokens, "keep")).toContain("storage.modifier");
  expect(scopeOf(tokens, "new")).toContain("entity.other.attribute-name");
  expect(scopeOf(tokens, "undo")).toContain("support.type.step");
  expect(scopeOf(tokens, " rm -f /tmp/marker")).toContain("meta.embedded.line.shell");
  expect(scopeOf(tokens, "        systemctl disable --now app")).toContain("meta.embedded.block.shell");
  expect(tokens.some(([, s]) => s.includes("entity.name.function.call"))).toBe(false);
});

test("highlights directory and line steps", async () => {
  const tokenize = await tokenizer();
  const tokens = tokenize(`task app {
    directory "/etc/app" sudo from "files/app"
    line "/etc/pam.d/sudo" sudo before "auth" "auth sufficient x"
}`);
  expect(scopeOf(tokens, "directory")).toContain("support.type.step");
  expect(scopeOf(tokens, "line")).toContain("support.type.step");
  expect(scopeOf(tokens, "before")).toContain("entity.other.attribute-name");
  expect(tokens.some(([, s]) => s.includes("entity.name.function.call"))).toBe(false);
});

test("highlights changed as a guard", async () => {
  const tokenize = await tokenizer();
  const tokens = tokenize(`task app {
    file "app.conf" "x"
    changed {
        run restart
    }
}`);
  expect(scopeOf(tokens, "changed")).toContain("keyword.control.conditional");
  expect(tokens.some(([, s]) => s.includes("entity.name.function.call"))).toBe(false);
});

test("highlights a directory without from", async () => {
  const tokenize = await tokenizer();
  const tokens = tokenize(`task app {
    directory "/var/lib/app" sudo keep mode 750 owner app:app
}`);
  expect(scopeOf(tokens, "directory")).toContain("support.type.step");
  expect(scopeOf(tokens, "keep")).toContain("entity.other.attribute-name");
  expect(tokens.some(([, s]) => s.includes("entity.name.function.call"))).toBe(false);
});

test("highlights notice as a keyword with a string", async () => {
  const tokens = (await tokenizer())(`task t {
    unless test -d /mnt {
        notice "not mounted"
    }
}`);
  expect(scopeOf(tokens, "notice")).toContain("keyword.other.directive");
  expect(scopeOf(tokens, "notice")).not.toContain("entity.name.function.call");
});

test("highlights else, settings, downloads, parameters and changed run", async () => {
  const tokens = (await tokenizer())(`template github(repo, path=".") {
    run echo {{ repo }}
}
task t {
    if test -e x {
        run a
    } else {
        changed run restart
    }
    setting on {
        get cat state
        set echo {{ value }} > state
    }
    file "/etc/k" url "https://e/k" sha256 "00"
}`);
  expect(scopeOf(tokens, "repo")).toContain("variable.parameter");
  expect(scopeOf(tokens, "path")).toContain("variable.parameter");
  expect(scopeOf(tokens, "else")).toContain("keyword.control.conditional");
  expect(scopeOf(tokens, "changed")).toContain("keyword.control.conditional");
  expect(scopeOf(tokens, " restart")).toContain("meta.embedded.line.shell");
  expect(scopeOf(tokens, "setting")).toContain("support.type.step");
  expect(scopeOf(tokens, "get")).toContain("support.type.step");
  expect(scopeOf(tokens, "set")).toContain("support.type.step");
  expect(scopeOf(tokens, "url")).toContain("entity.other.attribute-name");
  expect(scopeOf(tokens, "sha256")).toContain("entity.other.attribute-name");
  expect(tokens.some(([, s]) => s.includes("entity.name.function.call"))).toBe(false);
});

test("highlights flags, needs and locks in definition headers", async () => {
  const tokens = (await tokenizer())(`sudo interactive task update-aur needs yay \\
        base lock pacman {
    run true
}
sudo template pkg(names) needs yay {
}`);
  expect(scopeOf(tokens, "sudo interactive ")).toContain("storage.modifier");
  expect(scopeOf(tokens, "task")).toContain("storage.type.task");
  expect(scopeOf(tokens, "update-aur")).toContain("entity.name.function");
  expect(scopeOf(tokens, "needs")).toContain("keyword.other.needs");
  expect(scopeOf(tokens, "yay")).toContain("entity.name.function");
  expect(scopeOf(tokens, "base")).toContain("entity.name.function");
  expect(scopeOf(tokens, "lock")).toContain("keyword.other.lock");
  expect(scopeOf(tokens, "pacman")).toContain("variable.other.constant.lock");
  expect(scopeOf(tokens, "names")).toContain("variable.parameter");
  expect(scopeOf(tokens, " true")).toContain("meta.embedded.line.shell");
  const last = tokens.filter(([t]) => t === "}").pop()!;
  expect(last[1]).not.toContain("meta.");
});

test("tells control flow, steps, flags and options apart", async () => {
  const tokens = (await tokenizer())(`task t {
    sudo
    desc "d"
    unless test -e x
    run make
    file "/etc/x" mode 600 from "x"
    setting "s" default "old" keep {
        get cat s
    }
}`);
  expect(scopeOf(tokens, "sudo")).toContain("storage.modifier");
  expect(scopeOf(tokens, "desc")).toContain("keyword.other.directive");
  expect(scopeOf(tokens, "unless")).toContain("keyword.control.conditional");
  expect(scopeOf(tokens, "run")).toContain("support.type.step");
  expect(scopeOf(tokens, "file")).toContain("support.type.step");
  expect(scopeOf(tokens, "mode")).toContain("entity.other.attribute-name");
  expect(scopeOf(tokens, "from")).toContain("entity.other.attribute-name");
  expect(scopeOf(tokens, "setting")).toContain("support.type.step");
  expect(scopeOf(tokens, "default")).toContain("entity.other.attribute-name");
  expect(scopeOf(tokens, "keep")).toContain("entity.other.attribute-name");
  expect(scopeOf(tokens, "old")).toContain("string.quoted");
  expect(tokens.some(([, s]) => s.includes("entity.name.function.call"))).toBe(false);
});

test("highlights built-in checks and waits", async () => {
  const tokens = (await tokenizer())(`task t {
    unless exists "~/x" {
    }
    if which tool
    if $(docker context show) == "rootless" {
    }
    unless $(x) has-line 'y'
    if $(cmd) args
    wait "Insert the key" timeout 30 until $(fido2-token -L) != ""
    wait until exists "/run/a"
    after
}`);
  expect(scopeOf(tokens, "exists")).toContain("support.function.check");
  expect(scopeOf(tokens, "~/x")).toContain("string.quoted");
  expect(scopeOf(tokens, "which")).toContain("support.function.check");
  expect(scopeOf(tokens, "docker context show")).toContain("meta.embedded.line.shell");
  expect(scopeOf(tokens, "==")).toContain("keyword.operator.comparison");
  expect(scopeOf(tokens, "rootless")).toContain("string.quoted");
  expect(scopeOf(tokens, "has-line")).toContain("keyword.operator.comparison");
  expect(scopeOf(tokens, "wait")).toContain("support.type.step");
  expect(scopeOf(tokens, "Insert the key")).toContain("string.quoted");
  expect(scopeOf(tokens, "timeout")).toContain("entity.other.attribute-name");
  expect(scopeOf(tokens, "until")).toContain("keyword.control.conditional");
  expect(scopeOf(tokens, "!=")).toContain("keyword.operator.comparison");
  expect(scopeOf(tokens, "/run/a")).toContain("string.quoted");
  // `$(cmd) args` is shell, and doesn't run on past its line.
  expect(tokens.some(([t, s]) => t.includes("cmd) args") && s.includes("meta.embedded.line.shell"))).toBe(true);
  expect(scopeOf(tokens, "after")).toContain("entity.name.function.call");
  const last = tokens.filter(([t]) => t === "}").pop()!;
  expect(last[1]).not.toContain("meta.");
});

test("highlights call blocks and lines blocks", async () => {
  const tokens = (await tokenizer())(`task t "Does t" {
    display {{ oled }} {
        mode 2560x1440@280
    }
    lines "/etc/pacman.conf" sudo {
        match "#Color" "Color"
    }
    run true
}`);
  expect(scopeOf(tokens, "display")).toContain("entity.name.function.call");
  expect(scopeOf(tokens, "        mode 2560x1440@280")).not.toContain("entity.name.function.call");
  expect(scopeOf(tokens, "lines")).toContain("support.type.step");
  expect(scopeOf(tokens, "match")).toContain("entity.other.attribute-name");
  expect(scopeOf(tokens, " true")).toContain("meta.embedded.line.shell");
});
