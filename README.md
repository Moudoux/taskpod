# Taskpod

Taskpod runs tasks from a declarative `.task` file. Tasks declare what they
need, and everything that can run in parallel does. Checks like
`unless pacman -Q zsh` make tasks idempotent, so running a file again only
does what's missing, and `--dry-run` shows what that would be.

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
taskpod FILE -d            # dry run: run the checks, show what would change
taskpod FILE -r a b        # run tasks a and b, and what they need
taskpod FILE -i a          # what task a would do
taskpod FILE -l            # list tasks
taskpod FILE -s a          # skip task a
taskpod FILE -j1           # one task at a time, commands get the terminal directly
taskpod FILE -v            # show commands and up to date steps
```

Output from tasks running in parallel is prefixed with the task name. Commands
that use `sudo` make taskpod authenticate once up front and keep the
credentials fresh, so parallel tasks never ask for a password at the same time.
Tasks marked `interactive` run alone with the terminal.

## Install

Arch Linux, from [arch/PKGBUILD](arch/PKGBUILD):

```sh
cd arch && makepkg -si
```

Anywhere else with Rust: `cargo install --path .`

Prebuilt Linux and Windows (x86_64) binaries are attached to
[GitHub releases](https://github.com/Moudoux/taskpod/releases), built by CI
for every `v*` tag. Builds of other commits are under the CI run's artifacts.

## My system

`arch.task` (gitignored) sets up my Arch install, the taskpod version of my
arch-config scripts: `taskpod arch.task -d` to check, `taskpod arch.task` to apply.

## JetBrains syntax highlighting

[Task.xml](Task.xml) is a file type for JetBrains IDEs: copy it to the IDE's
`filetypes` config directory.
