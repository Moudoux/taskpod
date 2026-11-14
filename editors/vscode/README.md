# Taskpod for VS Code

Language support for [taskpod](https://github.com/Moudoux/taskpod) `.task` files.

- **Highlighting**, with shell highlighting inside `run`, `if` and `unless`
- **Completion** for keywords (with snippets), templates, task names after
  `needs` and `of`, variables and secrets inside `{{ }}`, file options and encodings
- **Hover** docs for every keyword, task summaries, template bodies and
  variable values
- **Ctrl+click** on tasks, templates and variables to go to their definition,
  on `{{ secrets.name }}` to go to its key in the sops file (keys stay
  readable in encrypted files, values are never read), on `from`, `include`,
  `env` and `secrets` paths to open the file, and on a `file` destination to
  open the file on your system
- **Find references**, **rename** and the **outline** for tasks, templates and variables
- **Diagnostics** while typing (unknown tasks, templates and variables, missing
  files, secrets missing from the `secrets` files) and from `taskpod --check` when saving
- **Run, Dry run and Diff** above each task, and in the editor title bar for the file
- **Live checks**: whether this system matches each task (✓ up to date,
  ~ would change, ⊘ skipped, ✗ failed, ? not checked), above the task and in the
  gutter, with the steps and diffs on hover; click the result for the full diff.

## Live checks

Live checks are off until you turn them on for a file with
**Taskpod: Check this file against this system** (or click `taskpod` in the
status bar). The choice is stored for this workspace on this machine, never in
the repository, so a file written for another system doesn't run here unless
you turn it on here.

When on, the extension runs `taskpod FILE --diff --json --no-secrets` when the file
is opened and saved, and when a file it includes is saved. A newer check
cancels a running one; the status bar shows progress and a summary.
**Taskpod: Check again** reruns it, **Taskpod: Stop checking this file** turns it off.

A check changes nothing, but it does run the file's `if` and `unless` checks
and `$( )` variables, on every save. Keep those read-only, as taskpod expects.
Secrets aren't decrypted (so there's no PIN or key touch on save) unless
`taskpod.checkSecrets` is on; steps that need them show as not checked.
Questions (`input`, `choose`, `confirm`) take their defaults, and an `input`
without a default makes its task fail in the check.

## Settings

- `taskpod.path`: the taskpod binary, default `taskpod`
- `taskpod.checkOnSave`: run `taskpod --check` on open and save, default on
- `taskpod.codeLens`: show Run, Dry run and Diff above tasks, default on
- `taskpod.checkSecrets`: let live checks decrypt secrets, default off
- `taskpod.checkTimeout`: seconds a live check may take, default 120

## Building

```sh
bun install
bun test             # indexer, grammar and live check tests
bun run package      # type check, bundle and create taskpod-<version>.vsix
code --install-extension taskpod-*.vsix
```
