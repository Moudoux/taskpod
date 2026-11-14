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
taskpod FILE -r a b        # run tasks a and b, and what they need
taskpod FILE -i a          # what task a would do
taskpod FILE -l            # list tasks
taskpod FILE --check       # check the file for errors
taskpod FILE --secrets s.yaml  # secrets for {{ secrets.name }}, see below
taskpod FILE -s a          # skip task a
taskpod FILE -j 4          # at most 4 tasks at once (default: one per CPU)
taskpod FILE -v            # stream every command and its output
taskpod FILE --plain       # no live display, output prefixed by task
```

Diffs and dry runs run the `if` and `unless` checks (which shouldn't change
anything), compare files byte for byte, mode and owner, and show unified diffs.

While running, each task gets a live spinner showing its latest output above
an overall progress bar; a failed task prints its last output. Questions
(`ask`, `confirm`, `choose`, `input`) pause the display. Commands that use
`sudo` make taskpod authenticate once, through sudo itself so security keys
work, and keep the credentials fresh, so parallel tasks never prompt at the
same time. Tasks marked `interactive` run alone with the terminal.

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

A secret used inside a `run` command is part of that command line, which
other users on the machine can see while it runs; writing it into a file
with `file ... mode 600` avoids that.

## Install

Arch Linux, from [dist/arch/PKGBUILD](dist/arch/PKGBUILD):

```sh
cd dist/arch && makepkg -si
```

Anywhere else with Rust: `cargo install --path .`

Prebuilt Linux and Windows (x86_64) binaries are attached to
[GitHub releases](https://github.com/Moudoux/taskpod/releases), built by CI
for every `v*` tag. Builds of other commits are under the CI run's artifacts.

## VS Code

[editors/vscode](editors/vscode) has an extension with highlighting,
completion, hover docs, go to definition, diagnostics and run buttons. CI
builds it as a `.vsix`, attached to releases; to build it yourself:

```sh
cd editors/vscode && bun install && bun run package
code --install-extension taskpod-*.vsix
```

## JetBrains syntax highlighting

[Task.xml](Task.xml) is a file type for JetBrains IDEs: copy it to the IDE's
`filetypes` config directory.
