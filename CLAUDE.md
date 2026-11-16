# Taskpod

A declarative task runner written in Rust. Task files (`.task`) declare tasks
with dependencies; tasks run in parallel, and `if`/`unless` checks make them
idempotent. Its main use is setting up an Arch system from
[arch-config](https://github.com/Moudoux/arch-config)'s `main.task`.

[spec.task](spec.task) is the language reference, keep it in sync with the parser.

## Layout

- `src/parser.rs`: line-based parser (statements end at the line end, `\` continues).
  `run` lines and `run { }` blocks are raw shell; brace matching in blocks skips quotes and comments.
- `src/ast.rs`: the parsed file, `link()` checks references and hoists template `needs` into tasks.
  File encodings live here too.
- `src/vars.rs`: `{{ name }}` templating, scopes, lazy `$( )` and prompt variables, how commands are spawned.
- `src/secrets.rs`: `{{ secrets.name }}` from sops files, decrypted on first use; `ui::redact`
  hides the values from everything printed (lines, spinners, diffs, errors).
- `src/exec/mod.rs`: task selection, the parallel scheduler (job slots, named locks,
  an RwLock so `interactive` tasks run alone), summaries. Modes: run, dry run (`-d`), diff (`-D`).
- `src/exec/step.rs`: running steps: shell, files (content/mode/owner, diffs), template calls and inline blocks.
  With `--pull` (`Options::pull`, combined with the Run/DryRun/Diff mode) file steps are compared the
  other way and queue edits on `Runner::edits`, which `main` applies at the end.
- `src/exec/journal.rs`: `--undo`'s record of each file before taskpod first wrote it, on the target
  machine (`~/.local/state/taskpod`, root files in `/var/lib/taskpod`); `Ctx::journal` overrides the
  place, and test builds always use the task file's directory. Undo itself (`undo_task`, `undo_block`,
  `undo_file`) is in `src/exec/step.rs`; `Runner::run` reverses dependencies when `Options::undo`.
- `src/exec/graph.rs`: `--graph` (tree, dot, mermaid) of the tasks `select` picks, from the file alone.
- `src/exec/sources.rs`: `--sources`, a tree of the files a task file reads, by the file naming them:
  `TaskFile::declarations` (include/env/secrets/age-key, recorded by the parser), `from` sources and
  secret names used (through variables and template calls too). Like `--graph`, only reads the file.
- `src/exec/json.rs`: `--json` documents. `TaskRun` records each step as a `StepRecord` next to the
  printed lines; `--no-secrets` errors (`secrets::NOT_CHECKED`, matched by text since lazy variables
  stringify errors) make a step `unchecked`, or the task `Outcome::Unchecked` if a guard or variable needs one.
- `src/exec/pull.rs`: mapping system content back onto templates (plain lines only, never secrets),
  writing string literals, and applying edits by the source spans the parser records (`FileOrigin`).
- `src/exec/target.rs`: reading, stat-ing and writing files on the machine being configured, local or remote.
  Root writes (`INSTALL`, local and remote) `install` to a `.taskpod.XXXXXX` file beside the target
  and `mv` it over, so watchers never see it missing; user writes truncate in place.
- `src/remote.rs`: `--host`: one ssh master connection per run, every command a POSIX script sent over
  stdin to a fixed bootstrap. Remote sudo passwords go through an askpass helper. `Ctx::remote` is set
  before parsing, since `include ... if` checks run on the target.
- `src/exec/sudo.rs`: authenticates once through `sudo -v` and keeps credentials fresh.
- `src/ui.rs`: indicatif live display and inquire prompts; plain prefixed lines without a terminal.
- `editors/vscode/`: VS Code extension (TypeScript, built with bun). `src/index.ts` is its own
  line-based indexer of task files, separate from the Rust parser; update both when the syntax changes,
  along with the TextMate grammar, `src/docs.ts` and [Task.xml](Task.xml).
  `src/project.ts` tracks which workspace files include which, so an included file is read
  together with the files above it (lookups, diagnostics, and `--check` run from those files).
  Live checks: `src/live.ts` reads `--json` (no vscode imports, unit tested), `src/checks.ts` runs
  checks for files enabled in workspace state and shows results; bump `JSON_VERSION` in both with the shape.
- `dist/arch/PKGBUILD`: Arch package, builds from the repo (`cd dist/arch && makepkg -si`).
  Its `pkgver` must match `Cargo.toml`; CI checks this.

## Commands

```sh
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
cargo run -- spec.task -D          # try the example file
cd editors/vscode && bun install && bun test && bun run check && bun run package
just install                       # build and install the Arch package and the extension (justfile)
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy, tests, the extension tests and builds
Linux and Windows binaries; `v*` tags create a GitHub release.

## Rules

- Tests must never call sudo. To test anything that might, put a fake `sudo` that fails
  first on `PATH`. Remote tests use a fake `ssh` that runs commands locally (`fake_remote` in
  `src/exec/tests.rs`). Write fake executables with `test_util::write_script`, which avoids
  "text file busy" races with parallel tests. Dry runs and diffs only run `if`/`unless` checks, so keep checks read-only.
- `.gitignore` patterns are anchored to the root (`/test*`), so source files aren't ignored by accident.
- Pushing straight to master is fine. Commits use the GitHub noreply address set in this
  repo's git config. Commits are signed with a YubiKey, which needs a touch, so a commit
  can time out if nobody is at the key.
