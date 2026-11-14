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

## Install

Arch Linux, from [dist/arch/PKGBUILD](dist/arch/PKGBUILD):

```sh
cd dist/arch && makepkg -si
```

Anywhere else with Rust: `cargo install --path .`

Prebuilt Linux and Windows (x86_64) binaries are attached to
[GitHub releases](https://github.com/Moudoux/taskpod/releases), built by CI
for every `v*` tag. Builds of other commits are under the CI run's artifacts.

## My system

`arch/` (gitignored) sets up my Arch install, the taskpod version of my
arch-config scripts: `taskpod arch/main.task -D` to compare, `taskpod arch/main.task`
to apply. Longer config files sit next to it in `arch/files/`.

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
