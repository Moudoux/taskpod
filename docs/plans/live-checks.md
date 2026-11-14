# Plan: live checks in the VS Code extension

Goal: while editing a task file in VS Code, show for each task whether this
system matches it (✓ up to date, ~ would change, ⊘ skipped, ✗ failed), updated
when the file is opened and saved, with the diff on hover. Off by default and
turned on per file per machine, so files meant for other systems never run.

## Status (2026-11-14)

Built, uncommitted, version bumped to 0.7.0: `--json` (`src/exec/json.rs`, shape v1),
`--no-secrets`, the extension's live checks (`src/live.ts`, `src/checks.ts`), docs.
fmt, clippy, cargo test (45) and bun test (18) / check pass. Not yet tried in a
real VS Code window. Not done: the optional per-file `--host`.

## State before this work (0.6.0, commit 033b29e)

- taskpod 0.6.0 is on master and pushed: `--pull`, `--host`, `age-key`.
- The extension (`editors/vscode/src/extension.ts`) already:
  - finds the binary with the `taskpod.path` setting (default `taskpod`);
  - runs `taskpod FILE --check` with `execFile` (10 s timeout, `NO_COLOR=1`)
    for diagnostics, parsing `error: file:line:col: message` from stderr;
  - has CodeLenses on each task (Run / Dry run / Diff) that open a terminal
    (`provideCodeLenses`, around line 381; commands registered around line 536);
  - has its own line-based indexer (`src/index.ts`) giving task names and lines.
- The Rust side has no machine-readable output yet. Task origins are
  `Task::origin` (`"path:line"` strings, `src/ast.rs`). Outcomes are
  `exec::Outcome` (Ran / UpToDate / Skipped(reason) / Failed / Blocked), and
  the per-step lines a check prints are collected in `TaskRun::lines` in
  `src/exec/step.rs`, already formatted with colors.
- Nothing for this feature exists yet. This plan isn't committed yet.

## Plan

### 1. taskpod: `--json` (Rust)

- `taskpod FILE -D --json` prints one JSON document on stdout instead of the
  human output (no live display, no summary): for each task in the selected order,
  `{ name, file, line, outcome: "up-to-date"|"would-change"|"skipped"|"failed"|"blocked",
  reason?, steps: [{ label, kind: "change"|"ok"|"note", diff?: [lines] }] }`.
- Collect structured step results next to (or instead of) the formatted
  `TaskRun::lines`. Strip styling (`console::strip_ansi_codes`) or build plain
  text, and keep every string redacted with `ui::redact`.
- `--json` should also work with `-d` (every step) and `--pull -D`, so the
  extension could later show pull previews. Reject `--json` in run mode.
- Prompt variables: without a terminal they already take defaults, and an
  `input` without a default fails. Report that per task in the JSON instead
  of failing the whole run if possible.
- Tests in `src/exec/tests.rs`: parse the JSON for a small file covering each
  outcome.

### 2. taskpod: `--no-secrets`

A diff decrypts secrets, which with a security key means a PIN and touch on
every save. With `--no-secrets`, `Secrets::get` returns an error such as
"uses secrets, not checked" without running sops, and the step or task that
needs a secret is reported as such (a note, not a failure, in the JSON).
The extension always passes it, at least by default (setting `taskpod.checkSecrets`, default false).

### 3. Extension: live checks

- A command "Taskpod: Check this file against this system" (and "Stop
  checking this file") toggles checks for a file. Store the set of enabled file
  paths in `context.workspaceState` (per machine, never in the repo). Show the
  state in the status bar for the active task file.
- When enabled: run `taskpod FILE -D --json --no-secrets` when the file is opened, on
  save (debounced ~500 ms) and from a "Refresh" command. Cancel a running check
  when a new one starts (kill the child). Show a status bar spinner while it runs.
  Timeout configurable (`taskpod.checkTimeout`, default 120 s).
- Show results per task: a CodeLens title next to the existing Run / Dry run / Diff
  (e.g. `✓ up to date`, `~ 3 changes`, `⊘ skipped: unless pacman -Q foo`),
  plus a gutter decoration. Hover on the task name shows the step list and
  diffs (markdown code block, `diff` language). Clicking opens the full diff in a
  read-only virtual document or an output channel.
- Map tasks to lines by name with the extension's own index (`src/index.ts`),
  since the file may have been edited since the check started. Tasks from
  included files show in the included file when it's open and enabled.
- Optional, last: a per-file host (workspace state too) that adds `--host`.
- Tests: extension unit tests for parsing the JSON and mapping to tasks (bun test),
  like the existing `src/*.test.ts`.

### 4. Docs and release

- README "VS Code" section, `src/docs.ts` if anything is user-facing there,
  CLAUDE.md layout notes. Bump the version (Cargo.toml, PKGBUILD `pkgver`,
  `editors/vscode/package.json`) when done.

## Things to keep in mind

- Checks run `if`/`unless` commands and `$( )` variables for real (read-only by
  convention). Remind in the README that enabling checks runs those on save.
- Checks run one task at a time (`Options::checking()` means one job slot), so a
  large `main.task` can take seconds; never block the editor on it.
- Never print or log secret values. Everything shown goes through `ui::redact`.
