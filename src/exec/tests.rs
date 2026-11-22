use super::*;
use crate::ast::{Encoding, FileSource, Step};
use crate::parser::parse_str;

fn options(jobs: usize) -> Options {
    Options {
        mode: Mode::Run,
        ignore_guards: false,
        jobs,
        skip: HashSet::new(),
        pull: false,
        yes: false,
        undo: false,
        adopt: false,
        root: false,
    }
}

fn run(source: &str, dir: &Path, opts: &Options, targets: &[&str]) -> HashMap<String, Outcome> {
    let file = parse_str(source, dir).unwrap_or_else(|e| panic!("{e:#}"));
    let targets: Vec<String> = targets.iter().map(|s| s.to_string()).collect();
    let order = select(&file, &targets).unwrap();
    Runner::new(&file, opts).run(&order).into_iter().collect()
}

/// Sets a file's modification time, as `touch -d @<seconds>` would on GNU
/// (BSD's `touch` doesn't take that).
fn set_modified(path: &Path, secs: u64, nanos: u32) {
    let time = std::time::UNIX_EPOCH + std::time::Duration::new(secs, nanos);
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(time))
        .unwrap();
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap_or_default()
}

#[test]
fn selects_dependencies_in_order() {
    let file = parse_str(
        "task a {\n needs b\n run true\n}\ntask b {\n needs c\n run true\n}\ntask c {\n manual\n run true\n}\ntask d {\n manual\n run true\n}\nsubtask e of a {\n run true\n}\n",
        Path::new("/tmp"),
    )
    .unwrap();
    assert_eq!(select(&file, &[]).unwrap(), ["c", "b", "a", "e"]);
    assert_eq!(select(&file, &["b".into()]).unwrap(), ["c", "b"]);
    assert_eq!(select(&file, &["d".into()]).unwrap(), ["d"]);
    assert!(select(&file, &["zzz".into()]).is_err());
}

#[test]
fn detects_cycles() {
    let file = parse_str(
        "task a {\n needs b\n}\ntask b {\n needs a\n}\n",
        Path::new("/tmp"),
    )
    .unwrap();
    let err = select(&file, &[]).unwrap_err().to_string();
    assert!(err.contains("a -> b -> a"), "{err}");
}

#[cfg(unix)]
#[test]
fn runs_in_dependency_order_and_parallel() {
    let dir = tempfile::tempdir().unwrap();
    // `slow1` and `slow2` each wait for the other to start, so they only
    // finish when run in parallel.
    let source = r#"
        task slow1 {
            run touch s1; for i in $(seq 100); do [ -e s2 ] && exit 0; sleep 0.05; done; exit 1
        }
        task slow2 {
            run touch s2; for i in $(seq 100); do [ -e s1 ] && exit 0; sleep 0.05; done; exit 1
        }
        task last {
            needs slow1 slow2
            run [ -e s1 ] && [ -e s2 ] && echo ok > last
        }
    "#;
    let outcomes = run(source, dir.path(), &options(4), &[]);
    assert_eq!(outcomes["slow1"], Outcome::Ran);
    assert_eq!(outcomes["slow2"], Outcome::Ran);
    assert_eq!(outcomes["last"], Outcome::Ran);
    assert_eq!(read(dir.path(), "last").trim(), "ok");
}

#[cfg(unix)]
#[test]
fn failures_block_dependents_only() {
    let dir = tempfile::tempdir().unwrap();
    let source = r#"
        task bad {
            run exit 3
            run touch not-reached
        }
        task after-bad {
            needs bad
            run touch after-bad
        }
        task other {
            run touch other
        }
    "#;
    let outcomes = run(source, dir.path(), &options(2), &[]);
    assert!(matches!(&outcomes["bad"], Outcome::Failed(e) if e.contains("status 3")));
    assert_eq!(outcomes["after-bad"], Outcome::Blocked("bad".into()));
    assert_eq!(outcomes["other"], Outcome::Ran);
    assert!(!dir.path().join("not-reached").exists());
    assert!(!dir.path().join("after-bad").exists());
    assert!(dir.path().join("other").exists());
}

#[cfg(unix)]
#[test]
fn guards_templates_and_subtasks() {
    let dir = tempfile::tempdir().unwrap();
    let source = r#"
        let greeting = "hello"
        template mark {
            unless test -e {{ 1 }}
            run echo {{ greeting }} {{ args }} > {{ 1 }}
        }
        task present {
            run touch existing
        }
        task marks {
            needs present
            mark existing
            mark new extra
        }
        task skipped {
            if false
            run touch skipped
        }
        subtask child of skipped {
            run touch child
        }
        subtask child2 of marks {
            run touch child2
        }
        task current {
            needs present
            mark existing
        }
        subtask after-current of current {
            run touch after-current
        }
    "#;
    let outcomes = run(source, dir.path(), &options(4), &[]);
    assert_eq!(outcomes["marks"], Outcome::Ran);
    assert_eq!(read(dir.path(), "existing"), "");
    assert_eq!(read(dir.path(), "new").trim(), "hello new extra");
    assert_eq!(outcomes["skipped"], Outcome::Skipped("if false".into()));
    assert_eq!(
        outcomes["child"],
        Outcome::Skipped("skipped had nothing to do".into())
    );
    assert_eq!(outcomes["child2"], Outcome::Ran);
    assert_eq!(outcomes["current"], Outcome::UpToDate);
    assert!(matches!(outcomes["after-current"], Outcome::Skipped(_)));
}

#[cfg(unix)]
#[test]
fn locks_serialize() {
    let dir = tempfile::tempdir().unwrap();
    // Each task fails if another one holds the "busy" marker.
    let script = "[ ! -e busy ] && touch busy && sleep 0.2 && rm busy";
    let source = format!(
        "task a {{\n lock db\n run {script}\n}}\ntask b {{\n lock db\n run {script}\n}}\ntask c {{\n lock db\n run {script}\n}}\n"
    );
    let outcomes = run(&source, dir.path(), &options(3), &[]);
    for name in ["a", "b", "c"] {
        assert_eq!(outcomes[name], Outcome::Ran, "{name}");
    }
}

#[cfg(unix)]
#[test]
fn skip_and_dry_run() {
    let dir = tempfile::tempdir().unwrap();
    let source = "task a {\n run touch a\n}\ntask b {\n needs a\n run touch b\n}\n";
    let mut opts = options(2);
    opts.skip.insert("a".into());
    let outcomes = run(source, dir.path(), &opts, &[]);
    assert_eq!(outcomes["a"], Outcome::Skipped("--skip".into()));
    assert_eq!(outcomes["b"], Outcome::Ran);
    assert!(!dir.path().join("a").exists());

    let dry = tempfile::tempdir().unwrap();
    let mut opts = options(2);
    opts.mode = Mode::DryRun;
    let outcomes = run(source, dry.path(), &opts, &[]);
    assert_eq!(outcomes["a"], Outcome::Ran);
    assert!(std::fs::read_dir(dry.path()).unwrap().next().is_none());
}

#[cfg(unix)]
#[test]
fn files_are_written_once() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let source = r#"
        task files {
            file "out/conf" "a = {{ dir }}\n"
            file "out/script.sh" """
                #!/bin/sh
                echo hi
                """
        }
    "#;
    let outcomes = run(source, dir.path(), &options(1), &[]);
    assert_eq!(outcomes["files"], Outcome::Ran);
    let conf = read(dir.path(), "out/conf");
    assert_eq!(conf, format!("a = {}\n", dir.path().display()));
    let mode = std::fs::metadata(dir.path().join("out/script.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    assert_eq!(read(dir.path(), "out/script.sh"), "#!/bin/sh\necho hi\n");

    let outcomes = run(source, dir.path(), &options(1), &[]);
    assert_eq!(outcomes["files"], Outcome::UpToDate);
}

#[cfg(unix)]
#[test]
fn inline_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let source = r#"
        task t {
            run touch a
            unless test -e a {
                run touch never
            }
            if test -e a {
                run touch b
            }
        }
    "#;
    let outcomes = run(source, dir.path(), &options(1), &[]);
    assert_eq!(outcomes["t"], Outcome::Ran);
    assert!(dir.path().join("b").exists());
    assert!(!dir.path().join("never").exists());
}

#[cfg(unix)]
#[test]
fn diff_mode_and_encodings() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("src.txt"), "copied é\n").unwrap();
    let source = r#"
        task files {
            file "out/utf16" encoding utf-16le "hi\n"
            file "out/latin" encoding latin1 "é\n"
            file "out/copy" mode 600 from "src.txt"
        }
    "#;
    let mut diff = options(1);
    diff.mode = Mode::Diff;
    assert_eq!(run(source, dir.path(), &diff, &[])["files"], Outcome::Ran);
    assert!(!dir.path().join("out").exists());

    assert_eq!(
        run(source, dir.path(), &options(1), &[])["files"],
        Outcome::Ran
    );
    assert_eq!(
        std::fs::read(dir.path().join("out/utf16")).unwrap(),
        [0xFF, 0xFE, b'h', 0, b'i', 0, b'\n', 0]
    );
    assert_eq!(
        std::fs::read(dir.path().join("out/latin")).unwrap(),
        [0xE9, b'\n']
    );
    assert_eq!(read(dir.path(), "out/copy"), "copied é\n");

    // Now the system matches, so a diff finds nothing.
    assert_eq!(
        run(source, dir.path(), &diff, &[])["files"],
        Outcome::UpToDate
    );

    // A mode change is a difference too.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        dir.path().join("out/copy"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert_eq!(run(source, dir.path(), &diff, &[])["files"], Outcome::Ran);
}

#[cfg(unix)]
#[test]
fn secrets_fill_files_and_commands() {
    let dir = tempfile::tempdir().unwrap();
    // A fake sops that prints the "encrypted" file as is.
    let fake = dir.path().join("fake-sops");
    crate::test_util::write_script(&fake, "#!/bin/sh\ncat \"$4\"\n");
    std::fs::write(
        dir.path().join("secrets.json"),
        r#"{"wifi": {"password": "hunter2-wifi"}, "token": "tok-123456"}"#,
    )
    .unwrap();
    let source = r#"
        secrets "secrets.json"
        task conf {
            file "wifi.conf" mode 600 "psk={{ secrets.wifi.password }}\n"
            run echo "{{ secrets.token }}" > token
        }
        task missing {
            manual
            run echo "{{ secrets.nope }}"
        }
    "#;
    let mut file = parse_str(source, dir.path()).unwrap();
    file.ctx.secrets.set_program(fake.display().to_string());
    let opts = options(2);
    let runner = Runner::new(&file, &opts);
    let outcomes: HashMap<_, _> = runner
        .run(&select(&file, &[]).unwrap())
        .into_iter()
        .collect();
    assert_eq!(outcomes["conf"], Outcome::Ran);
    assert_eq!(read(dir.path(), "wifi.conf"), "psk=hunter2-wifi\n");
    assert_eq!(read(dir.path(), "token").trim(), "tok-123456");
    // Loaded secrets are hidden from output.
    assert_eq!(
        crate::ui::redact("psk=hunter2-wifi and tok-123456"),
        "psk=•••• and ••••"
    );

    let outcomes: HashMap<_, _> = runner
        .run(&select(&file, &["missing".into()]).unwrap())
        .into_iter()
        .collect();
    assert!(
        matches!(&outcomes["missing"], Outcome::Failed(e) if e.contains("no secret 'nope'")),
        "{:?}",
        outcomes["missing"]
    );
}

/// A fake `ssh` that runs commands here, in `home`, with `bin` first on PATH.
#[cfg(unix)]
fn fake_remote(dir: &Path) -> (std::sync::Arc<crate::remote::Remote>, PathBuf) {
    let home = dir.join("home");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let ssh = dir.join("ssh");
    let script = format!(
        r#"#!/bin/sh
master=0
while [ $# -gt 0 ]; do
  case "$1" in
    -o) shift 2 ;;
    -O) exit 0 ;;
    -M) master=1; shift ;;
    -t|-T) shift ;;
    --) shift; break ;;
    *) break ;;
  esac
done
shift
[ $master = 1 ] && exec cat > /dev/null
cd '{home}' && HOME='{home}' PATH='{bin}':$PATH exec sh -c "$*"
"#,
        home = home.display(),
        bin = bin.display()
    );
    crate::test_util::write_script(&ssh, &script);
    let remote = crate::remote::Remote::connect_with("box", ssh.to_str().unwrap()).unwrap();
    (std::sync::Arc::new(remote), home)
}

#[cfg(unix)]
fn run_remote(
    source: &str,
    dir: &Path,
    remote: &std::sync::Arc<crate::remote::Remote>,
    opts: &Options,
) -> HashMap<String, Outcome> {
    let mut file = parse_str(source, dir).unwrap_or_else(|e| panic!("{e:#}"));
    file.ctx.remote = Some(remote.clone());
    let order = select(&file, &[]).unwrap();
    Runner::new(&file, opts).run(&order).into_iter().collect()
}

#[cfg(unix)]
#[test]
fn runs_over_ssh() {
    let dir = tempfile::tempdir().unwrap();
    let (remote, home) = fake_remote(dir.path());
    let source = r#"
        let who = $(echo "$HOME")
        task remote {
            if test -d "{{ home }}"
            run pwd > here
            file "~/conf/a.txt" mode 600 "home={{ home }} who={{ who }} it's\n"
            unless test -e nothing {
                run echo block > block
            }
        }
        task elsewhere {
            workdir "sub"
            run mkdir -p ~/sub
            run pwd > here
        }
    "#;
    std::fs::create_dir_all(home.join("sub")).unwrap();
    let outcomes = run_remote(source, dir.path(), &remote, &options(2));
    assert_eq!(outcomes["remote"], Outcome::Ran, "{outcomes:?}");
    assert_eq!(outcomes["elsewhere"], Outcome::Ran, "{outcomes:?}");
    let home_text = home.display().to_string();
    assert_eq!(read(&home, "here").trim(), home_text);
    assert_eq!(read(&home, "sub/here").trim(), format!("{home_text}/sub"));
    assert_eq!(read(&home, "block").trim(), "block");
    assert_eq!(
        read(&home, "conf/a.txt"),
        format!("home={home_text} who={home_text} it's\n")
    );
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(home.join("conf/a.txt"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    // Nothing was written next to the task file.
    assert!(!dir.path().join("here").exists());

    // A second run finds the file up to date.
    let source = "task again {\n file \"~/conf/a.txt\" mode 600 \"x\"\n}\ntask same {\n file \"~/conf/b.txt\" \"b\"\n}\n";
    std::fs::write(home.join("conf/b.txt"), "b").unwrap();
    std::fs::set_permissions(
        home.join("conf/b.txt"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let mut opts = options(1);
    opts.mode = Mode::DryRun;
    let outcomes = run_remote(source, dir.path(), &remote, &opts);
    assert_eq!(outcomes["again"], Outcome::Ran);
    assert_eq!(outcomes["same"], Outcome::UpToDate);
}

#[cfg(unix)]
#[test]
fn remote_sudo_uses_the_password() {
    let dir = tempfile::tempdir().unwrap();
    let (remote, home) = fake_remote(dir.path());
    // A fake sudo that only works through askpass with the right password.
    let sudo = dir.path().join("bin/sudo");
    crate::test_util::write_script(
        &sudo,
        "#!/bin/sh\n[ \"$1\" = -A ] || exit 9\nshift\n[ \"$(\"$SUDO_ASKPASS\")\" = \"pa'ss \\$w\" ] || exit 8\n[ \"$1\" = -k ] && exit 0\nexec \"$@\"\n",
    );
    remote.set_sudo_password("pa'ss $w");
    // Writes as root go through symlinks, like the other writes.
    std::fs::write(home.join("real.conf"), "old\n").unwrap();
    std::os::unix::fs::symlink(home.join("real.conf"), home.join("link.conf")).unwrap();
    let source = r#"
        task root {
            run sudo sh -c 'echo ok > rooted'
            file "~/etc/x.conf" sudo "x\n"
            file "~/link.conf" sudo "linked\n"
        }
    "#;
    let outcomes = run_remote(source, dir.path(), &remote, &options(1));
    assert_eq!(outcomes["root"], Outcome::Ran, "{outcomes:?}");
    assert_eq!(read(&home, "rooted").trim(), "ok");
    assert_eq!(read(&home, "etc/x.conf"), "x\n");
    assert!(home.join("link.conf").is_symlink());
    assert_eq!(read(&home, "real.conf"), "linked\n");
    // The helper directory is cleaned up.
    let leftovers = std::fs::read_dir(std::env::var("XDG_RUNTIME_DIR").unwrap_or("/tmp".into()))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with("taskpod.") && e.path().join("password").exists()
        })
        .count();
    assert_eq!(leftovers, 0);
}

#[cfg(unix)]
#[test]
fn root_only_files_are_read_as_root() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let (remote, home) = fake_remote(dir.path());
    // A fake sudo that can read what the user can't, by lending them read
    // access for the command.
    crate::test_util::write_script(
        &dir.path().join("bin/sudo"),
        &format!(
            "#!/bin/sh\n[ \"$1\" = -n ] && shift\nf='{}'\n[ -e \"$f\" ] && chmod u+r \"$f\"\n\"$@\"\ns=$?\n[ -e \"$f\" ] && chmod u-r \"$f\"\nexit $s\n",
            home.join("secret").display()
        ),
    );
    let source = "task t {\n file \"~/secret\" sudo mode 200 \"x\\n\"\n}\n";
    let outcomes = run_remote(source, dir.path(), &remote, &options(1));
    assert_eq!(outcomes["t"], Outcome::Ran, "{outcomes:?}");
    let mode = std::fs::metadata(home.join("secret"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o200);
    // Unreadable as the user, it's still seen to be up to date.
    let outcomes = run_remote(source, dir.path(), &remote, &options(1));
    assert_eq!(outcomes["t"], Outcome::UpToDate, "{outcomes:?}");
}

/// The labels of a task's steps, after running `source` on the fake remote.
#[cfg(unix)]
fn remote_labels(
    source: &str,
    dir: &Path,
    remote: &std::sync::Arc<crate::remote::Remote>,
    opts: &Options,
) -> (Outcome, Vec<String>) {
    let mut file = parse_str(source, dir).unwrap_or_else(|e| panic!("{e:#}"));
    file.ctx.remote = Some(remote.clone());
    let order = if opts.undo {
        select_undo(&file, &[]).unwrap()
    } else {
        select(&file, &[]).unwrap()
    };
    let runner = Runner::new(&file, opts);
    let outcome = runner.run(&order).remove(0).1;
    let labels = runner.reports.lock().unwrap()["t"]
        .steps
        .iter()
        .map(|s| s.label.clone())
        .collect();
    (outcome, labels)
}

#[cfg(unix)]
#[test]
fn files_in_directories_only_root_can_enter_are_read_as_root() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let (remote, home) = fake_remote(dir.path());
    // Like /var/lib/plasmalogin (mode 750, another user's): the user can't
    // even stat what's in it.
    let locked = home.join("locked");
    std::fs::create_dir_all(locked.join("data")).unwrap();
    std::fs::write(locked.join("layout.json"), "old\n").unwrap();
    std::fs::set_permissions(locked.join("data"), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    // A fake sudo that can enter it, by opening it for the command.
    crate::test_util::write_script(
        &dir.path().join("bin/sudo"),
        &format!(
            "#!/bin/sh\n[ \"$1\" = -n ] && shift\nchmod 700 '{l}'\n\"$@\"\ns=$?\nchmod 000 '{l}'\nexit $s\n",
            l = locked.display()
        ),
    );
    let source = r#"
        task t {
            file "~/locked/layout.json" sudo "new\n"
            directory "~/locked/data" sudo mode 750
            file "~/locked/missing.json" sudo "x\n"
        }
    "#;
    let mut diff = options(1);
    diff.mode = Mode::Diff;
    // A check doesn't use sudo: it can't tell what's there, but never
    // takes it for a new file.
    let (outcome, labels) = remote_labels(source, dir.path(), &remote, &diff);
    assert_eq!(outcome, Outcome::Ran);
    assert!(
        labels[0].ends_with("· can't read the current file"),
        "{labels:?}"
    );
    assert!(
        labels[1].ends_with("· can't see the current directory"),
        "{labels:?}"
    );
    assert!(
        labels[2].ends_with("· can't read the current file"),
        "{labels:?}"
    );

    // Running, it reads, stats and journals them as root.
    let (outcome, labels) = remote_labels(source, dir.path(), &remote, &options(1));
    assert_eq!(outcome, Outcome::Ran, "{labels:?}");
    assert!(labels[0].ends_with("layout.json · content"), "{labels:?}");
    assert!(labels[1].ends_with("· mode 700 → 750"), "{labels:?}");
    assert!(labels[2].ends_with("missing.json · new file"), "{labels:?}");
    let (outcome, labels) = remote_labels(source, dir.path(), &remote, &options(1));
    assert_eq!(outcome, Outcome::UpToDate, "{labels:?}");

    // The journal has what was there: undo puts it back, and deletes only
    // the file that was new.
    let mut undo = options(1);
    undo.undo = true;
    undo.yes = true;
    let (outcome, labels) = remote_labels(source, dir.path(), &remote, &undo);
    assert_eq!(outcome, Outcome::Ran, "{labels:?}");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(read(&locked, "layout.json"), "old\n");
    assert!(!locked.join("missing.json").exists());
    let mode = std::fs::metadata(locked.join("data"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[cfg(unix)]
#[test]
fn root_journal_needs_root_and_undo_puts_times_back() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let dir = tempfile::tempdir().unwrap();
    let (remote, home) = fake_remote(dir.path());
    let db = dir.path().join(".taskpod-journal/root.db");
    // A fake sudo that can read the root journal, by opening it for the command.
    crate::test_util::write_script(
        &dir.path().join("bin/sudo"),
        &format!(
            "#!/bin/sh\n[ \"$1\" = -n ] && shift\nchmod 600 '{db}' 2>/dev/null\n\"$@\"\ns=$?\nchmod 000 '{db}' 2>/dev/null\nexit $s\n",
            db = db.display()
        ),
    );
    let conf = home.join("conf.txt");
    std::fs::write(&conf, "old\n").unwrap();
    set_modified(&conf, 1_600_000_000, 500_000_000);
    let source = "task t {\n file \"~/conf.txt\" sudo \"new\\n\"\n}\n";
    let (outcome, _) = remote_labels(source, dir.path(), &remote, &options(1));
    assert_eq!(outcome, Outcome::Ran);
    assert_eq!(read(&home, "conf.txt"), "new\n");
    // Only the database, which only root can read.
    let names: Vec<String> = std::fs::read_dir(db.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["root.db"]);
    let mode = std::fs::metadata(&db).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o000);

    let mut check = options(1);
    check.mode = Mode::Diff;
    check.undo = true;
    // A check doesn't use sudo, so it can't tell what the journal has.
    let (outcome, labels) = remote_labels(source, dir.path(), &remote, &check);
    assert_eq!(outcome, Outcome::Ran);
    assert!(labels[0].ends_with(UNREAD_JOURNAL_TEST), "{labels:?}");
    // With --root it can.
    check.root = true;
    let (outcome, labels) = remote_labels(source, dir.path(), &remote, &check);
    assert_eq!(outcome, Outcome::Ran);
    assert!(
        labels[0].ends_with("put back what was there before taskpod (mode 644)"),
        "{labels:?}"
    );
    assert_eq!(read(&home, "conf.txt"), "new\n");

    let mut undo = options(1);
    undo.undo = true;
    let (outcome, labels) = remote_labels(source, dir.path(), &remote, &undo);
    assert_eq!(outcome, Outcome::Ran, "{labels:?}");
    assert_eq!(read(&home, "conf.txt"), "old\n");
    // Its modification time is back too.
    let meta = std::fs::metadata(&conf).unwrap();
    assert_eq!(
        (meta.mtime(), meta.mtime_nsec()),
        (1_600_000_000, 500_000_000)
    );
    // And it's gone from the journal.
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
    let journal = rusqlite::Connection::open(&db).unwrap();
    let left: i64 = journal
        .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0);
}

/// The note on root journal entries a check couldn't read.
#[cfg(unix)]
const UNREAD_JOURNAL_TEST: &str = ", if the journal has it (--root reads it)";

#[cfg(unix)]
#[test]
fn pulls_system_changes_back() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display().to_string();
    let main = dir.path().join("main.task");
    let source = format!(
        r#"let name = "pod"
task files {{
    file "{d}/out/copy.conf" from "files/copy.conf"
    file "{d}/out/inline.conf" mode 600 """
        # managed
        name={{{{ name }}}}
        keep
        """
    file "{d}/out/plain" "same"
}}
subtask after of files {{
    run touch {d}/ran
    file "{d}/out/sub" "sub\n"
}}
"#
    );
    std::fs::write(&main, &source).unwrap();
    std::fs::create_dir_all(dir.path().join("files")).unwrap();
    std::fs::write(dir.path().join("files/copy.conf"), "old copy\n").unwrap();
    // Write everything, then change it on the "system".
    let file = crate::parser::parse_file(&main).unwrap();
    let order = select(&file, &[]).unwrap();
    let outcomes: HashMap<_, _> = Runner::new(&file, &options(1))
        .run(&order)
        .into_iter()
        .collect();
    assert_eq!(outcomes["files"], Outcome::Ran);
    let out = dir.path().join("out");
    std::fs::write(out.join("copy.conf"), "new copy\n").unwrap();
    std::fs::write(
        out.join("inline.conf"),
        "# managed!\nname=changed\nkeep\nadded {{ x }}\n",
    )
    .unwrap();
    std::fs::set_permissions(out.join("plain"), std::fs::Permissions::from_mode(0o640)).unwrap();
    std::fs::write(out.join("sub"), "sub2\n").unwrap();
    std::fs::remove_file(dir.path().join("ran")).unwrap();

    let pull = |mode: Mode| {
        let file = crate::parser::parse_file(&main).unwrap();
        let order = select(&file, &[]).unwrap();
        let mut opts = options(4);
        opts.mode = mode;
        opts.pull = true;
        opts.yes = true;
        let runner = Runner::new(&file, &opts);
        let outcomes: HashMap<_, _> = runner.run(&order).into_iter().collect();
        let edits = std::mem::take(&mut *runner.edits.lock().unwrap());
        (outcomes, edits)
    };
    // Previews change nothing.
    for mode in [Mode::DryRun, Mode::Diff] {
        let (outcomes, edits) = pull(mode);
        assert_eq!(outcomes["files"], Outcome::Ran);
        assert!(edits.is_empty());
    }
    assert_eq!(std::fs::read_to_string(&main).unwrap(), source);

    let (outcomes, edits) = pull(Mode::Run);
    assert_eq!(outcomes["files"], Outcome::Ran);
    assert_eq!(outcomes["after"], Outcome::Ran, "subtasks are pulled too");
    assert!(!dir.path().join("ran").exists(), "commands don't run");
    let applied = super::pull::apply(&edits);
    assert!(applied.problems.is_empty(), "{:?}", applied.problems);
    assert_eq!(read(dir.path(), "files/copy.conf"), "new copy\n");
    let pulled = std::fs::read_to_string(&main).unwrap();
    // The templated line was kept, the plain ones pulled, `{{` escaped.
    assert!(
        pulled.contains("        # managed!\n        name={{ name }}\n        keep\n        added \\{{ x }}\n        \"\"\""),
        "{pulled}"
    );
    assert!(
        pulled.contains(r#"file "{d}/out/plain" mode 640 "same""#.replace("{d}", &d).as_str()),
        "{pulled}"
    );
    assert!(pulled.contains(r#""sub2\n""#), "{pulled}");
    // The pulled file parses and renders what the system has, apart from the refused line.
    let file = crate::parser::parse_file(&main).unwrap();
    let (outcomes, _) = {
        let order = select(&file, &[]).unwrap();
        let mut opts = options(1);
        opts.mode = Mode::Diff;
        let runner = Runner::new(&file, &opts);
        let outcomes: HashMap<_, _> = runner.run(&order).into_iter().collect();
        (outcomes, ())
    };
    assert_eq!(
        outcomes["files"],
        Outcome::Ran,
        "name=changed still differs"
    );
}

#[cfg(unix)]
#[test]
fn pull_never_writes_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display().to_string();
    crate::ui::add_redactions([&"hunter2-secret".to_string()]);
    let main = dir.path().join("main.task");
    let source = format!("task t {{\n file \"{d}/s.conf\" \"a\\n\"\n}}\n");
    std::fs::write(&main, &source).unwrap();
    std::fs::write(dir.path().join("s.conf"), "a\npassword=hunter2-secret\n").unwrap();
    let file = crate::parser::parse_file(&main).unwrap();
    let order = select(&file, &[]).unwrap();
    let mut opts = options(1);
    opts.pull = true;
    opts.yes = true;
    let runner = Runner::new(&file, &opts);
    runner.run(&order);
    let edits = std::mem::take(&mut *runner.edits.lock().unwrap());
    super::pull::apply(&edits);
    assert!(!std::fs::read_to_string(&main).unwrap().contains("hunter2"));
}

/// Check `source` with `--no-secrets` if `no_secrets`, and return the `--json` document.
#[cfg(unix)]
fn check_json(
    source: &str,
    dir: &Path,
    mode: Mode,
    setup: impl FnOnce(&mut TaskFile),
) -> serde_json::Value {
    let mut file = parse_str(source, dir).unwrap_or_else(|e| panic!("{e:#}"));
    setup(&mut file);
    let mut opts = options(1);
    opts.mode = mode;
    let runner = Runner::new(&file, &opts);
    let outcomes = runner.run(&select(&file, &[]).unwrap());
    json::document(&dir.join("main.task"), &runner, &outcomes, None)
}

#[cfg(unix)]
fn task<'a>(document: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    document["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("no task {name} in {document:#}"))
}

#[cfg(unix)]
#[test]
fn json_reports_each_outcome() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("same"), "same\n").unwrap();
    std::fs::write(dir.path().join("differs"), "a\nb\nc\n").unwrap();
    // A fake sops that would leave a mark: --no-secrets must never start it.
    let fake = dir.path().join("fake-sops");
    crate::test_util::write_script(
        &fake,
        "#!/bin/sh\ntouch \"$(dirname \"$4\")/decrypted\"\ncat \"$4\"\n",
    );
    std::fs::write(
        dir.path().join("secrets.json"),
        r#"{"token": "tok-123456"}"#,
    )
    .unwrap();
    let source = r#"
        secrets "secrets.json"
        task current {
            file "same" "same\n"
        }
        task changes {
            file "differs" "a\nB\nc\n"
            run echo hi
        }
        task guarded {
            if false
            run echo never
        }
        task broken {
            file "out" from "missing-source"
        }
        task after-broken {
            needs broken
            run true
        }
        task secret-guard {
            if test "{{ secrets.token }}" = x
            run true
        }
        subtask secret-sub of secret-guard {
            run true
        }
        task secret-step {
            file "same" "same\n"
            file "token" "{{ secrets.token }}"
        }
    "#;
    let document = check_json(source, dir.path(), Mode::Diff, |file| {
        file.ctx.secrets.set_program(fake.display().to_string());
        file.ctx.secrets.disable();
    });
    assert!(!dir.path().join("decrypted").exists(), "sops ran");
    assert_eq!(document["version"], 1);
    assert_eq!(document["mode"], "diff");
    assert!(document["error"].is_null());

    let current = task(&document, "current");
    assert_eq!(current["outcome"], "up-to-date");
    assert_eq!(current["line"], 3);
    assert!(
        current["file"].as_str().unwrap().ends_with("<string>"),
        "{current}"
    );
    assert_eq!(current["steps"][0]["kind"], "ok");

    let changes = task(&document, "changes");
    assert_eq!(changes["outcome"], "would-change");
    let file = &changes["steps"][0];
    assert_eq!(file["kind"], "change");
    assert!(
        file["label"]
            .as_str()
            .unwrap()
            .ends_with("differs · content"),
        "{file}"
    );
    assert_eq!(
        file["diff"],
        serde_json::json!(["@@ -1,3 +1,3 @@", " a", "-b", "+B", " c"])
    );
    assert_eq!(
        changes["steps"][1],
        serde_json::json!({"depth": 0, "kind": "run", "label": "echo hi"})
    );

    let guarded = task(&document, "guarded");
    assert_eq!(guarded["outcome"], "skipped");
    assert_eq!(guarded["reason"], "if false");

    let broken = task(&document, "broken");
    assert_eq!(broken["outcome"], "failed");
    assert!(
        broken["reason"]
            .as_str()
            .unwrap()
            .contains("missing-source"),
        "{broken}"
    );
    let blocked = task(&document, "after-broken");
    assert_eq!(blocked["outcome"], "blocked");
    assert_eq!(blocked["reason"], "broken");

    let guard = task(&document, "secret-guard");
    assert_eq!(guard["outcome"], "unchecked");
    assert!(
        guard["reason"].as_str().unwrap().contains("secret 'token'"),
        "{guard}"
    );
    let sub = task(&document, "secret-sub");
    assert_eq!(sub["outcome"], "skipped");
    assert_eq!(sub["reason"], "secret-guard wasn't checked");

    let step = task(&document, "secret-step");
    assert_eq!(step["outcome"], "up-to-date");
    assert_eq!(step["unchecked"], 1);
    assert_eq!(step["steps"][1]["kind"], "unchecked");
    assert_eq!(step["steps"][1]["label"], "file token");

    // Without --no-secrets, the same file decrypts.
    let document = check_json(source, dir.path(), Mode::Diff, |file| {
        file.ctx.secrets.set_program(fake.display().to_string());
    });
    assert!(dir.path().join("decrypted").exists());
    assert_eq!(task(&document, "secret-guard")["outcome"], "skipped");
    assert_eq!(task(&document, "secret-step")["outcome"], "would-change");
}

#[cfg(unix)]
#[test]
fn json_dry_runs_and_nested_steps() {
    let dir = tempfile::tempdir().unwrap();
    let source = r#"
        template make {
            unless test -e {{ 1 }}
            run touch {{ 1 }}
        }
        task t {
            make here
            make "$(pwd)"
            if true {
                run echo inside
            }
        }
    "#;
    let document = check_json(source, dir.path(), Mode::DryRun, |_| {});
    assert_eq!(document["mode"], "dry-run");
    let steps = &task(&document, "t")["steps"];
    let kinds: Vec<(u64, &str, &str)> = steps
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["depth"].as_u64().unwrap(),
                s["kind"].as_str().unwrap(),
                s["label"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            (0, "change", "make here"),
            (1, "run", "touch here"),
            (0, "ok", "make \"$(pwd)\""),
            (0, "change", "if true"),
            (1, "run", "echo inside"),
        ]
    );
}

#[cfg(unix)]
#[test]
fn json_redacts_secrets_and_colors() {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("fake-sops");
    crate::test_util::write_script(&fake, "#!/bin/sh\ncat \"$4\"\n");
    std::fs::write(
        dir.path().join("secrets.json"),
        r#"{"key": "json-secret-987"}"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("conf"), "key=old-value\nother\n").unwrap();
    let source = r#"
        secrets "secrets.json"
        task conf {
            file "conf" "key={{ secrets.key }}\nother\n"
        }
    "#;
    let document = check_json(source, dir.path(), Mode::Diff, |file| {
        file.ctx.secrets.set_program(fake.display().to_string());
    });
    let text = document.to_string();
    assert!(!text.contains("json-secret-987"), "{text}");
    assert!(!text.contains("old-value"), "{text}");
    assert!(!text.contains('\u{1b}'), "{text}");
    let steps = &task(&document, "conf")["steps"];
    assert_eq!(steps[0]["diff"][1], "-••••");
    assert_eq!(steps[0]["diff"][2], "+key=••••");
    assert_eq!(steps[1]["kind"], "note");
}

#[cfg(unix)]
#[test]
fn compares_and_pulls_attributes() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display().to_string();
    let main = dir.path().join("main.task");
    let source = format!(
        r#"task t {{
    file "{d}/enc" encoding utf-16le "hi\n"
    file "{d}/crlf" "a\nb\n"
    file "{d}/nl" "x"
    file "{d}/link" "same\n"
    file "{d}/owned" owner root "o\n"
    file "{d}/both" "q\n"
}}
"#
    );
    std::fs::write(&main, &source).unwrap();
    let set = |name: &str, bytes: &[u8], mode: u32| {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    set("enc", b"hi\n", 0o644);
    set("crlf", b"a\r\nb\r\n", 0o644);
    set("nl", b"x\n", 0o644);
    set("target", b"same\n", 0o644);
    std::os::unix::fs::symlink(dir.path().join("target"), dir.path().join("link")).unwrap();
    set("owned", b"o\n", 0o644);
    set("both", &Encoding::Utf16Le.encode("q\n").unwrap(), 0o640);

    let file = crate::parser::parse_file(&main).unwrap();
    let document = check_json("", dir.path(), Mode::Diff, |f| *f = file);
    let labels: Vec<String> = task(&document, "t")["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["kind"] == "change")
        .map(|s| s["label"].as_str().unwrap().replace(&d, ""))
        .collect();
    let me = std::process::Command::new("id")
        .arg("-un")
        .output()
        .unwrap();
    let me = String::from_utf8(me.stdout).unwrap().trim().to_string();
    assert_eq!(labels.len(), 5, "{labels:?}");
    assert_eq!(labels[0], "file /enc · encoding utf-8 → utf-16le");
    assert_eq!(labels[1], "file /crlf · line endings CRLF → LF");
    assert_eq!(labels[2], "file /nl · removes the final newline");
    // The symlink's target matches, mode included: no change for /link.
    assert!(
        labels[3].starts_with(&format!("file /owned · owner {me}:")),
        "{labels:?}"
    );
    assert!(labels[3].ends_with(" → root"), "{labels:?}");
    assert_eq!(
        labels[4],
        "file /both · encoding utf-16le → utf-8, mode 640 → 644"
    );

    // Pull it all back into the task file.
    let file = crate::parser::parse_file(&main).unwrap();
    let mut opts = options(1);
    opts.pull = true;
    opts.yes = true;
    let runner = Runner::new(&file, &opts);
    runner.run(&select(&file, &[]).unwrap());
    let edits = std::mem::take(&mut *runner.edits.lock().unwrap());
    let applied = pull::apply(&edits);
    assert!(applied.problems.is_empty(), "{:?}", applied.problems);
    let pulled = std::fs::read_to_string(&main).unwrap();
    assert!(
        pulled.contains(&format!(r#"file "{d}/enc" encoding utf-8 "hi\n""#)),
        "{pulled}"
    );
    assert!(
        pulled.contains(&format!(r#"file "{d}/owned" owner {me} "o\n""#)),
        "{pulled}"
    );
    assert!(
        pulled.contains(&format!(
            r#"file "{d}/both" encoding utf-16le mode 640 "q\n""#
        )),
        "{pulled}"
    );

    // And now the task file matches the system for those steps.
    let file = crate::parser::parse_file(&main).unwrap();
    let document = check_json("", dir.path(), Mode::Diff, |f| *f = file);
    let changed: Vec<String> = task(&document, "t")["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["kind"] == "change")
        .map(|s| s["label"].as_str().unwrap().replace(&d, ""))
        .collect();
    assert!(
        !changed
            .iter()
            .any(|l| l.contains("/enc") || l.contains("/owned") || l.contains("/both")),
        "{changed:?}"
    );
}

/// Undo (or preview undoing, with `mode`) `targets` of `source`.
#[cfg(unix)]
fn undo(source: &str, dir: &Path, mode: Mode, targets: &[&str]) -> HashMap<String, Outcome> {
    let file = parse_str(source, dir).unwrap_or_else(|e| panic!("{e:#}"));
    let targets: Vec<String> = targets.iter().map(|s| s.to_string()).collect();
    let mut opts = options(2);
    opts.mode = mode;
    opts.undo = true;
    opts.yes = true;
    let order = select_undo(&file, &targets).unwrap();
    Runner::new(&file, &opts).run(&order).into_iter().collect()
}

#[cfg(unix)]
#[test]
fn undo_puts_files_back() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = |n: &str| dir.path().join(n);
    std::fs::write(path("existing"), "before\n").unwrap();
    std::fs::set_permissions(path("existing"), std::fs::Permissions::from_mode(0o600)).unwrap();
    set_modified(&path("existing"), 1_600_000_000, 250_000_000);
    std::fs::write(path("unrecorded"), "theirs\n").unwrap();
    std::fs::write(path("kept"), "mine\n").unwrap();
    let source = r#"
        task files {
            file "existing" "managed\n"
            file "unrecorded" "managed\n"
            file "marked" new "x\n"
            file "kept" keep "managed\n"
        }
    "#;
    assert_eq!(
        run(source, dir.path(), &options(1), &[])["files"],
        Outcome::Ran
    );
    assert_eq!(read(dir.path(), "existing"), "managed\n");
    // As if these were written before the journal existed: `new` says
    // taskpod created it, and nothing is known about the other one.
    let journal = rusqlite::Connection::open(path(".taskpod-journal/user.db")).unwrap();
    journal
        .execute(
            "DELETE FROM files WHERE path LIKE '%/unrecorded' OR path LIKE '%/marked'",
            [],
        )
        .unwrap();
    drop(journal);
    let source = source.to_string();

    // A preview shows what would happen and changes nothing.
    let file = parse_str(&source, dir.path()).unwrap();
    let mut opts = options(1);
    opts.mode = Mode::Diff;
    opts.undo = true;
    let runner = Runner::new(&file, &opts);
    runner.run(&select_undo(&file, &[]).unwrap());
    let reports = runner.reports.lock().unwrap();
    let steps: Vec<(StepKind, String)> = reports["files"]
        .steps
        .iter()
        .map(|s| {
            (
                s.kind,
                s.label.replace(&dir.path().display().to_string(), ""),
            )
        })
        .collect();
    assert_eq!(
        steps,
        [
            (StepKind::Ok, "file /kept · kept".to_string()),
            (StepKind::Change, "file /marked · delete it, taskpod created it".to_string()),
            (StepKind::Note, "file /unrecorded: no record of what was there before taskpod wrote it; mark it `new` if taskpod created it, or `keep`".to_string()),
            (StepKind::Change, "file /existing · put back what was there before taskpod (mode 600)".to_string()),
        ]
    );
    assert_eq!(
        reports["files"].steps[3].diff.as_deref().unwrap(),
        ["@@ -1 +1 @@", "-managed", "+before"]
    );
    assert_eq!(reports["files"].cannot, 1);
    drop(reports);
    assert_eq!(read(dir.path(), "existing"), "managed\n");

    assert_eq!(
        undo(&source, dir.path(), Mode::Run, &[])["files"],
        Outcome::Ran
    );
    assert_eq!(read(dir.path(), "existing"), "before\n");
    let meta = std::fs::metadata(path("existing")).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            (meta.mtime(), meta.mtime_nsec()),
            (1_600_000_000, 250_000_000)
        );
    }
    assert!(!path("marked").exists());
    assert_eq!(read(dir.path(), "unrecorded"), "managed\n");
    assert_eq!(read(dir.path(), "kept"), "managed\n");
    // Undone files leave the journal (`kept` was never written: it stays
    // recorded); doing it again finds nothing to do.
    let journal = rusqlite::Connection::open(path(".taskpod-journal/user.db")).unwrap();
    let left: Vec<String> = journal
        .prepare("SELECT path FROM files")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    drop(journal);
    assert!(left.iter().all(|n| n.ends_with("kept")), "{left:?}");
    assert_eq!(
        undo(&source, dir.path(), Mode::Run, &[])["files"],
        Outcome::UpToDate
    );
}

#[cfg(unix)]
#[test]
fn undo_runs_undo_steps_in_reverse() {
    let dir = tempfile::tempdir().unwrap();
    let source = r#"
        template mk {
            unless test -e {{ 1 }}
            run touch {{ 1 }}
            undo rm {{ 1 }}
        }
        task base {
            run echo base >> forward
            undo echo base >> log
        }
        task app {
            needs base
            undo test -e app.conf || echo "first undo, after the file" >> log
            file "app.conf" "x\n"
            mk made-a
            mk made-b
            undo test -e app.conf && echo "last undo, runs first" >> log
        }
        subtask app-up of app {
            undo echo app-up >> log
        }
        task stuck {
            run echo no undo here
        }
        task left {
            keep
            run echo left alone
        }
        task elsewhere {
            if false
            undo echo never >> log
        }
    "#;
    let outcomes = run(source, dir.path(), &options(1), &[]);
    assert_eq!(outcomes["app"], Outcome::Ran);
    std::fs::remove_file(dir.path().join("made-b")).unwrap();

    let outcomes = undo(source, dir.path(), Mode::Run, &[]);
    assert_eq!(outcomes["app"], Outcome::Ran, "{outcomes:?}");
    assert_eq!(outcomes["stuck"], Outcome::UpToDate);
    assert_eq!(outcomes["left"], Outcome::Skipped("keep".into()));
    assert_eq!(outcomes["elsewhere"], Outcome::Skipped("if false".into()));
    // Dependents before what they need, subtasks before their parent, and
    // in a task, the last step first.
    assert_eq!(
        read(dir.path(), "log"),
        "app-up\nlast undo, runs first\nfirst undo, after the file\nbase\n"
    );
    assert!(!dir.path().join("made-a").exists());
    assert!(!dir.path().join("app.conf").exists());

    // With --run: the task and its subtasks, not what it needs.
    std::fs::remove_file(dir.path().join("log")).unwrap();
    let outcomes = undo(source, dir.path(), Mode::Run, &["app"]);
    assert_eq!(outcomes.keys().len(), 2, "{outcomes:?}");
    assert!(outcomes.contains_key("app-up"));
}

#[cfg(unix)]
#[test]
fn undo_notices_later_edits() {
    let dir = tempfile::tempdir().unwrap();
    let source = r#"
        task t {
            file "conf" "managed\n"
        }
    "#;
    run(source, dir.path(), &options(1), &[]);
    std::fs::write(dir.path().join("conf"), "edited by hand\n").unwrap();
    let file = parse_str(source, dir.path()).unwrap();
    let mut opts = options(1);
    opts.mode = Mode::DryRun;
    opts.undo = true;
    let runner = Runner::new(&file, &opts);
    runner.run(&select_undo(&file, &[]).unwrap());
    let label = runner.reports.lock().unwrap()["t"].steps[0].label.clone();
    assert!(
        label.ends_with("delete it, taskpod created it · changed since taskpod wrote it"),
        "{label}"
    );
    // --yes undoes it anyway.
    assert_eq!(undo(source, dir.path(), Mode::Run, &[])["t"], Outcome::Ran);
    assert!(!dir.path().join("conf").exists());
}

#[test]
fn undo_keeps_what_others_need() {
    let file = parse_str(
        "task c {\n run true\n}\ntask a {\n needs c\n run true\n}\ntask b {\n needs c\n run true\n}\ntask m {\n manual\n needs c\n run true\n}\n",
        Path::new("/tmp"),
    )
    .unwrap();
    // Undoing a leaves c, which b still needs.
    assert_eq!(select_undo(&file, &["a".into()]).unwrap(), ["a"]);
    // c alone would break a and b (manual tasks don't count).
    let err = select_undo(&file, &["c".into()]).unwrap_err().to_string();
    assert!(err.contains("a needs c, b needs c"), "{err}");
    assert!(err.contains("-u -r c a b"), "{err}");
    let all = select_undo(&file, &["c".into(), "a".into(), "b".into()]).unwrap();
    assert_eq!(all, ["c", "a", "b"]);

    // A task that needs several of them is named once.
    let file = parse_str(
        "task c {\n run true\n}\ntask d {\n run true\n}\ntask a {\n needs c d\n run true\n}\ntask b {\n needs c\n run true\n}\n",
        Path::new("/tmp"),
    )
    .unwrap();
    let err = select_undo(&file, &["c".into(), "d".into()])
        .unwrap_err()
        .to_string();
    assert!(err.ends_with("-u -r c d a b"), "{err}");
}

#[cfg(unix)]
#[test]
fn copies_directories() {
    let dir = tempfile::tempdir().unwrap();
    let path = |n: &str| dir.path().join(n);
    std::fs::create_dir_all(path("files/rules/nested")).unwrap();
    std::fs::write(path("files/rules/a.json"), "a\n").unwrap();
    std::fs::write(path("files/rules/{{ b }}.json"), "b\n").unwrap();
    std::fs::write(path("files/rules/nested/c.sh"), "#!/bin/sh\n").unwrap();
    std::fs::create_dir_all(path("out")).unwrap();
    std::fs::write(path("out/theirs.json"), "theirs\n").unwrap();
    // Not in a directory the source has: an app's data, never pulled.
    std::fs::create_dir_all(path("out/data")).unwrap();
    std::fs::write(path("out/data/db"), "data\n").unwrap();
    let source = r#"
        task rules {
            directory "out" new from "files/rules"
            file "out/inline.env" new "inline\n"
        }
    "#;
    assert_eq!(
        run(source, dir.path(), &options(1), &[])["rules"],
        Outcome::Ran
    );
    assert_eq!(read(dir.path(), "out/a.json"), "a\n");
    assert_eq!(read(dir.path(), "out/{{ b }}.json"), "b\n");
    assert_eq!(read(dir.path(), "out/nested/c.sh"), "#!/bin/sh\n");
    assert_eq!(
        run(source, dir.path(), &options(1), &[])["rules"],
        Outcome::UpToDate
    );

    // --pull brings changes back to the file they came from.
    std::fs::write(path("out/a.json"), "changed\n").unwrap();
    let main = path("main.task");
    std::fs::write(&main, source).unwrap();
    let file = crate::parser::parse_file(&main).unwrap();
    let mut opts = options(1);
    opts.pull = true;
    opts.yes = true;
    let runner = Runner::new(&file, &opts);
    runner.run(&select(&file, &[]).unwrap());
    let edits = runner.edits.lock().unwrap();
    let applied = pull::apply(&edits);
    assert!(applied.problems.is_empty(), "{:?}", applied.problems);
    assert_eq!(read(dir.path(), "files/rules/a.json"), "changed\n");
    // Files only on the system are pulled in too, unless they hold a secret.
    assert_eq!(read(dir.path(), "files/rules/theirs.json"), "theirs\n");
    // But not files another step of the task writes.
    assert!(!path("files/rules/inline.env").exists());
    assert!(!path("files/rules/data").exists());
    drop(edits);

    // Undo removes what it wrote, and leaves the system's own files.
    std::fs::write(path("out/later.json"), "later\n").unwrap();
    undo(source, dir.path(), Mode::Run, &[]);
    assert!(!path("out/a.json").exists());
    assert!(!path("out/nested/c.sh").exists());
    assert_eq!(read(dir.path(), "out/later.json"), "later\n");
}

#[test]
fn directories_are_copied_from_directories() {
    let dir = tempfile::tempdir().unwrap();
    let err = parse_str("task t {\n directory \"/a\" \"x\"\n}\n", dir.path())
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("copied `from`"), "{err}");
    // A `file` from a directory says to use `directory`.
    std::fs::create_dir(dir.path().join("src")).unwrap();
    let source = "task t {\n file \"out\" from \"src/\"\n}\n";
    let outcome = &run(source, dir.path(), &options(1), &[])["t"];
    assert!(
        matches!(outcome, Outcome::Failed(e) if e.contains("copy it with `directory`")),
        "{outcome:?}"
    );
}

#[cfg(unix)]
#[test]
fn lines_are_added_and_undone() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = |n: &str| dir.path().join(n);
    std::fs::write(
        path("pam"),
        "#%PAM-1.0\n  auth include system\nauth x\naccount y\n",
    )
    .unwrap();
    std::fs::set_permissions(path("pam"), std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(path("rc"), "alias a=b").unwrap();
    std::fs::write(path("conf"), "[a]\nx=1\n[b]\n").unwrap();
    let source = r#"
        let script = "funcs.sh"
        task lines {
            line "pam" before "auth" "auth sufficient u2f"
            line "rc" ". {{ script }}"
            line "conf" after "x=" "y=2"
        }
    "#;
    // A diff shows it and changes nothing.
    let mut opts = options(1);
    opts.mode = Mode::Diff;
    assert_eq!(run(source, dir.path(), &opts, &[])["lines"], Outcome::Ran);
    assert_eq!(read(dir.path(), "rc"), "alias a=b");

    assert_eq!(
        run(source, dir.path(), &options(1), &[])["lines"],
        Outcome::Ran
    );
    assert_eq!(
        read(dir.path(), "pam"),
        "#%PAM-1.0\nauth sufficient u2f\n  auth include system\nauth x\naccount y\n"
    );
    let mode = std::fs::metadata(path("pam")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(read(dir.path(), "rc"), "alias a=b\n. funcs.sh\n");
    assert_eq!(read(dir.path(), "conf"), "[a]\nx=1\ny=2\n[b]\n");
    assert_eq!(
        run(source, dir.path(), &options(1), &[])["lines"],
        Outcome::UpToDate
    );

    // Undo takes the lines out, and only them.
    assert_eq!(
        undo(source, dir.path(), Mode::Run, &[])["lines"],
        Outcome::Ran
    );
    assert_eq!(
        read(dir.path(), "pam"),
        "#%PAM-1.0\n  auth include system\nauth x\naccount y\n"
    );
    assert_eq!(read(dir.path(), "rc"), "alias a=b\n");
    assert_eq!(read(dir.path(), "conf"), "[a]\nx=1\n[b]\n");
    assert_eq!(
        undo(source, dir.path(), Mode::Run, &[])["lines"],
        Outcome::UpToDate
    );

    // Nowhere to put it, or no file: the step fails.
    let source =
        "task t {\n line \"conf\" before \"nope\" \"z\"\n}\ntask u {\n line \"missing\" \"z\"\n}\n";
    let outcomes = run(source, dir.path(), &options(1), &[]);
    assert!(
        matches!(&outcomes["t"], Outcome::Failed(e) if e.contains("no line starts with \"nope\"")),
        "{outcomes:?}"
    );
    assert!(
        matches!(&outcomes["u"], Outcome::Failed(e) if e.contains("doesn't exist")),
        "{outcomes:?}"
    );
}

#[cfg(unix)]
#[test]
fn lines_over_ssh() {
    let dir = tempfile::tempdir().unwrap();
    let (remote, home) = fake_remote(dir.path());
    std::fs::write(home.join(".bashrc"), "export A=1\n").unwrap();
    let source = "task t {\n line \"~/.bashrc\" \". ~/f.sh\"\n}\n";
    assert_eq!(
        run_remote(source, dir.path(), &remote, &options(1))["t"],
        Outcome::Ran
    );
    assert_eq!(read(&home, ".bashrc"), "export A=1\n. ~/f.sh\n");
    assert_eq!(
        run_remote(source, dir.path(), &remote, &options(1))["t"],
        Outcome::UpToDate
    );
}

#[test]
fn line_steps_parse() {
    let err = |source: &str| match parse_str(source, Path::new("/tmp")) {
        Ok(_) => panic!("parsed: {source}"),
        Err(e) => e.to_string(),
    };
    assert!(err("task t {\n line \"/a\" \"x\\ny\"\n}\n").contains("newline"));
    assert!(err("task t {\n line \"/a\" \" \"\n}\n").contains("empty"));
    assert!(err("task t {\n line \"/a\" before \"a\" after \"b\" \"x\"\n}\n").contains("only one"));
    assert!(err("task t {\n line \"/a\" mode 600 \"x\"\n}\n").contains("unexpected 'mode'"));
    let file = parse_str(
        "task t {\n line \"/a\" sudo keep after \"b\" \"x\"\n}\n",
        Path::new("/tmp"),
    )
    .unwrap();
    let crate::ast::Step::Line(line) = &file.tasks["t"].body.steps[0] else {
        panic!()
    };
    assert!(line.sudo && line.keep);
    assert!(matches!(&line.place, Some(crate::ast::LinePlace::After(p)) if p == "b"));
    assert_eq!(line.text, "x");
}

#[cfg(unix)]
#[test]
fn lines_start_from_a_base() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = |n: &str| dir.path().join(n);
    std::fs::create_dir(path("vendor")).unwrap();
    std::fs::write(path("vendor/kde"), "#%PAM-1.0\nauth\tinclude  login\n").unwrap();
    std::fs::set_permissions(path("vendor/kde"), std::fs::Permissions::from_mode(0o640)).unwrap();
    let source = r#"
        task pam {
            line "etc/kde" base "vendor/kde" before "auth" "auth sufficient u2f"
        }
    "#;
    assert_eq!(
        run(source, dir.path(), &options(1), &[])["pam"],
        Outcome::Ran
    );
    assert_eq!(
        read(dir.path(), "etc/kde"),
        "#%PAM-1.0\nauth sufficient u2f\nauth\tinclude  login\n"
    );
    let mode = std::fs::metadata(path("etc/kde"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o640);
    // Lined up differently, it's the same line.
    std::fs::write(
        path("etc/kde"),
        "#%PAM-1.0\nauth   sufficient\tu2f\nauth\tinclude  login\n",
    )
    .unwrap();
    assert_eq!(
        run(source, dir.path(), &options(1), &[])["pam"],
        Outcome::UpToDate
    );
    // Undone, it's the base again, so it goes.
    assert_eq!(
        undo(source, dir.path(), Mode::Run, &[])["pam"],
        Outcome::Ran
    );
    assert!(!path("etc/kde").exists());
    // With other changes, only the line goes.
    run(source, dir.path(), &options(1), &[]);
    std::fs::write(
        path("etc/kde"),
        "#%PAM-1.0\nauth sufficient u2f\nauth\tinclude  login\nmine\n",
    )
    .unwrap();
    undo(source, dir.path(), Mode::Run, &[]);
    assert_eq!(
        read(dir.path(), "etc/kde"),
        "#%PAM-1.0\nauth\tinclude  login\nmine\n"
    );
}

#[cfg(unix)]
#[test]
fn lists_directories_over_ssh() {
    let dir = tempfile::tempdir().unwrap();
    let (remote, home) = fake_remote(dir.path());
    std::fs::create_dir_all(home.join("rules/sub")).unwrap();
    std::fs::write(home.join("rules/b"), "").unwrap();
    std::fs::write(home.join("rules/sub/a"), "").unwrap();
    std::fs::create_dir_all(home.join("rules/data/deeper")).unwrap();
    std::fs::write(home.join("rules/data/deeper/db"), "").unwrap();
    let mut file = parse_str("", dir.path()).unwrap();
    file.ctx.remote = Some(remote);
    let list = |p: &Path, subdirs: &[&str]| {
        let subdirs: Vec<String> = subdirs.iter().map(|s| s.to_string()).collect();
        super::target::list(&file.ctx, "t", p, &subdirs, false).unwrap()
    };
    // Only the directories asked for, each without what's below it.
    assert_eq!(
        list(&home.join("rules"), &["", "sub", "missing"]),
        ["b", "sub/a"]
    );
    assert_eq!(list(&home.join("rules"), &[""]), ["b"]);
    assert!(list(&home.join("nothing"), &[""]).is_empty());
}

/// Run an undo and return the task's report too.
fn undo_report(source: &str, dir: &Path, task: &str) -> (Outcome, TaskReport) {
    let file = parse_str(source, dir).unwrap_or_else(|e| panic!("{e:#}"));
    let mut opts = options(1);
    opts.undo = true;
    opts.yes = true;
    let runner = Runner::new(&file, &opts);
    let outcomes: HashMap<_, _> = runner
        .run(&select_undo(&file, &[]).unwrap())
        .into_iter()
        .collect();
    let report = runner.reports.lock().unwrap().remove(task).unwrap();
    (outcomes[task].clone(), report)
}

#[cfg(unix)]
#[test]
fn undo_only_takes_out_lines_taskpod_added() {
    let dir = tempfile::tempdir().unwrap();
    let path = |n: &str| dir.path().join(n);
    // Already there before taskpod: undo leaves it, and says so.
    std::fs::write(path("rc"), "a\n. f.sh\n").unwrap();
    let source = "task t {\n line \"rc\" \". f.sh\"\n}\n";
    assert_eq!(
        run(source, dir.path(), &options(1), &[])["t"],
        Outcome::UpToDate
    );
    let (_, report) = undo_report(source, dir.path(), "t");
    assert_eq!(report.cannot, 1);
    assert_eq!(read(dir.path(), "rc"), "a\n. f.sh\n");
    // `new` says taskpod added it.
    let source = "task t {\n line \"rc\" new \". f.sh\"\n}\n";
    undo(source, dir.path(), Mode::Run, &[]);
    assert_eq!(read(dir.path(), "rc"), "a\n");

    // Added by taskpod, then a copy by hand: undo takes out one.
    run(source, dir.path(), &options(1), &[]);
    std::fs::write(path("rc"), "a\n. f.sh\n. f.sh\n").unwrap();
    undo(source, dir.path(), Mode::Run, &[]);
    assert_eq!(read(dir.path(), "rc"), "a\n. f.sh\n");
}

#[cfg(unix)]
#[test]
fn matched_lines_are_replaced_and_put_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = |n: &str| dir.path().join(n);
    std::fs::write(path("pam"), "#%PAM\nauth u2f.so old\nauth include x\n").unwrap();
    let source = r#"
        task t {
            line "pam" match "auth u2f.so" before "auth" "auth u2f.so cue"
        }
    "#;
    assert_eq!(run(source, dir.path(), &options(1), &[])["t"], Outcome::Ran);
    assert_eq!(
        read(dir.path(), "pam"),
        "#%PAM\nauth u2f.so cue\nauth include x\n"
    );
    // Edited by hand: put right again, not added a second time.
    std::fs::write(path("pam"), "#%PAM\nauth u2f.so mine\nauth include x\n").unwrap();
    assert_eq!(run(source, dir.path(), &options(1), &[])["t"], Outcome::Ran);
    assert_eq!(
        read(dir.path(), "pam"),
        "#%PAM\nauth u2f.so cue\nauth include x\n"
    );
    // Undo puts back what was there before taskpod.
    undo(source, dir.path(), Mode::Run, &[]);
    assert_eq!(
        read(dir.path(), "pam"),
        "#%PAM\nauth u2f.so old\nauth include x\n"
    );

    // Edited after taskpod wrote it, undo still finds it (and asks; --yes here).
    run(source, dir.path(), &options(1), &[]);
    std::fs::write(path("pam"), "#%PAM\nauth u2f.so edited\nauth include x\n").unwrap();
    let (outcome, report) = undo_report(source, dir.path(), "t");
    assert_eq!(outcome, Outcome::Ran);
    assert!(
        report
            .steps
            .iter()
            .any(|s| s.label.contains("changed since taskpod wrote it")),
        "{:?}",
        report.steps
    );
    assert_eq!(
        read(dir.path(), "pam"),
        "#%PAM\nauth u2f.so old\nauth include x\n"
    );
}

/// A `match` for a commented-out default only finds it once: the journal's
/// record of what taskpod wrote finds the line after that, so a new text
/// replaces it rather than adding a copy at the end (in another section).
#[cfg(unix)]
#[test]
fn a_replaced_default_is_found_again_when_the_text_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = |n: &str| dir.path().join(n);
    let before = "[options]\n#IgnorePkg   =\nColor\n\n[multilib]\nInclude = x\n";
    std::fs::write(path("pacman.conf"), before).unwrap();
    let step = |pkgs: &str| {
        format!(
            "task t {{\n line \"pacman.conf\" match \"#IgnorePkg\" \"IgnorePkg = {pkgs}\"\n}}\n"
        )
    };
    let after =
        |pkgs: &str| format!("[options]\nIgnorePkg = {pkgs}\nColor\n\n[multilib]\nInclude = x\n");
    run(&step("a"), dir.path(), &options(1), &[]);
    assert_eq!(read(dir.path(), "pacman.conf"), after("a"));
    run(&step("a b"), dir.path(), &options(1), &[]);
    assert_eq!(read(dir.path(), "pacman.conf"), after("a b"));
    // Undo still puts back the commented-out default.
    undo(&step("a b"), dir.path(), Mode::Run, &[]);
    assert_eq!(read(dir.path(), "pacman.conf"), before);

    // Edited by hand to the step's next text: in the file, and recorded as
    // that, so the change after it is found too.
    run(&step("a"), dir.path(), &options(1), &[]);
    std::fs::write(path("pacman.conf"), after("a b")).unwrap();
    run(&step("a b"), dir.path(), &options(1), &[]);
    assert_eq!(read(dir.path(), "pacman.conf"), after("a b"));
    run(&step("a b c"), dir.path(), &options(1), &[]);
    assert_eq!(read(dir.path(), "pacman.conf"), after("a b c"));
    undo(&step("a b c"), dir.path(), Mode::Run, &[]);
    assert_eq!(read(dir.path(), "pacman.conf"), before);
}

#[cfg(unix)]
#[test]
fn lines_move_to_their_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = |n: &str| dir.path().join(n);
    std::fs::write(path("pam"), "auth include x\nauth u2f\naccount y").unwrap();
    let source = "task t {\n line \"pam\" before \"auth\" \"auth u2f\"\n}\n";
    assert_eq!(run(source, dir.path(), &options(1), &[])["t"], Outcome::Ran);
    assert_eq!(
        read(dir.path(), "pam"),
        "auth u2f\nauth include x\naccount y"
    );
    assert_eq!(
        run(source, dir.path(), &options(1), &[])["t"],
        Outcome::UpToDate
    );
    // Moved, not added: it was there before taskpod, so undo leaves it.
    let (_, report) = undo_report(source, dir.path(), "t");
    assert_eq!(report.cannot, 1);

    let source = "task t {\n line \"pam\" after \"account\" \"session z\"\n}\n";
    run(source, dir.path(), &options(1), &[]);
    assert_eq!(
        read(dir.path(), "pam"),
        "auth u2f\nauth include x\naccount y\nsession z\n"
    );
}

#[cfg(unix)]
#[test]
fn changed_runs_only_after_a_change() {
    let dir = tempfile::tempdir().unwrap();
    // A template's own `changed` sees the steps before the call, in the
    // calling task, and a nested block sees the steps before the outer one.
    let source = r#"
        template up {
            changed
            run echo up {{ 1 }} >> log
            undo echo down {{ 1 }} >> log
        }
        task stack {
            file "app.conf" "v1\n"
            up app
        }
        task nested {
            file "other.conf" "v1\n"
            if true {
                changed {
                    run echo nested >> log
                }
            }
        }
        task inside {
            changed {
                run echo never >> log
            }
            if true {
                run echo inside >> log
                changed {
                    run echo after inside >> log
                }
            }
        }
    "#;
    let outcomes = run(source, dir.path(), &options(1), &[]);
    assert_eq!(outcomes["stack"], Outcome::Ran);
    let log = read(dir.path(), "log");
    assert!(log.contains("up app\n"), "{log}");
    assert!(log.contains("nested\n"), "{log}");
    assert!(log.contains("after inside\n"), "{log}");
    assert!(!log.contains("never"), "{log}");

    // Up to date: nothing changed, so nothing after it runs either.
    std::fs::remove_file(dir.path().join("log")).unwrap();
    let outcomes = run(source, dir.path(), &options(1), &["stack", "nested"]);
    assert_eq!(outcomes["stack"], Outcome::UpToDate);
    assert_eq!(outcomes["nested"], Outcome::UpToDate);
    assert_eq!(read(dir.path(), "log"), "");

    // A diff shows the change and what would run after it, without running it.
    std::fs::write(dir.path().join("app.conf"), "edited\n").unwrap();
    let mut opts = options(1);
    opts.mode = Mode::Diff;
    let outcomes = run(source, dir.path(), &opts, &["stack"]);
    assert_eq!(outcomes["stack"], Outcome::Ran);
    assert_eq!(read(dir.path(), "log"), "");
    let outcomes = run(source, dir.path(), &options(1), &["stack"]);
    assert_eq!(outcomes["stack"], Outcome::Ran);
    assert_eq!(read(dir.path(), "log"), "up app\n");

    // Undo runs a `changed` block's undo where it stands, before the file goes.
    std::fs::remove_file(dir.path().join("log")).unwrap();
    let outcomes = undo(source, dir.path(), Mode::Run, &["stack"]);
    assert_eq!(outcomes["stack"], Outcome::Ran);
    assert_eq!(read(dir.path(), "log"), "down app\n");
    assert!(!dir.path().join("app.conf").exists());
}

#[test]
fn changed_needs_something_before_it() {
    let err = parse_str("task a {\n changed\n run true\n}\n", Path::new("/tmp"))
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("nothing before it"), "{err:#}");
    assert!(parse_str("template t {\n changed\n run true\n}\n", Path::new("/tmp")).is_ok());
    let err = parse_str("template changed {\n run true\n}\n", Path::new("/tmp"))
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("keyword"), "{err:#}");
}

#[cfg(unix)]
#[test]
fn directories_without_from_are_made_and_undone() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = |n: &str| dir.path().join(n);
    let mode = |n: &str| std::fs::metadata(path(n)).unwrap().permissions().mode() & 0o7777;
    std::fs::create_dir(path("theirs")).unwrap();
    std::fs::set_permissions(path("theirs"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let source = r#"
        task dirs {
            directory "made/deep" mode 750
            directory "theirs"
            directory "data" keep
        }
    "#;
    assert_eq!(
        run(source, dir.path(), &options(1), &[])["dirs"],
        Outcome::Ran
    );
    assert_eq!(mode("made/deep"), 0o750);
    assert_eq!(mode("theirs"), 0o755);
    assert!(path("data").is_dir());
    assert_eq!(
        run(source, dir.path(), &options(1), &[])["dirs"],
        Outcome::UpToDate
    );

    // A diff notices the mode.
    std::fs::set_permissions(path("made/deep"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut opts = options(1);
    opts.mode = Mode::Diff;
    assert_eq!(run(source, dir.path(), &opts, &[])["dirs"], Outcome::Ran);
    assert_eq!(mode("made/deep"), 0o700);
    assert_eq!(
        run(source, dir.path(), &options(1), &[])["dirs"],
        Outcome::Ran
    );
    assert_eq!(mode("made/deep"), 0o750);

    // Undo: a directory taskpod made goes if it's empty, one that was there
    // gets its mode back, and a kept one stays.
    std::fs::write(path("data/db"), "").unwrap();
    let (outcome, report) = undo_report(source, dir.path(), "dirs");
    assert_eq!(outcome, Outcome::Ran);
    assert_eq!(report.cannot, 0);
    assert!(!path("made/deep").exists());
    assert!(path("made").is_dir(), "only the step's own directory goes");
    assert_eq!(mode("theirs"), 0o700);
    assert!(path("data/db").exists());

    // Not empty: left, and reported.
    assert_eq!(
        run(source, dir.path(), &options(1), &[])["dirs"],
        Outcome::Ran
    );
    std::fs::write(path("made/deep/file"), "").unwrap();
    let (_, report) = undo_report(source, dir.path(), "dirs");
    assert_eq!(report.cannot, 1);
    assert!(path("made/deep/file").exists());

    // Something else in the way is an error.
    std::fs::write(path("plain"), "").unwrap();
    let outcome = &run(
        "task t {\n directory \"plain\"\n}\n",
        dir.path(),
        &options(1),
        &[],
    )["t"];
    assert!(
        matches!(outcome, Outcome::Failed(e) if e.contains("isn't a directory")),
        "{outcome:?}"
    );
}

#[test]
fn directories_without_from_parse() {
    let file = parse_str(
        "task t {\n directory \"/x\" sudo keep mode 750 owner a:b\n}\n",
        Path::new("/tmp"),
    )
    .unwrap();
    let Step::File(step) = &file.tasks["t"].body.steps[0] else {
        panic!()
    };
    assert!(step.dir && matches!(step.source, FileSource::Empty));
    assert_eq!(step.mode, Some(0o750));
    let err = parse_str(
        "task t {\n directory \"/x\" encoding latin1\n}\n",
        Path::new("/tmp"),
    )
    .err()
    .unwrap();
    assert!(format!("{err:#}").contains("no encoding"), "{err:#}");
    assert!(parse_str("task t {\n file \"/x\"\n}\n", Path::new("/tmp")).is_err());
}

#[cfg(unix)]
#[test]
fn makes_directories_over_ssh() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let (remote, home) = fake_remote(dir.path());
    let mut file = parse_str("", dir.path()).unwrap();
    file.ctx.remote = Some(remote);
    let made = home.join("a b/c");
    let state = |p: &Path| super::target::dir_state(&file.ctx, "t", p, false).unwrap();
    assert_eq!(state(&made), super::target::DirState::Missing);
    super::target::make_dir(&file.ctx, "t", &made, 0o750, None, false).unwrap();
    assert_eq!(
        std::fs::metadata(&made).unwrap().permissions().mode() & 0o7777,
        0o750
    );
    assert_eq!(state(&made), super::target::DirState::Empty);
    std::fs::write(made.join("f"), "").unwrap();
    assert_eq!(state(&made), super::target::DirState::NotEmpty);
    assert_eq!(state(&made.join("f")), super::target::DirState::NotDir);
    std::fs::remove_file(made.join("f")).unwrap();
    super::target::remove_dir(&file.ctx, "t", &made, false).unwrap();
    assert!(!made.exists());
}

#[test]
fn notices_are_reported_without_counting_as_changes() {
    let dir = tempfile::tempdir().unwrap();
    let source = r#"
        let what = "scratch"
        task quiet {
            unless false {
                notice "{{ what }} isn't mounted"
            }
            unless true {
                notice "never shown"
            }
        }
        task busy {
            run touch made
            notice "made it"
        }
    "#;
    let file = parse_str(source, dir.path()).unwrap();
    for mode in [Mode::Run, Mode::DryRun, Mode::Diff] {
        let mut opts = options(1);
        opts.mode = mode;
        let runner = Runner::new(&file, &opts);
        let outcomes: HashMap<_, _> = runner
            .run(&select(&file, &[]).unwrap())
            .into_iter()
            .collect();
        assert_eq!(outcomes["quiet"], Outcome::UpToDate, "{mode:?}");
        assert_eq!(outcomes["busy"], Outcome::Ran, "{mode:?}");
        let reports = runner.reports.lock().unwrap();
        assert_eq!(reports["quiet"].notices, ["scratch isn't mounted"]);
        assert_eq!(reports["busy"].notices, ["made it"]);
        assert!(
            reports["quiet"]
                .steps
                .iter()
                .any(|s| s.kind == StepKind::Note && s.label == "scratch isn't mounted")
        );
    }

    // Undo skips them, and they aren't commands it can't undo.
    let source = r#"
        task quiet {
            unless false {
                notice "not in an undo"
            }
        }
    "#;
    let file = parse_str(source, dir.path()).unwrap();
    let mut opts = options(1);
    opts.undo = true;
    let runner = Runner::new(&file, &opts);
    runner.run(&select_undo(&file, &[]).unwrap());
    let reports = runner.reports.lock().unwrap();
    assert!(reports["quiet"].notices.is_empty());
    assert_eq!(reports["quiet"].cannot, 0);
}

#[cfg(unix)]
#[test]
fn steps_that_want_the_terminal_fail_fast() {
    let dir = tempfile::tempdir().unwrap();
    // `kill -TTIN $$` stops the shell the way reading the terminal from the
    // background does. Tests have no terminal to hand over.
    let source = r#"
        task plain {
            run kill -TTIN $$; touch resumed
        }
        task asks {
            terminal
            run kill -TTOU $$; touch resumed
        }
        task block {
            if true {
                terminal
                run kill -TTIN $$
            }
        }
        task check {
            unless kill -TTIN $$
            run true
        }
        task fine {
            terminal
            run echo ok > fine
        }
    "#;
    let start = std::time::Instant::now();
    let outcomes = run(source, dir.path(), &options(4), &[]);
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
    let failed = |task: &str| match &outcomes[task] {
        Outcome::Failed(e) => e.clone(),
        other => panic!("{task}: {other:?}"),
    };
    let plain = failed("plain");
    assert!(plain.contains("sh wanted the terminal"), "{plain}");
    assert!(plain.contains("Add `terminal`"), "{plain}");
    for task in ["asks", "block"] {
        let error = failed(task);
        assert!(
            error.contains("taskpod has no terminal to give it"),
            "{error}"
        );
    }
    let check = failed("check");
    assert!(check.contains("checks can't ask"), "{check}");
    assert!(!dir.path().join("resumed").exists());
    assert_eq!(outcomes["fine"], Outcome::Ran);
    assert_eq!(read(dir.path(), "fine").trim(), "ok");
}

#[cfg(unix)]
#[test]
fn changed_reactions_run_again_after_an_undo() {
    let dir = tempfile::tempdir().unwrap();
    let source = r#"
        template restart {
            changed
            run test -e {{ 1 }}.conf && echo "{{ 1 }} with it" >> {{ 1 }}.log || echo "{{ 1 }} without it" >> {{ 1 }}.log
        }
        task inline {
            file "a.conf" new "v1\n"
            changed run test -e a.conf && echo with >> a.log || echo without >> a.log
        }
        task call {
            file "b.conf" new "v1\n"
            restart b
        }
        task kept {
            file "c.conf" new "v1\n"
            changed {
                keep
                run echo once >> c.log
            }
        }
    task ordered {
            undo echo removed >> d.log
            file "d.conf" new "v1\n"
            changed run echo restart >> d.log
            file "e.conf" new "v1\n"
            changed run echo again >> d.log
        }
    "#;
    let outcomes = run(source, dir.path(), &options(1), &[]);
    assert!(
        outcomes.values().all(|o| *o == Outcome::Ran),
        "{outcomes:?}"
    );
    assert_eq!(read(dir.path(), "a.log"), "with\n");
    assert_eq!(read(dir.path(), "b.log"), "b with it\n");

    // It runs again after the file goes, and says so.
    let (outcome, report) = undo_report(source, dir.path(), "inline");
    assert_eq!(outcome, Outcome::Ran);
    let labels: Vec<&str> = report.steps.iter().map(|s| s.label.as_str()).collect();
    assert!(
        labels[0].ends_with("delete it, taskpod created it"),
        "{labels:?}"
    );
    assert_eq!(labels[1], "changed · again, after the undo");
    assert_eq!(read(dir.path(), "a.log"), "with\nwithout\n");
    assert_eq!(read(dir.path(), "b.log"), "b with it\nb without it\n");
    // `keep`: it ran after the change, but not again in the undo.
    assert_eq!(read(dir.path(), "c.log"), "once\n");
    assert!(!dir.path().join("a.conf").exists());
    // Each one right after the files before it, back to the command before
    // them: so before what installed the service is undone.
    assert_eq!(
        read(dir.path(), "d.log"),
        "restart\nagain\nagain\nrestart\nremoved\n"
    );

    // Nothing left to undo: nothing runs again.
    let outcomes = undo(source, dir.path(), Mode::Run, &["inline"]);
    assert_eq!(outcomes["inline"], Outcome::UpToDate);
    assert_eq!(read(dir.path(), "a.log"), "with\nwithout\n");
}

#[cfg(unix)]
#[test]
fn else_runs_when_the_if_fails() {
    let dir = tempfile::tempdir().unwrap();
    let source = r#"
        task pick {
            if test -e flag {
                run echo yes > out
                undo echo undo yes >> log
            } else {
                run echo no > out
                undo echo undo no >> log
            }
        }
    "#;
    run(source, dir.path(), &options(1), &[]);
    assert_eq!(read(dir.path(), "out"), "no\n");
    undo(source, dir.path(), Mode::Run, &[]);
    assert_eq!(read(dir.path(), "log"), "undo no\n");
    std::fs::write(dir.path().join("flag"), "").unwrap();
    run(source, dir.path(), &options(1), &[]);
    assert_eq!(read(dir.path(), "out"), "yes\n");
    undo(source, dir.path(), Mode::Run, &[]);
    assert_eq!(read(dir.path(), "log"), "undo no\nundo yes\n");
}

#[test]
fn else_only_follows_if() {
    let parse = |source: &str| parse_str(source, Path::new("/tmp")).map(|_| ());
    let err =
        parse("task a {\n unless true {\n run true\n } else {\n run true\n }\n}\n").unwrap_err();
    assert!(
        format!("{err:#}").contains("as `if X { B } else { A }`"),
        "{err:#}"
    );
    let err = parse(
        "task a {\n if true {\n run true\n } else {\n run true\n } else {\n run true\n }\n}\n",
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("has no `else`"), "{err:#}");
    assert!(parse("task a {\n if true {\n run true\n }else{\n run true\n }\n}\n").is_ok());
    // A word starting with `else` isn't one.
    let err = parse("task a {\n if true {\n run true\n } elsewhere\n}\n").unwrap_err();
    assert!(
        format!("{err:#}").contains("expected the end of the line"),
        "{err:#}"
    );
}

#[cfg(unix)]
#[test]
fn settings_are_set_and_put_back() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("state"), "off\n").unwrap();
    let source = r#"
        task switch {
            setting "on" {
                get cat state
                set echo "{{ value }}" > state
            }
        }
        task broken {
            manual
            setting "on" {
                get echo off
                set true
            }
        }
    "#;
    let mut opts = options(1);
    opts.mode = Mode::Diff;
    let file = parse_str(source, dir.path()).unwrap();
    let runner = Runner::new(&file, &opts);
    runner.run(&["switch".to_string()]);
    let label = runner.reports.lock().unwrap()["switch"].steps[0]
        .label
        .clone();
    assert_eq!(label, "setting cat state · off → on");
    assert_eq!(read(dir.path(), "state"), "off\n");

    assert_eq!(
        run(source, dir.path(), &options(1), &[])["switch"],
        Outcome::Ran
    );
    assert_eq!(read(dir.path(), "state"), "on\n");
    assert_eq!(
        run(source, dir.path(), &options(1), &[])["switch"],
        Outcome::UpToDate
    );

    // Undo sets what was there before taskpod, and forgets it.
    assert_eq!(
        undo(source, dir.path(), Mode::Run, &["switch"])["switch"],
        Outcome::Ran
    );
    assert_eq!(read(dir.path(), "state"), "off\n");
    assert_eq!(
        undo(source, dir.path(), Mode::Run, &["switch"])["switch"],
        Outcome::UpToDate
    );

    // No record, and set by hand: undo can't know what it was.
    std::fs::write(dir.path().join("state"), "on\n").unwrap();
    let (outcome, report) = undo_report(source, dir.path(), "switch");
    assert_eq!(outcome, Outcome::UpToDate);
    assert_eq!(report.cannot, 1);
    // Unless it has a default.
    let with_default = source.replace("get cat state", "get cat state\n default \"idle\"");
    let (outcome, _) = undo_report(&with_default, dir.path(), "switch");
    assert_eq!(outcome, Outcome::Ran);
    assert_eq!(read(dir.path(), "state"), "idle\n");

    // `set` must make `get` print the value.
    let outcome = &run(source, dir.path(), &options(1), &["broken"])["broken"];
    assert!(
        matches!(outcome, Outcome::Failed(e) if e.contains("prints off instead of on")),
        "{outcome:?}"
    );
}

#[test]
fn settings_parse() {
    let parse = |source: &str| parse_str(source, Path::new("/tmp")).map(|_| ());
    assert!(
        parse("task a {\n setting on {\n get echo\n set {\n  echo {{ value }}\n }\n keep\n }\n}\n")
            .is_ok()
    );
    let err = parse("task a {\n setting on {\n get echo\n }\n}\n").unwrap_err();
    assert!(format!("{err:#}").contains("needs a `set`"), "{err:#}");
    let err =
        parse("task a {\n setting on {\n get echo\n get echo\n set true\n }\n}\n").unwrap_err();
    assert!(format!("{err:#}").contains("one 'get'"), "{err:#}");
}

#[cfg(unix)]
#[test]
fn downloads_are_checked_and_only_fetched_when_needed() {
    let dir = tempfile::tempdir().unwrap();
    let path = |n: &str| dir.path().join(n);
    std::fs::write(path("served"), "hello\n").unwrap();
    let url = format!("file://{}", path("served").display());
    let sha = "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03";
    let source = format!(
        r#"
        task fetch {{
            file "got" url "{url}" sha256 "{sha}"
        }}
        task wrong {{
            manual
            file "other" url "{url}" sha256 "{}"
        }}
    "#,
        "0".repeat(64)
    );
    assert_eq!(
        run(&source, dir.path(), &options(1), &[])["fetch"],
        Outcome::Ran
    );
    assert_eq!(read(dir.path(), "got"), "hello\n");
    // Already the file with that sha256: not downloaded again.
    std::fs::remove_file(path("served")).unwrap();
    assert_eq!(
        run(&source, dir.path(), &options(1), &[])["fetch"],
        Outcome::UpToDate
    );

    std::fs::write(path("served"), "hello\n").unwrap();
    let outcome = &run(&source, dir.path(), &options(1), &["wrong"])["wrong"];
    assert!(
        matches!(outcome, Outcome::Failed(e) if e.contains(&format!("has sha256 {sha}"))),
        "{outcome:?}"
    );
    assert!(!path("other").exists());

    let err = parse_str(
        "task a {\n file \"x\" url \"https://e\"\n}\n",
        Path::new("/tmp"),
    )
    .err()
    .unwrap();
    assert!(format!("{err:#}").contains("needs its checksum"), "{err:#}");
}

#[cfg(unix)]
#[test]
fn templates_name_their_parameters() {
    let dir = tempfile::tempdir().unwrap();
    let source = r#"
        let default = "hello"
        template greet(who, greeting="{{ default }}") {
            run echo {{ greeting }} {{ who }} >> log
        }
        template words {
            run printf '[%s]' {{ args }} "{{ 1 }}" >> words
        }
        let place = "the world"
        task greetings {
            greet world
            greet you hi
            greet greeting=hey them
            greet "two words" greeting='and more'
            greet {{ place }}
            greet "{{ place }}"
            words a=b 'c d'
        }
    "#;
    run(source, dir.path(), &options(1), &[]);
    assert_eq!(
        read(dir.path(), "log"),
        "hello world\nhi you\nhey them\nand more two words\nworld the\nhello the world\n"
    );
    // Without named parameters `a=b` is a word, and {{ args }} is as written.
    assert_eq!(read(dir.path(), "words"), "[a=b][c d][a=b]");

    let parse = |source: &str| parse_str(source, Path::new("/tmp")).map(|_| ());
    let template = "template greet(who, greeting=\"hi\") {\n run true\n}\n";
    let err = parse(&format!("{template}task a {{\n greet\n}}\n")).unwrap_err();
    assert!(
        format!("{err:#}").contains("needs 1 argument (who missing): greet who [greeting]"),
        "{err:#}"
    );
    let err = parse(&format!("{template}task a {{\n greet a b c\n}}\n")).unwrap_err();
    assert!(
        format!("{err:#}").contains("takes at most 2 arguments"),
        "{err:#}"
    );
    let err = parse("template t(a=1, b) {\n run true\n}\n").unwrap_err();
    assert!(
        format!("{err:#}").contains("needs a default too"),
        "{err:#}"
    );
    let err = parse("template t(args) {\n run true\n}\n").unwrap_err();
    assert!(
        format!("{err:#}").contains("can't be a parameter name"),
        "{err:#}"
    );
    let err = parse(&format!("{template}task a {{\n greet a who=b\n}}\n")).unwrap_err();
    assert!(
        format!("{err:#}").contains("given by position and as `who=`"),
        "{err:#}"
    );
    let err = parse(&format!("{template}task a {{\n greet greeting=x\n}}\n")).unwrap_err();
    assert!(format!("{err:#}").contains("who missing"), "{err:#}");
    let err = parse("template t a b {\n run true\n}\n").unwrap_err();
    assert!(
        format!("{err:#}").contains("parameters go in parentheses"),
        "{err:#}"
    );
    let err = parse("template t() {\n run true\n}\ntask a {\n t x\n}\n").unwrap_err();
    assert!(format!("{err:#}").contains("takes at most 0"), "{err:#}");
    let err = parse("template t(a) {\n run true\n}\ntask a {\n t 'x\n}\n").unwrap_err();
    assert!(format!("{err:#}").contains("unclosed quote"), "{err:#}");
}

#[cfg(unix)]
#[test]
fn tmp_is_shared_by_a_task_and_new_for_each_template_call() {
    let dir = tempfile::tempdir().unwrap();
    let source = r#"
        template scratch {
            run test -z "$(ls -A {{ tmp }})" && echo {{ tmp }} >> calls
        }
        task build {
            run test -d {{ tmp }} && test -z "$(ls -A {{ tmp }})" && touch {{ tmp }}/x && echo {{ tmp }} >> where
            if true {
                run test -e {{ tmp }}/x && echo {{ tmp }} >> where
            }
            scratch
            scratch
            run echo '\{{ tmp }}' > literal
        }
        task unused {
            run true
        }
    "#;
    let outcomes = run(source, dir.path(), &options(1), &[]);
    assert_eq!(outcomes["build"], Outcome::Ran);
    let where_ = read(dir.path(), "where");
    let dirs: Vec<&str> = where_.lines().collect();
    assert_eq!(dirs.len(), 2);
    assert_eq!(dirs[0], dirs[1], "inline blocks share the task's");
    let calls = read(dir.path(), "calls");
    let calls: Vec<&str> = calls.lines().collect();
    assert_eq!(calls.len(), 2);
    assert!(calls[0] != calls[1] && calls[0] != dirs[0], "{calls:?}");
    assert!(
        dirs.iter().chain(&calls).all(|d| !Path::new(d).exists()),
        "{dirs:?} {calls:?}"
    );
    assert_eq!(read(dir.path(), "literal"), "{{ tmp }}\n");
}

#[cfg(unix)]
#[test]
fn workdir_in_templates_and_blocks() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("c/a/b")).unwrap();
    let source = r#"
        template here(name) {
            workdir "{{ name }}"
            unless exists "{{ name }}/done"
            run pwd > done
        }
        task t {
            here a
            here a
            if exists "a/b" {
                workdir "a"
                run touch from-block
            } else {
                run touch wrong
            }
            if exists "nope" {
                workdir "a"
                run touch wrong
            } else {
                run touch from-else
            }
            workdir "c"
            run touch from-task
        }
        task clone {
            if true {
                workdir "{{ tmp }}"
                run touch x
            }
            run test -e {{ tmp }}/x && touch c/tmp-ok
        }
    "#;
    let outcomes = run(source, dir.path(), &options(1), &[]);
    assert_eq!(outcomes["t"], Outcome::Ran, "{outcomes:?}");
    assert_eq!(outcomes["clone"], Outcome::Ran, "{outcomes:?}");
    let p = dir.path();
    assert!(read(&p.join("c/a"), "done").trim_end().ends_with("/c/a"));
    assert!(p.join("c/a/from-block").exists());
    assert!(p.join("c/from-else").exists());
    assert!(p.join("c/from-task").exists());
    assert!(!p.join("c/wrong").exists() && !p.join("c/a/wrong").exists());
    assert!(p.join("c/tmp-ok").exists());
}

#[cfg(unix)]
#[test]
fn builtin_checks() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("there"), "").unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    crate::test_util::write_script(&bin.join("tool"), "#!/bin/sh\necho hi\n");
    std::fs::write(bin.join("not-executable"), "").unwrap();
    std::fs::write(
        dir.path().join("vars.env"),
        format!("PATH={}:/usr/bin:/bin\n", bin.display()),
    )
    .unwrap();
    let source = r#"
        env "vars.env"
        task checks {
            if exists "there" {
                run echo exists >> out
            }
            unless exists "not-there" {
                run echo missing >> out
            }
            if which tool {
                run echo tool >> out
            }
            if which "sh" {
                run echo sh >> out
            }
            unless which not-executable {
                run echo not-executable >> out
            }
            if $(printf 'a\n  b  \nc') has-line "b" {
                run echo has-line >> out
            }
            if $(echo hello world) contains "lo wo" {
                run echo contains >> out
            }
            if $(echo '  same  ') == "same" {
                run echo equals >> out
            }
            if $(exit 1) == "" {
                run echo failed-is-empty >> out
            }
            if $(echo printed; exit 3) == "printed" {
                run echo failed-prints >> out
            }
            if $(echo x) != "" {
                run echo not-empty >> out
            }
            if $(echo "a)b") == "a)b" {
                run echo parens >> out
            }
            if $(echo x) == "y" {
                run echo wrong >> out
            }
        }
    "#;
    let outcomes = run(source, dir.path(), &options(1), &[]);
    assert_eq!(outcomes["checks"], Outcome::Ran, "{outcomes:?}");
    assert_eq!(
        read(dir.path(), "out"),
        "exists\nmissing\ntool\nsh\nnot-executable\nhas-line\ncontains\nequals\n\
         failed-is-empty\nfailed-prints\nnot-empty\nparens\n"
    );
}

#[cfg(unix)]
#[test]
fn builtin_checks_over_ssh() {
    let dir = tempfile::tempdir().unwrap();
    let (remote, home) = fake_remote(dir.path());
    std::fs::write(home.join("there"), "").unwrap();
    let source = r#"
        task remote {
            if exists "~/there" {
                run echo exists >> out
            }
            unless exists "not-there" {
                run echo missing >> out
            }
            if which sh {
                run echo sh >> out
            }
            unless which no-such-program-anywhere {
                run echo none >> out
            }
            if $(cat there; echo x) == "x" {
                run echo output >> out
            }
        }
    "#;
    let outcomes = run_remote(source, dir.path(), &remote, &options(1));
    assert_eq!(outcomes["remote"], Outcome::Ran, "{outcomes:?}");
    assert_eq!(read(&home, "out"), "exists\nmissing\nsh\nnone\noutput\n");
}

#[cfg(unix)]
#[test]
fn wait_until_a_check_passes() {
    let dir = tempfile::tempdir().unwrap();
    let source = r#"
        task waits {
            run (sleep 1.5; touch ready) >/dev/null 2>&1 &
            wait "for ready" until exists "ready"
            run echo after > out
            wait until $(echo x) == "x"
        }
        task gives-up {
            manual
            wait timeout 1 until exists "never"
            run echo wrong > wrong
        }
    "#;
    let start = std::time::Instant::now();
    let outcomes = run(source, dir.path(), &options(1), &[]);
    assert_eq!(outcomes["waits"], Outcome::Ran, "{outcomes:?}");
    assert!(start.elapsed() >= std::time::Duration::from_secs(1));
    assert_eq!(read(dir.path(), "out"), "after\n");

    let outcomes = run(source, dir.path(), &options(1), &["gives-up"]);
    assert!(
        matches!(outcomes["gives-up"], Outcome::Failed(_)),
        "{outcomes:?}"
    );
    assert!(!dir.path().join("wrong").exists());

    // A check doesn't wait.
    let mut check = options(1);
    check.mode = Mode::DryRun;
    let start = std::time::Instant::now();
    let outcomes = run(source, dir.path(), &check, &["gives-up"]);
    assert_eq!(outcomes["gives-up"], Outcome::Ran, "{outcomes:?}");
    assert!(start.elapsed() < std::time::Duration::from_secs(1));
}

#[cfg(unix)]
#[test]
fn tmp_and_settings_over_ssh() {
    let dir = tempfile::tempdir().unwrap();
    let (remote, home) = fake_remote(dir.path());
    std::fs::write(home.join("state"), "off\n").unwrap();
    let source = r#"
        task remote {
            run test -d {{ tmp }} && echo {{ tmp }} > where
            setting on {
                get cat state
                set echo {{ value }} > state
            }
        }
    "#;
    let outcomes = run_remote(source, dir.path(), &remote, &options(1));
    assert_eq!(outcomes["remote"], Outcome::Ran, "{outcomes:?}");
    let tmp = read(&home, "where");
    assert!(
        !tmp.trim().is_empty() && !Path::new(tmp.trim()).exists(),
        "{tmp}"
    );
    assert_eq!(read(&home, "state"), "on\n");
    assert_eq!(
        run_remote(source, dir.path(), &remote, &options(1))["remote"],
        Outcome::Ran,
        "the unguarded run step always runs"
    );
}

#[test]
fn definitions_take_flags_needs_and_locks_in_their_header() {
    let source = r#"
        sudo template pkg(names="x") needs base lock pacman {
            run true
        }
        task base {
            run true
        }
        task other {
            run true
        }
        manual interactive task app needs base \
            lock pacman lock other-lock {
            needs other
            lock body-lock
            pkg a
        }
        terminal keep subtask after of app needs other {
            run true
        }
        task old-style {
            needs base
            lock pacman
            sudo
            manual
            run true
        }
    "#;
    let file = parse_str(source, Path::new("/tmp")).unwrap_or_else(|e| panic!("{e:#}"));
    let pkg = &file.templates["pkg"];
    assert!(pkg.body.sudo);
    assert_eq!(pkg.params.as_ref().map(Vec::len), Some(1));
    assert_eq!(pkg.needs, ["base"]);
    assert_eq!(pkg.body.locks, ["pacman"]);
    let app = &file.tasks["app"];
    assert!(app.manual && app.interactive);
    // The header's first, then the body's, then templates' (hoisted).
    assert_eq!(app.needs, ["base", "other"]);
    assert_eq!(app.body.locks, ["pacman", "other-lock", "body-lock"]);
    let after = &file.tasks["after"];
    assert!(after.body.terminal && after.body.keep);
    assert_eq!(after.parent.as_deref(), Some("app"));
    assert_eq!(after.needs, ["other"]);
    let old = &file.tasks["old-style"];
    assert!(old.body.sudo && old.manual);
    assert_eq!(old.needs, ["base"]);
    assert_eq!(old.body.locks, ["pacman"]);

    let parse = |source: &str| parse_str(source, Path::new("/tmp")).map(|_| ());
    let err = parse("manual template t {\n run true\n}\n").unwrap_err();
    assert!(
        format!("{err:#}").contains("can only be used on tasks"),
        "{err:#}"
    );
    let err = parse("sudo sudo task t {\n run true\n}\n").unwrap_err();
    assert!(format!("{err:#}").contains("given twice"), "{err:#}");
    let err = parse("sudo let x = 1\n").unwrap_err();
    assert!(
        format!("{err:#}").contains("expected 'task', 'subtask' or 'template'"),
        "{err:#}"
    );
    let err = parse("task t needs {\n run true\n}\n").unwrap_err();
    assert!(
        format!("{err:#}").contains("'needs' needs a task name"),
        "{err:#}"
    );
    let err = parse("task t after x {\n run true\n}\n").unwrap_err();
    assert!(
        format!("{err:#}").contains("expected 'needs', 'lock' or '{'"),
        "{err:#}"
    );
    let err = parse("task a needs nope {\n run true\n}\n").unwrap_err();
    assert!(
        format!("{err:#}").contains("unknown task 'nope'"),
        "{err:#}"
    );
}

#[cfg(unix)]
#[test]
fn templated_sources_are_relative_to_their_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/x.conf"), "from sub\n").unwrap();
    std::fs::write(root.join("main.task"), "include \"sub/inc.task\"\n").unwrap();
    std::fs::write(
        root.join("sub/inc.task"),
        "let n = \"x\"\ntask t {\n  file \"plain\" from \"x.conf\"\n  file \"templated\" from \"{{ n }}.conf\"\n}\n",
    )
    .unwrap();
    let file = crate::parser::parse_file(&root.join("main.task")).unwrap();
    let order = select(&file, &[]).unwrap();
    let outcomes: HashMap<String, Outcome> = Runner::new(&file, &options(1))
        .run(&order)
        .into_iter()
        .collect();
    assert_eq!(outcomes["t"], Outcome::Ran, "{outcomes:?}");
    assert_eq!(read(root, "plain"), "from sub\n");
    assert_eq!(read(root, "templated"), "from sub\n");
}

fn adopt(source: &str, dir: &Path, mode: Mode) -> Outcome {
    let file = parse_str(source, dir).unwrap_or_else(|e| panic!("{e:#}"));
    let mut opts = options(1);
    opts.mode = mode;
    opts.adopt = true;
    let order = select(&file, &[]).unwrap();
    let outcomes: HashMap<_, _> = Runner::new(&file, &opts).run(&order).into_iter().collect();
    outcomes["t"].clone()
}

#[cfg(unix)]
#[test]
fn adopt_records_what_new_says() {
    let dir = tempfile::tempdir().unwrap();
    let path = |n: &str| dir.path().join(n);
    std::fs::write(path("rc"), "a\n").unwrap();
    std::fs::write(path("vendor"), "v\n").unwrap();
    std::fs::create_dir(path("src")).unwrap();
    std::fs::write(path("src/one"), "1\n").unwrap();
    let with_new = r#"
        template made(name) {
            file "{{ name }}" new "x\n"
        }
        task t {
            made made
            file "gone" new "y\n"
            file "theirs" "z\n"
            directory "copied" new from "src"
            directory "empty" new
            line "rc" new ". f.sh"
            line "conf" new base "vendor" "extra"
            if false {
                file "elsewhere" new "w\n"
            }
        }
    "#;
    assert_eq!(
        run(with_new, dir.path(), &options(1), &[])["t"],
        Outcome::Ran
    );
    std::fs::remove_file(path("gone")).unwrap();
    std::fs::write(path("elsewhere"), "w\n").unwrap();
    // As if all of it was written before taskpod kept a journal.
    std::fs::remove_dir_all(path(".taskpod-journal")).unwrap();

    // A preview records nothing.
    assert_eq!(adopt(with_new, dir.path(), Mode::Diff), Outcome::Ran);
    assert!(!path(".taskpod-journal/user.db").exists());
    assert_eq!(adopt(with_new, dir.path(), Mode::Run), Outcome::Ran);
    assert_eq!(adopt(with_new, dir.path(), Mode::Run), Outcome::UpToDate);

    // Without `new`, undo now does what it said.
    let without = with_new.replace(" new ", " ");
    undo(&without, dir.path(), Mode::Run, &[]);
    for gone in ["made", "copied/one", "empty", "conf"] {
        assert!(!path(gone).exists(), "{gone}");
    }
    assert_eq!(read(dir.path(), "rc"), "a\n");
    // Not `new`, or where the task doesn't apply: left alone.
    assert_eq!(read(dir.path(), "theirs"), "z\n");
    assert_eq!(read(dir.path(), "elsewhere"), "w\n");
}
