// Documentation for taskpod keywords, shown on hover and in completions.

/** Where a keyword goes: the top level, a task body, or the options of a `file` or `line` step. */
export type Place = "top" | "body" | "file" | "line";

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
    summary:
      "Defines a task. Tasks run in parallel unless `needs` orders them. Flags (`sudo`, `terminal`, `keep`, `manual`, `interactive`) can go before `task`, and `needs` and `lock` after the name, or all of them in the body.",
    syntax: 'task name {\n    desc "What it does"\n    run command\n}\n\nsudo interactive task update-aur needs yay lock pacman {\n    run yay -Sua\n}',
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
      "Reusable steps, called by name inside tasks: `name words...`. Inside, `{{ args }}` is all the words and `{{ 1 }}`, `{{ 2 }}`... each one. Name the words after the template's name (`name=\"default\"` for optional ones, last), and calls with too few or too many are an error. A template's guards only apply to that call. Flags can go before `template`, and `needs` and `lock` after the parameters.",
    syntax: 'template pkg {\n    unless pacman -Q {{ args }}\n    run yay -S --needed {{ args }}\n}\n\ntemplate greet who greeting="hello" {\n    run echo "{{ greeting }}, {{ who }}"\n}',
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
      "An age identity, itself encrypted (to a security key, say), that the `secrets` files are also encrypted to. It's decrypted once per run with `age`, and every sops file is decrypted with it, so several files need one PIN and touch. Files not encrypted to it still decrypt the usual way. It's decrypted with sops' identity file, or with `plugin NAME`, by that age plugin alone (`age -d -j NAME`), with no identity file. `taskpod --age-key FILE [--age-plugin NAME]` works too.",
    syntax: 'age-key "taskpod.key.age" [plugin fido2-hmac]',
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
      "Tasks that must finish first. They run either way (unless a guard skips them), but if one fails, this task doesn't run. In the body, or in the header: `task name needs base files {`.",
    syntax: "needs base files",
    snippet: "needs ${1:task}",
  },
  if: {
    places: ["top", "body"],
    summary:
      "Run only if the check passes: a shell command that succeeds, or a built-in check (`exists \"path\"`, `which name`, `$(command) == \"value\"`, also `!=`, `contains` and `has-line`). In a task, it applies to the task; ending the line with `{` makes a block, so it only applies to the steps inside.",
    syntax: 'if lsusb | grep Logitech\n\nunless exists "~/.oh-my-zsh" {\n    run ...\n}\n\nif $(yay -Qua) != ""',
    snippet: "if ${1:check}",
  },
  unless: {
    places: ["top", "body"],
    summary:
      "Skip if the check passes (a shell command that succeeds, or a built-in check, see `if`): the usual way to make a step idempotent. End the line with `{` to guard only the steps in the block.",
    syntax: 'unless pacman -Q zsh\n\nunless $(docker context show) == "rootless" {\n    run docker context use rootless\n}',
    snippet: "unless ${1:check}",
  },
  ask: {
    places: ["top", "body"],
    summary:
      "Ask a yes or no question first; no skips. Dry runs and diffs assume yes. End the line with `{` to only guard a block.",
    syntax: 'ask "Install games?"',
    snippet: 'ask "${1:Question?}"',
  },
  changed: {
    places: ["body"],
    summary:
      "Only if an earlier step in this task changed something, like restarting a service after its config file. `changed {` guards a block, `changed run <command>` is a block with one command; in a template, a `changed` line makes the call wait for a change before it in the calling task. In undo, one with only `run` commands (no `undo` or `keep`) runs again once the files, lines and settings right before it are undone (back to the command or template call before them), if that changed something; one with an `undo` runs that where it stands. A subtask does the same as its own task.",
    syntax: 'file "/etc/app.conf" sudo from "files/app.conf"\nchanged run sudo systemctl try-restart app',
    snippet: "changed {\n\t$0\n}",
  },
  else: {
    places: [],
    summary:
      "After an `if` block: runs when its check fails. The check runs once, so the first block can't change which one runs. For `unless X { A } else { B }`, write `if X { B } else { A }`.",
    syntax: 'if mountpoint -q /mnt/data {\n    directory "/mnt/data/app" keep\n} else {\n    notice "/mnt/data isn\'t mounted"\n}',
  },
  setting: {
    places: ["body"],
    summary:
      "A value commands read and write. `get` prints it (compared trimmed); when it's not the value, `set` runs with it as `{{ value }}`, and then `get` must print it. Diffs show the old and new value, and `--undo` sets the value from before taskpod first set it, from the journal (`keep`: undo leaves it). `default \"x\"` is what undo sets when the journal has no record.",
    syntax: 'setting "org.kde.dolphin.desktop" {\n    get xdg-mime query default inode/directory\n    set xdg-mime default {{ value }} inode/directory\n}',
    snippet: 'setting "${1:value}" {\n\tget ${2:command}\n\tset ${3:command {{ value \\}\\}}}\n}',
  },
  default: {
    places: [],
    summary: "In a `setting`: what `--undo` sets when the journal has no record of the value before taskpod, like for one set before taskpod kept them.",
    syntax: 'setting "us,se" {\n    default "us"\n    ...\n}',
  },
  get: {
    places: [],
    summary: "In a `setting`: the shell command that prints its current value.",
    syntax: "get tailscale debug prefs | jq .ShieldsUp",
  },
  set: {
    places: [],
    summary: "In a `setting`: the shell command that sets it to `{{ value }}`.",
    syntax: "set sudo tailscale set --shields-up={{ value }}",
  },
  lock: {
    places: ["body"],
    summary:
      "Tasks holding the same lock never run at the same time, e.g. `lock pacman` for package installs. While waiting, a task doesn't take up a job slot. In the body, or in the header: `task name lock pacman {`.",
    syntax: "lock pacman",
    snippet: "lock ${1:name}",
  },
  sudo: {
    places: ["top", "body", "file", "line"],
    summary:
      "Ask for sudo once before running, so parallel tasks never prompt at the same time. Commands that call `sudo` do this automatically. Implies `terminal`. On a `file` or `line`, reads and writes the file as root. Can also go before `task`, `subtask` or `template`.",
    syntax: "sudo",
    snippet: "sudo",
  },
  terminal: {
    places: ["top", "body"],
    summary:
      "A command that asks on the terminal anyway (a tool running `sudo -k`, doas, su, a security key touch, a question) gets the terminal when it asks: the live display pauses until it ends, and tasks that ask at once take turns. Without this (or `sudo`, or `interactive`), such a command is stopped and the step fails instead of waiting out of sight. Can also go before `task`, `subtask` or `template`.",
    syntax: "terminal",
    snippet: "terminal",
  },
  manual: {
    places: ["top", "body"],
    summary: "Only runs when selected with `--run`, or when another task needs it. Can also go before `task`, `subtask` or `template`.",
    syntax: "manual",
    snippet: "manual",
  },
  interactive: {
    places: ["top", "body"],
    summary:
      "The task gets the terminal: nothing else runs alongside it, the live display pauses, and its commands can ask questions. Can also go before `task`, `subtask` or `template`.",
    syntax: "interactive",
    snippet: "interactive",
  },
  exists: {
    places: [],
    summary:
      "A built-in check: something is at the path (a file, directory or other), on the machine being configured, through symlinks. `~/` is the home directory there; relative paths are from the workdir. No shell is involved.",
    syntax: 'unless exists "~/.oh-my-zsh"',
  },
  which: {
    places: [],
    summary:
      "A built-in check: a program of that name is on the PATH (an `env` file's PATH counts), or with a `/`, the file is executable. Over `--host`, the remote shell's `command -v`.",
    syntax: "if which checkupdates",
  },
  contains: {
    places: [],
    summary: "In a check: what the command prints contains the text. See `if`.",
    syntax: '$(tailscale serve status) contains "(svc:git)"',
  },
  "has-line": {
    places: [],
    summary: "In a check: one of the lines the command prints is the text, ignoring spaces at either end. See `if`.",
    syntax: '$(flatpak remotes --user --columns=name) has-line "flathub"',
  },
  wait: {
    places: ["body"],
    summary:
      "Waits until the check passes (any `if` check), checking every second, with the message on the task's line meanwhile; `timeout` gives up after that many seconds, failing the step. If the check already passes, it's done. Dry runs and diffs never wait, and `--undo` and `--pull` skip it. It changes nothing, and the task gives its job slot back while it waits.",
    syntax: 'wait "Insert the security key" until $(fido2-token -L) != ""\nwait timeout 30 until exists "/run/app.sock"',
    snippet: 'wait "${1:message}" until ${2:check}',
  },
  until: {
    places: [],
    summary: "In a `wait`: the check to wait for.",
    syntax: 'wait until exists "/run/app.sock"',
  },
  timeout: {
    places: [],
    summary: "In a `wait`: give up (failing the step) after this many seconds.",
    syntax: 'wait "Starting" timeout 30 until exists "/run/app.sock"',
  },
  workdir: {
    places: ["body"],
    summary:
      "Where commands and checks run. In a task, relative to the task file (default: its directory). In a template or inline block, where its steps run, relative to the directory around it; its own guards are checked outside it, since they say whether to go in.",
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
  notice: {
    places: ["body"],
    summary:
      "A message for whoever runs taskpod, shown under the task and again above the final summary, so it isn't lost like a command's output. It changes nothing: a task with only notices is still up to date, and `--undo` and `--pull` skip it. Put it in a guarded block to say why something can't be done yet.",
    syntax: 'unless mountpoint -q /mnt/data {\n    notice "/mnt/data isn\'t mounted"\n}',
    snippet: 'notice "${1:text}"',
  },
  keep: {
    places: ["top", "body", "file", "line"],
    summary:
      "`--undo` leaves what this task or block did in place, and doesn't count its commands as impossible to undo. On a `file`, undo leaves the file as it is; on a `line`, the line stays. Can also go before `task`, `subtask` or `template`.",
    syntax: "keep",
    snippet: "keep",
  },
  new: {
    places: ["file", "line"],
    summary:
      "The file didn't exist before taskpod, so `--undo` deletes it; on a `line`, taskpod added the line. Only needed for what taskpod wrote before it kept an undo journal: since then, undo knows what was there.",
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
  directory: {
    places: ["body"],
    summary:
      "Copies every file in a `from` directory (subdirectories included) to the same place under the path, each like its own `file` step with the same options: written only when it differs, pulled back with `--pull`, undone with `--undo`. Files on the system that the directory doesn't have are left alone, and `--pull` copies them into it: only from directories the source has too, and not ones another step of the task writes. Without `from`, only the directory itself: made if it's missing, with `mode` (default 755) and `owner`; undo deletes one taskpod made once it's empty.",
    syntax: 'directory "/etc/opensnitchd/rules" sudo new from "files/etc/opensnitchd/rules"\ndirectory "/var/lib/app" sudo keep mode 750 owner app:app',
    snippet: 'directory "${1:path}" from "${2:files/path}"',
  },
  line: {
    places: ["body"],
    summary:
      "Makes sure a file has this line, leaving the rest of it, its mode and owner as they are: for files a package or you own, like PAM configs or `~/.bashrc`. Missing, the line goes before the first other line starting with `before`, after the last starting with `after` (both ignoring indentation), or at the end, and on the wrong side it's moved. One on the right side counts first, so the same line can be elsewhere too (an `Include` in each of pacman.conf's sections); several on the wrong side stay, and it's added. Lines are compared ignoring spacing; with `match`, a line starting with that is replaced. The file must exist, unless a `base` says what to start from. The journal records what taskpod added or replaced, so `--undo` puts back only that; a line with no record is left, unless the step says `new`.",
    syntax: 'line "/etc/pam.d/sudo" sudo before "auth" "auth sufficient pam_u2f.so cue"\nline "~/.bashrc" ". ~/.scripts/functions.sh"',
    snippet: 'line "${1:path}" "${2:text}"',
  },
  base: {
    places: ["line"],
    summary:
      "When the file doesn't exist, start from a copy of this one (on the machine being configured), with its mode and owner: for overrides of vendor files, like `/etc/pam.d/kde` over `/usr/lib/pam.d/kde`. `--undo` deletes the file once it's the same as the base again.",
    syntax: 'base "/usr/lib/pam.d/kde"',
    snippet: 'base "${1:path}"',
  },
  match: {
    places: ["line"],
    summary:
      "A line starting with this (ignoring indentation) is this step's line: it's replaced with the step's text instead of a second line being added, and `--undo` puts back what it replaced. The line taskpod last wrote for the step counts too, so `match \"#Option\"` still finds a commented-out default after replacing it.",
    syntax: 'match "auth sufficient pam_u2f.so"',
    snippet: 'match "${1:start}"',
  },
  before: {
    places: ["line"],
    summary: "Add the line before the first line starting with this text (ignoring indentation). If none does, the step fails.",
    syntax: 'before "auth"',
    snippet: 'before "${1:start}"',
  },
  after: {
    places: ["line"],
    summary: "Add the line after the last line starting with this text (ignoring indentation). If none does, the step fails.",
    syntax: 'after "[section]"',
    snippet: 'after "${1:start}"',
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
  url: {
    places: ["file"],
    summary:
      "Download the content with curl, on the machine running taskpod, and check it against the `sha256` that must follow. When the file on the system already has that sha256, nothing is downloaded. `--pull` leaves it alone.",
    syntax: 'file "/etc/apt/keyrings/docker.asc" sudo new url "https://download.docker.com/linux/debian/gpg" sha256 "..."',
    snippet: 'url "${1:https://}" sha256 "${2}"',
  },
  sha256: {
    places: [],
    summary: "The sha256 of a downloaded file, 64 hexadecimal digits. A download with another one fails.",
    syntax: 'url "https://example.com/key.asc" sha256 "..."',
  },
  from: {
    places: ["file"],
    summary:
      "Copy the content from a file, relative to this task file, or for `directory`, every file in a directory. Ctrl+click to open a file.",
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
  tmp: "A new empty directory on the machine being configured, made when first used: one for the task (its inline blocks share it), and one for each template call. Deleted once that ends.",
  value: "In a `setting`'s `set` command: the value to set.",
};

export const ENCODINGS = ["utf-8", "utf-8-bom", "utf-16le", "utf-16be", "latin1", "ascii"];

export function keywordMarkdown(word: string): string | undefined {
  const doc = KEYWORDS[word];
  if (!doc) return undefined;
  return `**${word}**: ${doc.summary}\n\n\`\`\`taskpod\n${doc.syntax}\n\`\`\``;
}
