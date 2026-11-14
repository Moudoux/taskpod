# Taskpod for VS Code

Language support for [taskpod](https://github.com/Moudoux/taskpod) `.task` files.

- **Highlighting**, with shell highlighting inside `run`, `if` and `unless`
- **Completion** for keywords (with snippets), templates, task names after
  `needs` and `of`, variables inside `{{ }}`, file options and encodings
- **Hover** docs for every keyword, task summaries, template bodies and
  variable values
- **Ctrl+click** on tasks, templates and variables to go to their definition,
  on `from`, `include` and `env` paths to open the file, and on a `file`
  destination to open the file on your system
- **Find references**, **rename** and the **outline** for tasks, templates and variables
- **Diagnostics** while typing (unknown tasks, templates and variables, missing
  files) and from `taskpod --check` when saving
- **Run, Dry run and Diff** above each task, and in the editor title bar for the file

## Settings

- `taskpod.path`: the taskpod binary, default `taskpod`
- `taskpod.checkOnSave`: run `taskpod --check` on open and save, default on
- `taskpod.codeLens`: show Run, Dry run and Diff above tasks, default on

## Building

```sh
bun install
bun test             # indexer and grammar tests
bun run package      # type check, bundle and create taskpod-<version>.vsix
code --install-extension taskpod-*.vsix
```
