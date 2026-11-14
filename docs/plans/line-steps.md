# `directory` and `line` steps

## Status (2026-11-14)

Built, **uncommitted** in taskpod and arch-config, version still 0.8.1. fmt,
clippy, cargo test (63), bun test (22) and `bun run check` pass. A diff of
arch-config's `main.task` on this machine shows nothing to change (apart from
pending package updates).

## What was built

- `directory "path" [file options] from "dir"`: each file under the source
  directory is a file step with the same options (`FileStep::dir`,
  `Runner::dir`). Files only on the system are left alone; `--pull` copies
  them into the source directory (`Runner::pull_new_files`, listing through
  `target::list`, local or remote). A `file` copied from a directory fails
  with a hint to use `directory`.
- `line "path" [sudo] [keep] [base "path"] [before|after "start"] "text"`
  (`Step::Line`, `Runner::line`): the file has the line, compared ignoring
  spacing. Missing, it goes before the first line starting with `before`,
  after the last starting with `after` (ignoring indentation), or at the end.
  The file must exist, or `base` (on the target machine) is copied first,
  with its mode and owner. Writes keep the file's mode and owner. `--pull`
  has nothing to pull. Undo works from the journal, below.

- The journal (`journal::Lines`, an entry `<path>%lines` next to the file
  entries) records per file whether taskpod created it (and the base's
  content then), and per step (keyed by `match`, else the text) the line it
  wrote and the line it replaced. Undo (`Runner::undo_line_step`) removes or
  restores only recorded lines, asks when only `match` finds the line (it
  changed since), deletes a created file once nothing else of taskpod's is
  in it and it equals the base, and reports a line with no record unless
  the step says `new`. `match "start"` replaces an edited line instead of
  adding a second one; a line on the wrong side of `before`/`after` (not
  counting itself) is moved. A moved line that wasn't taskpod's isn't
  recorded, so undo doesn't move it back.

Decisions: plain prefixes rather than regexes for `before`/`after` (no new
dependency); a missing file without `base` is an error.

## arch-config

- opensnitch: one `directory` step instead of 34 template calls, and a
  `let recolor` shared by the tray icon fix and its pacman hook.
- yubikey: `line` steps for the pam_u2f lines (polkit-1 and kde with `base`
  from `/usr/lib/pam.d`, all with `match` and `new`), `files/etc/pam.d/`
  deleted, the `undo sed` lines gone.
- shell-functions: two `line ... new` steps.
- `arch/CLAUDE.md` mentions both steps. Another agent's opensnitch work
  (the task and `files/etc/opensnitchd/`) is also still uncommitted.

## Not done

- `directory "path"` without `from` to create a directory with `mode`/`owner`
  (docker's `install -d`): one use, guarded and `keep`, not worth a feature yet.
