# Taskpod

Taskpod runs tasks from a declarative `.task` file. Tasks declare what they
need, and everything that can run in parallel does. Checks like
`unless pacman -Q zsh` make tasks idempotent, so running a file again only
does what's missing, and `--diff` shows what that would be, or tells you the
system already matches.

```sh
template pkg {
    lock pacman                 # one package install at a time
    unless pacman -Q {{ args }}
    run yay -S --needed --noconfirm {{ args }}
}

task bluetooth {
    pkg bluez-utils
    unless systemctl is-enabled -q bluetooth
    run sudo systemctl enable --now bluetooth
}

task config {
    file "~/.config/code-flags.conf" "--ozone-platform=wayland\n"
}
```

See [spec.task](spec.task) for the whole syntax, and [build.task](build.task)
for a small real example.

## Usage

```sh
taskpod [FILE]             # run every task that isn't manual (FILE defaults to main.task)
taskpod FILE -D            # diff: compare with the system, show only what would change
taskpod FILE -d            # dry run: every task and step, and what would change
taskpod FILE --pull        # bring changes made on the system back into the task files
taskpod FILE -u            # undo every task; -u -r a: task a and its subtasks; -u -D: only show it
taskpod FILE -H pi@rpi     # configure another machine over SSH
taskpod FILE -r a b        # run tasks a and b, and what they need
taskpod FILE -i a          # what task a would do
taskpod FILE -l            # list tasks
taskpod FILE -g            # task graph: each task with what runs before it (with -r: only those)
taskpod FILE -g dot        # the graph for Graphviz (| dot -Tsvg > tasks.svg), or -g mermaid
taskpod FILE --check       # check the file for errors
taskpod FILE --secrets s.yaml  # secrets for {{ secrets.name }}, see below
taskpod FILE -s a          # skip task a
taskpod FILE -j 4          # at most 4 tasks at once (default: one per CPU)
taskpod FILE -v            # stream every command and its output
taskpod FILE --plain       # no live display, output prefixed by task
taskpod FILE -D --json     # the diff as JSON, for editors
taskpod FILE -D --no-secrets  # don't decrypt: steps that need secrets show as not checked
```

Diffs and dry runs run the `if` and `unless` checks (which shouldn't change
anything), compare files byte for byte, mode and owner (through symlinks, as
writes go), and show unified diffs. When the text is the same, they say what
differs instead: the encoding or byte order mark, line endings, or the final newline.
`--json` prints the result of a diff or dry run (`-d`) as one JSON document
on stdout, with plain text and secrets hidden, instead of the usual output;
it never asks anything. `--no-secrets` keeps a check from decrypting secrets
(no PIN or key touch): a step that needs one is reported as not checked, and
so is a task whose `if`, `unless` or variables need one.

While running, each task gets a live spinner showing its latest output above
an overall progress bar; a failed task prints its last output. Questions
(`ask`, `confirm`, `choose`, `input`) pause the display. Commands that use
`sudo` make taskpod authenticate once, through sudo itself so security keys
work, and keep the credentials fresh, so parallel tasks never prompt at the
same time. Tasks marked `interactive` run alone with the terminal.

## Pulling changes back

Changed a config on the system and want to keep it? `--pull` goes the other
way: it compares every file step with the system (running `if` and `unless`
checks like a diff, but no commands) and brings the system's version back
into your task files, asking about each file. `--pull -D` shows the diffs
without writing anything, `--pull -d` lists every file, and `--pull --yes`
takes everything without asking.

- A `from "path"` file gets the system's copy (decoded to UTF-8 when it has
  an `encoding`). A `directory` step pulls each of its files, and copies files
  that are only on the system into the source directory.
- Inline content is rewritten in the task file. If it uses `{{ variables }}`,
  changed plain lines are pulled and changed lines with a variable are shown
  for you to edit by hand, so values never end up in your task files. Lines
  holding a secret value are never pulled, wherever they are.
- A different mode updates (or adds) the step's `mode`, and a different owner
  the step's `owner` (when it has one). A file in another encoding (a different
  byte order mark, or text the step's encoding can't hold) updates or adds
  `encoding`. Files missing on the system are left alone.

A template's content is shared by every call, so a pulled change applies to
all of them; two calls wanting different content is reported as a conflict.

## Undo

`taskpod main.task --undo` (`-u`) undoes every task, `-u -r name` one task and
its subtasks (not what it needs, which other tasks may use). A task that other
tasks still need isn't undone on its own: taskpod names them, and you add them
to `-r` to undo them too. `-u -D` and `-u -d`
show what it would do. Tasks that need another go first, subtasks before their
parent, and inside a task the steps run last first.

- **Files** go back to what they were before taskpod first wrote them. Before
  each first write, taskpod records the old content, mode and owner (or that
  there was no file) in a journal on the machine it configures:
  `~/.local/state/taskpod/journal`, and `/var/lib/taskpod/journal`, readable by
  root only, for files written as root. Files written before the journal
  existed have no record: mark the ones taskpod created `new` and undo deletes
  them, otherwise undo reports them. A file changed since taskpod wrote it is
  asked about (`--yes` undoes it anyway).
- **Commands** can't be undone by themselves. `undo` says how, like `run`:

  ```
  template pkg {
      unless pacman -Q {{ args }}
      run yay -S --noconfirm {{ args }}
      undo yay -Rns --noconfirm {{ args }}
  }
  ```

  Each `undo` runs where it stands as the steps are walked backwards, so one at
  the end of a task runs before the task's files are removed (`docker compose
  down` before deleting `compose.yml`). A block with commands and no `undo` is
  reported as not undoable, unless it says `keep`: leave it as it is (a system
  update, data directories). `keep` on a file step leaves the file.
- **Lines**: the journal records each line taskpod added or replaced (and
  files it created from a `base`), so undo takes out only those, puts back a
  replaced line, and asks about a line changed since. A line with no record
  is reported and left, unless the step says `new`; `keep` leaves it.
- **Checks**: an `if` still says where a task applies; an `unless` says when
  there's something to undo, since its check passes when the task is done.

## Remote machines

`taskpod main.task --host pi@raspberrypi` (or a `Host` from `~/.ssh/config`)
configures another machine over SSH. Taskpod doesn't need to be installed
there, only `sh` and the usual coreutils. The task file, variables, prompts
and secrets are handled locally, and everything that touches the system runs
remotely: `run` commands, `if`/`unless` checks, `$( )` variables, and file
reads and writes. `~`, `{{ home }}` and `{{ user }}` are the remote ones, and
commands run in the remote home unless a task sets `workdir`. Diffs, dry runs
and `--pull` work the same way.

SSH connects once (asking for a password or a key touch if it needs to), and
every command reuses that connection. Commands are sent over stdin, so they
and the secrets in them don't show up in `ps` of the ssh process, though the
remote shell has them in its command line, like locally. If sudo on the
remote machine needs a password, taskpod asks for it once and keeps it in
memory; each command that runs then gets a `sudo` that's given the password
through an askpass helper in a private temporary directory, removed when the
command ends. Scripts that call `/usr/bin/sudo` by its full path bypass that.
`--host` needs a Unix system to run on.

## Secrets

Secrets live in a [sops](https://github.com/getsops/sops)-encrypted file,
which is safe to commit next to your task files, and are used as
`{{ secrets.name }}`:

```sh
secrets "secrets.yaml"     # or: taskpod main.task --secrets secrets.yaml

task wifi {
    file "/etc/iwd/home.psk" mode 600 sudo """
        [Security]
        Passphrase={{ secrets.wifi.password }}
        """
}
```

Nested keys join with dots. Taskpod runs `sops` the first time a secret is
used, so tasks that don't use secrets, `--check` and `--list` need no key, and
a task that installs sops can run first. Decrypted values stay in memory and
are shown as `••••` in everything taskpod prints, including diffs. Output of
`interactive` tasks goes straight to the terminal and isn't hidden.

To set it up with an [age](https://github.com/FiloSottile/age) key:

```sh
sudo pacman -S sops age
mkdir -p ~/.config/sops/age
age-keygen -o ~/.config/sops/age/keys.txt   # back this file up somewhere safe
# Tell sops which key encrypts files here, using the public key age-keygen printed:
printf 'creation_rules:\n  - age: age1...\n' > .sops.yaml
sops secrets.yaml                           # opens your editor, saves encrypted
```

Or keep the key on a security key, so decrypting needs the key, its PIN and
a touch (asked once per run). Which age plugin depends on the key:
`ykman info` shows whether it has PIV.

- Any FIDO2 key, such as a YubiKey Security Key:
  [age-plugin-fido2-hmac](https://github.com/olastor/age-plugin-fido2-hmac)
  (`yay -S age-plugin-fido2-hmac`). `age-plugin-fido2-hmac -g` prints an
  `age1fido2-hmac1...` recipient; put the identity it prints, or
  `age-plugin-fido2-hmac -m` if you chose no separate identity, in
  `~/.config/sops/age/keys.txt`.
- A YubiKey with PIV (YubiKey 5):
  [age-plugin-yubikey](https://github.com/str4d/age-plugin-yubikey)
  (`pacman -S age-plugin-yubikey`). `age-plugin-yubikey` creates the key and
  prints an `age1yubikey1...` recipient, and
  `age-plugin-yubikey --identity >> ~/.config/sops/age/keys.txt` sets up sops.

Then use the recipient in `.sops.yaml`. Add a second
recipient as a backup, a spare YubiKey or a plain age key kept offline,
separated by commas: losing the only key means losing the secrets.

### Several files, one touch

Each sops file is unlocked on its own, so with a security key, every
secrets file costs a PIN and a touch. To unlock them all at once, make an age
key that's itself encrypted to the security key (plus the backup), and
encrypt the files to it too:

```sh
# Prints the new public key; the private key only ever goes to age, encrypted.
age-keygen | age -r age1fido2-hmac1... -r age1backup... -o taskpod.key.age
# Check: prints the same public key.
age -d -i ~/.config/sops/age/keys.txt taskpod.key.age | age-keygen -y
# Add the new public key to the recipients in .sops.yaml, then for each file:
sops updatekeys -y secrets.yaml
```

Then add `age-key "taskpod.key.age"` to the task file (it's encrypted, so
commit it). Taskpod decrypts it with `age` once, using the identity sops
would use (`SOPS_AGE_KEY_FILE` or `~/.config/sops/age/keys.txt`), keeps it in
memory and gives it to each `sops` in `SOPS_AGE_KEY`, with sops' own key file
hidden so the security key isn't asked again. A file that isn't encrypted to
it yet is unlocked the usual way, with a hint to run `sops updatekeys`.
Since the security key stays a recipient of every file, plain `sops` keeps
working as before.

A secret used inside a `run` command is part of that command line, which
other users on the machine can see while it runs; writing it into a file
with `file ... mode 600` avoids that.

## Install

Arch Linux, from [dist/arch/PKGBUILD](dist/arch/PKGBUILD):

```sh
cd dist/arch && makepkg -si
```

Or with [just](https://github.com/casey/just): `just install` builds and
installs the Arch package and the VS Code extension, `just arch` or
`just vscode` one of them (`just` lists every recipe).

Anywhere else with Rust: `cargo install --path .`

Prebuilt Linux and Windows (x86_64) binaries are attached to
[GitHub releases](https://github.com/Moudoux/taskpod/releases), built by CI
for every `v*` tag.

Latest CI builds of `master` (zip files, through [nightly.link](https://nightly.link),
since GitHub only lets signed-in users download build artifacts):

- [Linux x86_64 binary](https://nightly.link/Moudoux/taskpod/workflows/ci/master/taskpod-x86_64-unknown-linux-gnu.zip)
- [Windows x86_64 binary](https://nightly.link/Moudoux/taskpod/workflows/ci/master/taskpod-x86_64-pc-windows-msvc.zip)
- [VS Code extension (.vsix)](https://nightly.link/Moudoux/taskpod/workflows/ci/master/taskpod-vscode.zip)

Or signed in, from the [latest CI runs](https://github.com/Moudoux/taskpod/actions/workflows/ci.yml?query=branch%3Amaster).

## VS Code

[editors/vscode](editors/vscode) has an extension with highlighting,
completion, hover docs, go to definition, diagnostics, run buttons and live
checks, which show above each task whether this system matches it. CI
builds it as a `.vsix`, attached to releases, and the
[latest build of master](https://nightly.link/Moudoux/taskpod/workflows/ci/master/taskpod-vscode.zip) is a download away; to build it yourself:

```sh
cd editors/vscode && bun install && bun run package
code --install-extension taskpod-*.vsix
```

or `just vscode`, which installs the version just built.

## JetBrains syntax highlighting

[Task.xml](Task.xml) is a file type for JetBrains IDEs: copy it to the IDE's
`filetypes` config directory.
