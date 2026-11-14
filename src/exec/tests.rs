use super::*;
use crate::ast::Encoding;
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
    }
}

fn run(source: &str, dir: &Path, opts: &Options, targets: &[&str]) -> HashMap<String, Outcome> {
    let file = parse_str(source, dir).unwrap_or_else(|e| panic!("{e:#}"));
    let targets: Vec<String> = targets.iter().map(|s| s.to_string()).collect();
    let order = select(&file, &targets).unwrap();
    Runner::new(&file, opts).run(&order).into_iter().collect()
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
    for entry in std::fs::read_dir(path(".taskpod-journal/user")).unwrap() {
        let entry = entry.unwrap().path();
        let name = entry.file_name().unwrap().to_string_lossy().to_string();
        if name.ends_with("unrecorded") || name.ends_with("marked") {
            std::fs::remove_file(entry).unwrap();
        }
    }
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
    let mode = std::fs::metadata(path("existing"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(!path("marked").exists());
    assert_eq!(read(dir.path(), "unrecorded"), "managed\n");
    assert_eq!(read(dir.path(), "kept"), "managed\n");
    // Undone files leave the journal (`kept` was never written: it stays
    // recorded); doing it again finds nothing to do.
    let left: Vec<String> = std::fs::read_dir(path(".taskpod-journal/user"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
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
}
