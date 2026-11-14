// Documentation for taskpod keywords, shown on hover and in completions.

export type Place = "top" | "body" | "file";

export interface KeywordDoc {
  /** Where the keyword can be used. */
  places: Place[];
  summary: string;
  syntax: string;
  /** Completion snippet, in VS Code snippet syntax. */
  snippet?: string;
}

export const KEYWORDS: Record<string, KeywordDoc> = {
  task: {
    places: ["top"],
    summary: "Defines a task. Tasks run in parallel unless `needs` orders them.",
    syntax: "task name {\n    desc \"What it does\"\n    run command\n}",
    snippet: "task ${1:name} {\n\t$0\n}",
  },
  subtask: {
    places: ["top"],
    summary:
      "A task that runs after its parent, and only if the parent changed something (not when it was skipped or already up to date).",
    syntax: "subtask name of parent {\n    run command\n}",
    snippet: "subtask ${1:name} of ${2:parent} {\n\t$0\n}",
  },
  template: {
    places: ["top"],
    summary:
      "Reusable steps, called by name inside tasks: `name words...`. Inside, `{{ args }}` is all the words and `{{ 1 }}`, `{{ 2 }}`... each one. A template's guards only apply to that call.",
    syntax: "template pkg {\n    unless pacman -Q {{ args }}\n    run yay -S --needed {{ args }}\n}",
    snippet: "template ${1:name} {\n\t$0\n}",
  },
  let: {
    places: ["top", "body"],
    summary:
      "Defines a variable, used anywhere as `{{ name }}`. Values: a string, a bare word, `$(command)` (shell output, run once when first used), or a question: `confirm`, `choose` or `input`.",
    syntax:
      'let name = "text {{ other }}"\nlet version = $(git describe)\nlet games = confirm "Install games?"     # true or false\nlet desktop = choose "Desktop?" kde gnome\nlet host = input "Hostname" "archbox"   # with a default',
    snippet: "let ${1:name} = ${2:\"value\"}",
  },
  env: {
    places: ["top"],
    summary:
      "Loads `KEY=value` lines from a file. The values become variables and are exported to every command. Blank lines and `#` comments are ignored.",
    syntax: 'env ".env"',
    snippet: 'env "${1:.env}"',
  },
  secrets: {
    places: ["top"],
    summary:
      "Reads secrets from a sops-encrypted file (YAML, JSON, .env or INI), used as `{{ secrets.name }}`; nested keys join with dots: `{{ secrets.wifi.password }}`. Decrypted with `sops` when a secret is first used, and hidden from output. `taskpod --secrets FILE` adds files too.",
    syntax: 'secrets "secrets.yaml"',
    snippet: 'secrets "${1:secrets.yaml}"',
  },
  "age-key": {
    places: ["top"],
    summary:
      "An age identity, itself encrypted (to a security key, say), that the `secrets` files are also encrypted to. It's decrypted once per run with `age`, and every sops file is decrypted with it, so several files need one PIN and touch. Files not encrypted to it still decrypt the usual way. `taskpod --age-key FILE` works too.",
    syntax: 'age-key "taskpod.key.age"',
    snippet: 'age-key "${1:taskpod.key.age}"',
  },
  include: {
    places: ["top"],
    summary:
      "Merges another task file. The path is relative to this file. An optional `if` or `unless` check decides whether to include it.",
    syntax: 'include "desktop.task" if pacman -Q plasma-workspace',
    snippet: 'include "${1:file.task}"',
  },
  shell: {
    places: ["top"],
    summary: "The shell that runs commands, the script is passed as its last argument. Default `sh -ec`.",
    syntax: 'shell "bash -eo pipefail -c"',
    snippet: 'shell "${1:bash -eo pipefail -c}"',
  },
  desc: {
    places: ["body"],
    summary: "A description, shown when the task runs and in `taskpod --list`.",
    syntax: 'desc "Install the desktop"',
    snippet: 'desc "${1}"',
  },
  needs: {
    places: ["body"],
    summary:
      "Tasks that must finish first. They run either way (unless a guard skips them), but if one fails, this task doesn't run.",
    syntax: "needs base files",
    snippet: "needs ${1:task}",
  },
  if: {
    places: ["top", "body"],
    summary:
      "Run only if the shell command succeeds. In a task, it applies to the task; ending the line with `{` makes a block, so it only applies to the steps inside.",
    syntax: "if lsusb | grep Logitech\n\nunless test -d ~/.oh-my-zsh {\n    run ...\n}",
    snippet: "if ${1:command}",
  },
  unless: {
    places: ["top", "body"],
    summary:
      "Skip if the shell command succeeds: the usual way to make a step idempotent. End the line with `{` to guard only the steps in the block.",
    syntax: "unless pacman -Q zsh\n\nunless systemctl is-enabled -q cups {\n    run sudo systemctl enable --now cups\n}",
    snippet: "unless ${1:command}",
  },
  ask: {
    places: ["top", "body"],
    summary:
      "Ask a yes or no question first; no skips. Dry runs and diffs assume yes. End the line with `{` to only guard a block.",
    syntax: 'ask "Install games?"',
    snippet: 'ask "${1:Question?}"',
  },
  lock: {
    places: ["body"],
    summary:
      "Tasks holding the same lock never run at the same time, e.g. `lock pacman` for package installs. While waiting, a task doesn't take up a job slot.",
    syntax: "lock pacman",
    snippet: "lock ${1:name}",
  },
  sudo: {
    places: ["body", "file"],
    summary:
      "Ask for sudo once before running, so parallel tasks never prompt at the same time. Commands that call `sudo` do this automatically. On a `file`, writes the file as root.",
    syntax: "sudo",
    snippet: "sudo",
  },
  manual: {
    places: ["body"],
    summary: "Only runs when selected with `--run`, or when another task needs it.",
    syntax: "manual",
    snippet: "manual",
  },
  interactive: {
    places: ["body"],
    summary:
      "The task gets the terminal: nothing else runs alongside it, the live display pauses, and its commands can ask questions.",
    syntax: "interactive",
    snippet: "interactive",
  },
  workdir: {
    places: ["body"],
    summary: "Where the task's commands run, relative to the task file. Default: the task file's directory.",
    syntax: 'workdir "build"',
    snippet: 'workdir "${1:dir}"',
  },
  run: {
    places: ["body"],
    summary:
      "A shell command, passed to the shell as written. `run {` starts a block that runs as one script, so `cd` and shell variables carry over; it ends with `}` at the same indentation.",
    syntax: "run cargo build --release\n\nrun {\n    cd build\n    make\n}",
    snippet: "run ${1:command}",
  },
  undo: {
    places: ["body"],
    summary:
      "How `taskpod --undo` undoes this task, template or block: a shell command, or an `undo {` block, like `run`. Normal runs skip it. Undo goes through the steps last first, running each `undo` where it stands, so one at the end runs before the files above it are removed. In undo, an `unless` check says when there's something to undo (it passes when the task is done), and an `if` still says where it applies.",
    syntax: "template pkg {\n    unless pacman -Q {{ args }}\n    run yay -S {{ args }}\n    undo yay -Rns {{ args }}\n}",
    snippet: "undo ${1:command}",
  },
  keep: {
    places: ["body", "file"],
    summary:
      "`--undo` leaves what this task or block did in place, and doesn't count its commands as impossible to undo. On a `file`, undo leaves the file as it is.",
    syntax: "keep",
    snippet: "keep",
  },
  new: {
    places: ["file"],
    summary:
      "The file didn't exist before taskpod, so `--undo` deletes it. Only needed for files written before taskpod kept an undo journal: since then, undo knows what was there.",
    syntax: 'file "~/.config/app.conf" new from "files/app.conf"',
    snippet: "new",
  },
  file: {
    places: ["body"],
    summary:
      "Writes a file, only when its content, mode or owner differs. Content is a string, a `\"\"\"` block, or `from \"path\"` (relative to this task file). Mode defaults to 644, or 755 for content starting with `#!`.",
    syntax:
      'file "/etc/x.conf" sudo """\n    contents\n    """\nfile "~/.config/y" mode 600 owner me:me encoding utf-16le from "files/y"',
    snippet: 'file "${1:path}" """\n\t$0\n\t"""',
  },
  mode: {
    places: ["file"],
    summary: "Permission bits in octal. Default 644, or 755 for content starting with `#!`.",
    syntax: "mode 600",
    snippet: "mode ${1:644}",
  },
  owner: {
    places: ["file"],
    summary: "Owner as `user` or `user:group`. Checked against the current file, and set with `sudo install`.",
    syntax: "owner root:root",
    snippet: "owner ${1:root}",
  },
  encoding: {
    places: ["file"],
    summary:
      "How the text is written: `utf-8` (default), `utf-8-bom`, `utf-16le`, `utf-16be` (both with a byte order mark), `latin1` or `ascii`. Files copied with `from` are written byte for byte unless an encoding is given.",
    syntax: "encoding utf-16le",
    snippet: "encoding ${1|utf-8,utf-8-bom,utf-16le,utf-16be,latin1,ascii|}",
  },
  from: {
    places: ["file"],
    summary: "Copy the content from a file, relative to this task file. Ctrl+click to open it.",
    syntax: 'from "files/etc/logid.cfg"',
    snippet: 'from "${1:path}"',
  },
  of: {
    places: [],
    summary: "Names a subtask's parent: `subtask name of parent`.",
    syntax: "subtask notify of build { ... }",
  },
  choose: {
    places: [],
    summary: "Asks to pick one of the options, the first one is used without a terminal.",
    syntax: 'let desktop = choose "Which desktop?" kde gnome "none at all"',
  },
  input: {
    places: [],
    summary: "Asks for text, with an optional default that's also used without a terminal.",
    syntax: 'let host = input "Hostname" "archbox"',
  },
  confirm: {
    places: [],
    summary: "Asks yes or no. The value is `true` or `false`, so `if {{ name }}` works as a check.",
    syntax: 'let games = confirm "Install games?"',
  },
};

export const BUILTIN_VARS: Record<string, string> = {
  home: "Your home directory.",
  user: "Your user name.",
  pwd: "The directory taskpod was started in.",
  dir: "The directory of the main task file.",
  args: "In a template: every word it was called with.",
};

export const ENCODINGS = ["utf-8", "utf-8-bom", "utf-16le", "utf-16be", "latin1", "ascii"];

export function keywordMarkdown(word: string): string | undefined {
  const doc = KEYWORDS[word];
  if (!doc) return undefined;
  return `**${word}**: ${doc.summary}\n\n\`\`\`taskpod\n${doc.syntax}\n\`\`\``;
}
