use super::*;
use crate::parser::parse_str;

fn options(jobs: usize) -> Options {
    Options {
        dry_run: false,
        ignore_guards: false,
        jobs,
        skip: HashSet::new(),
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
    opts.dry_run = true;
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
