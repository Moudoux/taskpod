# `changed` guard, the Pi pass, and what's next

## Status (2026-11-14)

All **uncommitted**, in both repos. The user has installed taskpod 0.10.0 and the
VS Code extension built from this working tree.

### taskpod (version bumped to 0.10.0: Cargo.toml, Cargo.lock, PKGBUILD, package.json)

- `Guard::Changed` (`src/ast.rs`): `changed { ... }` runs a block only if an
  earlier step in the same task changed something; in a template, a `changed`
  line makes the call wait for that. A task-level `changed` is a parse error
  ("nothing before it"). `changed` is a keyword (no template may be named so).
- `TaskRun::changed_before` (`src/exec/step.rs`) is the OR of the enclosing
  blocks' earlier changes; `nested` checks `Changed` before other guards.
  `Runner::failing_guard` treats it as passing, `guard_passes` bails (only a
  task run knows). In undo it always applies, so its `undo` runs where it stands.
- Tests: `changed_runs_only_after_a_change`, `changed_needs_something_before_it`
  (src/exec/tests.rs), plus extension tests in index.test.ts and grammar.test.ts.
- spec.task, docs.ts, index.ts, the TextMate grammar and Task.xml know it.
- fmt, clippy, cargo test (65), bun test (24), bun run check pass.

### arch-config

- `pi/main.task`: new `compose <dir> [up flags]` template (`changed`, `docker
  compose up -d`, undo `down`) replaces all nine `<stack>-up` subtasks; showport
  wraps its call in `if cd ~/stacks/showport && [ -n "$(docker compose ps -q)" ] {`.
  `docker-restart` subtask is a `changed { keep; run ... }` block in docker-config.
  proxy: `directory` for `~/proxy/nginx` and `~/proxy/www`. shell: two `line ... new`
  alias steps; `pi/files/home/.bashrc` deleted (git rm, staged).
- `pi/CLAUDE.md` and `arch/CLAUDE.md` mention `changed`.
- `taskpod main.task -H pi@raspberrypi -D --no-secrets` says nothing would change.

## Next steps: done (2026-11-14, same session), version 0.11.0

1. Arch `initramfs` subtask is a `changed` block in `kernel-modules`.
2. `--pull` on a `directory` leaves out files other steps of the task write
   (`TaskRun::written_paths`) and only lists the source's own directories, not
   subdirectories it doesn't have (`target::list` takes `subdirs`, no recursion),
   so an app's `data/` is never offered. The Pi's gitea uses `directory`.
3. `directory "path"` without `from` (`FileSource::Empty`, `TaskRun::make_dir`,
   `undo_dir`; `target::dir_state`, `make_dir`, `remove_dir`). The journal
   records `Previous::Missing`, or a file entry with no content for the mode
   and owner before. Used for the Pi's /var/lib/tramline (mode 700, what it is
   on the Pi) and Arch's /mnt/scratch/docker-rootless (710, what Docker sets).

`-D` on both systems says nothing would change. Still uncommitted.
