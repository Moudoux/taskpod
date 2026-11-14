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
- `src/exec/mod.rs`: task selection, the parallel scheduler (job slots, named locks,
  an RwLock so `interactive` tasks run alone), summaries. Modes: run, dry run (`-d`), diff (`-D`).
- `src/exec/step.rs`: running steps: shell, files (content/mode/owner, diffs), template calls and inline blocks.
- `src/exec/sudo.rs`: authenticates once through `sudo -v` and keeps credentials fresh.
- `src/ui.rs`: indicatif live display and inquire prompts; plain prefixed lines without a terminal.
- `editors/vscode/`: VS Code extension (TypeScript, built with bun). `src/index.ts` is its own
  line-based indexer of task files, separate from the Rust parser; update both when the syntax changes,
  along with the TextMate grammar, `src/docs.ts` and [Task.xml](Task.xml).
- `dist/arch/PKGBUILD`: Arch package, builds from the repo (`cd dist/arch && makepkg -si`).
  Its `pkgver` must match `Cargo.toml`; CI checks this.

## Commands

```sh
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
cargo run -- spec.task -D          # try the example file
cd editors/vscode && bun install && bun test && bun run check && bun run package
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy, tests, the extension tests and builds
Linux and Windows binaries; `v*` tags create a GitHub release.

## Rules

- Tests must never call sudo. To test anything that might, put a fake `sudo` that fails
  first on `PATH`. Dry runs and diffs only run `if`/`unless` checks, so keep checks read-only.
- `.gitignore` patterns are anchored to the root (`/test*`), so source files aren't ignored by accident.
- Pushing straight to master is fine. Commits use the GitHub noreply address set in this
  repo's git config. Commits are signed with a YubiKey, which needs a touch, so a commit
  can time out if nobody is at the key.
