//! Secrets from sops-encrypted files, used as `{{ secrets.name }}`.
//!
//! Files are decrypted with `sops` the first time a secret is used, so runs
//! that don't need them don't need the key either. Plaintext only lives in
//! memory, and every value is redacted from what taskpod prints.
//!
//! Each sops file needs its own decryption by the key, so with a security
//! key, several files mean several PIN entries and touches. An `age-key`
//! avoids that: an age identity, itself encrypted (to the security key), that
//! the secrets files are also encrypted to. It's decrypted once per run and
//! handed to every sops in `SOPS_AGE_KEY`.

use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};

/// The prefix of secret names in templates: `{{ secrets.github.token }}`.
pub const PREFIX: &str = "secrets.";

/// In the error for a secret used with `--no-secrets`. Errors pass through
/// lazy variables as text, so this is how a check recognises them.
pub const NOT_CHECKED: &str = "not checked, --no-secrets doesn't decrypt secrets";

/// Whether `error` comes from a secret used with `--no-secrets`.
pub fn not_checked(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains(NOT_CHECKED)
}

type Loaded = std::result::Result<BTreeMap<String, String>, String>;

#[derive(Debug, Clone)]
pub struct Secrets {
    files: Vec<PathBuf>,
    /// The program that decrypts, `sops` unless a test swaps it.
    program: String,
    /// An encrypted age identity that unlocks every file, see the module docs.
    age_key: Option<PathBuf>,
    /// The program that decrypts the age key, `age` unless a test swaps it.
    age_program: String,
    /// Where `age` finds the identity that decrypts the age key, for tests;
    /// otherwise sops' own key file.
    identity: Option<PathBuf>,
    values: Arc<OnceLock<Loaded>>,
    /// `--no-secrets`: never decrypt, every secret is an error.
    disabled: bool,
}

impl Default for Secrets {
    fn default() -> Self {
        Secrets {
            files: Vec::new(),
            program: "sops".into(),
            age_key: None,
            age_program: "age".into(),
            identity: None,
            values: Arc::default(),
            disabled: false,
        }
    }
}

impl Secrets {
    /// Add a file. Later files override keys from earlier ones.
    pub fn add(&mut self, path: PathBuf) {
        if !self.files.contains(&path) {
            self.files.push(path);
        }
    }

    /// Decrypt every file with this encrypted age identity.
    pub fn set_age_key(&mut self, path: PathBuf) {
        self.age_key = Some(path);
    }

    /// Use other programs and identity file for the age key, for tests.
    #[doc(hidden)]
    pub fn set_age_programs(&mut self, age: impl Into<String>, identity: PathBuf) {
        self.age_program = age.into();
        self.identity = Some(identity);
    }

    /// Never decrypt: using a secret is an error that [`not_checked`] recognises.
    pub fn disable(&mut self) {
        self.disabled = true;
    }

    pub fn set_program(&mut self, program: impl Into<String>) {
        self.program = program.into();
    }

    /// The value of `name` (without the `secrets.` prefix), decrypting the files if needed.
    pub fn get(&self, name: &str) -> std::result::Result<String, String> {
        if self.disabled {
            return Err(format!("secret '{name}': {NOT_CHECKED}"));
        }
        if self.files.is_empty() {
            return Err(format!(
                "secret '{name}' is used, but no secrets file was given \
                 (use --secrets FILE or `secrets \"FILE\"`)"
            ));
        }
        let values = self.values.get_or_init(|| self.load()).as_ref()?;
        values.get(name).cloned().ok_or_else(|| {
            let files: Vec<String> = self.files.iter().map(|f| f.display().to_string()).collect();
            format!("no secret '{name}' in {}", files.join(", "))
        })
    }

    fn load(&self) -> Loaded {
        let ui = crate::ui::ui();
        // sops and age may ask for a passphrase or a PIN, so they get the terminal.
        let identity = match &self.age_key {
            Some(key) => Some(ui.suspend(|| self.unwrap_age_key(key))?),
            None => None,
        };
        let mut values = BTreeMap::new();
        for file in &self.files {
            let json = match &identity {
                Some(identity) => match decrypt(&self.program, file, Some(identity)) {
                    Ok(json) => json,
                    // Not encrypted to the age key (yet): the usual way, with the security key.
                    Err(_) => {
                        let json = ui.suspend(|| decrypt(&self.program, file, None))?;
                        ui.line(format!(
                            "{} {} isn't encrypted to the age key, so it needed its own unlock; \
                             `sops updatekeys {}` fixes that",
                            console::style("!").yellow(),
                            file.display(),
                            file.display()
                        ));
                        json
                    }
                },
                None => ui.suspend(|| decrypt(&self.program, file, None))?,
            };
            let parsed: Value = serde_json::from_str(&json)
                .map_err(|e| format!("{}: sops didn't print JSON: {e}", file.display()))?;
            flatten("", &parsed, &mut values);
        }
        crate::ui::add_redactions(values.values());
        Ok(values)
    }
}

impl Secrets {
    /// Decrypt the age key with `age` and the identity sops would use.
    fn unwrap_age_key(&self, key: &Path) -> std::result::Result<String, String> {
        if !key.exists() {
            return Err(format!("age key {} doesn't exist", key.display()));
        }
        let identity = match &self.identity {
            Some(identity) => identity.clone(),
            None => sops_key_file().ok_or("no home directory to find the sops age key file in")?,
        };
        if !identity.exists() {
            return Err(format!(
                "decrypting the age key {} needs the identity in {}, which doesn't exist \
                 (set SOPS_AGE_KEY_FILE if it's elsewhere)",
                key.display(),
                identity.display()
            ));
        }
        let mut command = Command::new(&self.age_program);
        command.arg("-d").arg("-i").arg(&identity).arg(key);
        let what = format!("the age key {}", key.display());
        let output = run_prompting(command, &self.age_program, &what, true)?;
        let text = String::from_utf8(output).map_err(|_| format!("{what} isn't UTF-8"))?;
        if !text.lines().any(|l| l.trim().starts_with("AGE-")) {
            return Err(format!("{what} doesn't hold an age identity"));
        }
        crate::ui::add_redactions(text.lines().map(str::to_string).collect::<Vec<_>>().iter());
        Ok(text)
    }
}

/// The age identity file sops uses, like sops finds it.
fn sops_key_file() -> Option<PathBuf> {
    if let Some(file) = std::env::var_os("SOPS_AGE_KEY_FILE") {
        return Some(file.into());
    }
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) => PathBuf::from(dir),
        None if cfg!(target_os = "macos") => {
            PathBuf::from(crate::vars::home_dir()?).join("Library/Application Support")
        }
        None => PathBuf::from(crate::vars::home_dir()?).join(".config"),
    };
    Some(config.join("sops/age/keys.txt"))
}

/// Decrypt a sops file. With `identity`, sops gets only that age identity:
/// its usual key file is hidden, so the security key is never asked.
fn decrypt(
    program: &str,
    file: &Path,
    identity: Option<&str>,
) -> std::result::Result<String, String> {
    if !file.exists() {
        return Err(format!("secrets file {} doesn't exist", file.display()));
    }
    let mut command = Command::new(program);
    command.args(["-d", "--output-type", "json"]).arg(file);
    if let Some(identity) = identity {
        let hidden = std::env::temp_dir().join(format!("taskpod-no-config-{}", std::process::id()));
        command
            .env("SOPS_AGE_KEY", identity)
            .env_remove("SOPS_AGE_KEY_FILE")
            .env_remove("SOPS_AGE_KEY_CMD")
            .env("XDG_CONFIG_HOME", hidden);
    }
    let what = file.display().to_string();
    // With the age key nothing prompts, and a failure is retried the usual way.
    let output = run_prompting(command, program, &what, identity.is_none())?;
    String::from_utf8(output).map_err(|_| format!("{}: sops printed invalid UTF-8", file.display()))
}

/// Run `command` with the terminal for prompts, returning its stdout. With
/// `echo`, its stderr (where security key prompts appear) is shown as it comes.
fn run_prompting(
    mut command: Command,
    program: &str,
    what: &str,
    echo: bool,
) -> std::result::Result<Vec<u8>, String> {
    let mut child = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!(
                "decrypting {what} needs `{program}`, which isn't installed (on Arch: pacman -S sops age)"
            ),
            _ => format!("failed to start {program}: {e}"),
        })?;
    // sops shows security key prompts ("enter PIN", "touch your key") on
    // stderr, so pass it through as it comes, and keep it for the error.
    let mut err = child.stderr.take().expect("stderr is piped");
    let reader = std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buf = [0u8; 1024];
        while let Ok(n @ 1..) = err.read(&mut buf) {
            if echo {
                let mut terminal = std::io::stderr();
                let _ = terminal.write_all(&buf[..n]);
                let _ = terminal.flush();
            }
            kept.extend_from_slice(&buf[..n]);
        }
        kept
    });
    let output = child
        .wait_with_output()
        .map_err(|e| format!("failed to run {program}: {e}"))?;
    let stderr = reader.join().unwrap_or_default();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        return Err(format!("failed to decrypt {what}: {}", stderr.trim()));
    }
    Ok(output.stdout)
}

/// Nested keys are joined with dots, list items are numbered from 0.
fn flatten(prefix: &str, value: &Value, out: &mut BTreeMap<String, String>) {
    let key = |k: &str| {
        if prefix.is_empty() {
            k.to_string()
        } else {
            format!("{prefix}.{k}")
        }
    };
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                // sops metadata is removed on decrypt, but skip it just in case.
                if prefix.is_empty() && k == "sops" {
                    continue;
                }
                flatten(&key(k), v, out);
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                flatten(&key(&i.to_string()), v, out);
            }
        }
        Value::String(s) => {
            out.insert(prefix.to_string(), s.clone());
        }
        Value::Null => {
            out.insert(prefix.to_string(), String::new());
        }
        other => {
            out.insert(prefix.to_string(), other.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_nested_values() {
        let value: Value = serde_json::from_str(
            r#"{"a": "x", "b": {"c": 1, "d": [true, null]}, "sops": {"mac": "m"}}"#,
        )
        .unwrap();
        let mut out = BTreeMap::new();
        flatten("", &value, &mut out);
        let pairs: Vec<(&str, &str)> = out.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            pairs,
            [("a", "x"), ("b.c", "1"), ("b.d.0", "true"), ("b.d.1", "")]
        );
    }

    #[test]
    fn errors_without_files() {
        let err = Secrets::default().get("x").unwrap_err();
        assert!(err.contains("no secrets file"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn decrypts_once_and_merges_files() {
        let dir = tempfile::tempdir().unwrap();
        // A fake sops that prints the file as is and counts its runs.
        let fake = dir.path().join("fake-sops");
        crate::test_util::write_script(
            &fake,
            &format!(
                "#!/bin/sh\necho x >> {}/runs\ncat \"$4\"\n",
                dir.path().display()
            ),
        );
        std::fs::write(dir.path().join("a.json"), r#"{"x": "one", "y": "a"}"#).unwrap();
        std::fs::write(dir.path().join("b.json"), r#"{"y": "b"}"#).unwrap();
        let mut secrets = Secrets::default();
        secrets.set_program(fake.display().to_string());
        secrets.add(dir.path().join("a.json"));
        secrets.add(dir.path().join("b.json"));
        assert_eq!(secrets.get("x").unwrap(), "one");
        assert_eq!(secrets.get("y").unwrap(), "b");
        assert!(secrets.get("z").unwrap_err().contains("no secret 'z'"));
        let runs = std::fs::read_to_string(dir.path().join("runs")).unwrap();
        assert_eq!(runs.lines().count(), 2, "each file is decrypted once");
    }

    #[cfg(unix)]
    #[test]
    fn age_key_is_unwrapped_once() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display();
        // A fake age that "decrypts" by printing the file, counting its runs.
        let age = dir.path().join("fake-age");
        crate::test_util::write_script(
            &age,
            &format!(
                "#!/bin/sh\necho x >> {d}/age-runs\n[ \"$2\" = -i ] && [ -e \"$3\" ] || exit 1\ncat \"$4\"\n"
            ),
        );
        // A fake sops that needs the unwrapped key for files ending in .json,
        // and the "security key" (no SOPS_AGE_KEY) for old.json.
        let sops = dir.path().join("fake-sops");
        crate::test_util::write_script(
            &sops,
            &format!(
                "#!/bin/sh\necho \"$4 ${{SOPS_AGE_KEY:+key}}\" >> {d}/sops-runs\n\
                 case \"$4\" in *old.json) [ -z \"$SOPS_AGE_KEY\" ] || exit 1 ;; \
                 *) [ \"$(echo \"$SOPS_AGE_KEY\")\" = 'AGE-SECRET-KEY-1TEST' ] || exit 1 ;; esac\n\
                 [ -z \"$SOPS_AGE_KEY_FILE\" ] || [ -z \"$SOPS_AGE_KEY\" ] || exit 2\n\
                 cat \"$4\"\n"
            ),
        );
        std::fs::write(dir.path().join("key.age"), "AGE-SECRET-KEY-1TEST\n").unwrap();
        std::fs::write(dir.path().join("keys.txt"), "identity").unwrap();
        for name in ["a", "b", "old"] {
            std::fs::write(
                dir.path().join(format!("{name}.json")),
                format!(r#"{{"{name}": "v"}}"#),
            )
            .unwrap();
        }
        let mut secrets = Secrets::default();
        secrets.set_program(sops.display().to_string());
        secrets.set_age_programs(age.display().to_string(), dir.path().join("keys.txt"));
        secrets.set_age_key(dir.path().join("key.age"));
        for name in ["a", "b", "old"] {
            secrets.add(dir.path().join(format!("{name}.json")));
        }
        assert_eq!(secrets.get("a").unwrap(), "v");
        assert_eq!(secrets.get("old").unwrap(), "v");
        let age_runs = std::fs::read_to_string(dir.path().join("age-runs")).unwrap();
        assert_eq!(age_runs.lines().count(), 1, "the age key is decrypted once");
        let sops_runs = std::fs::read_to_string(dir.path().join("sops-runs")).unwrap();
        let with_key = sops_runs.lines().filter(|l| l.ends_with(" key")).count();
        // a and b with the key; old tried with it, then the usual way.
        assert_eq!((with_key, sops_runs.lines().count()), (3, 4), "{sops_runs}");
        assert_eq!(crate::ui::redact("AGE-SECRET-KEY-1TEST"), "••••");
    }
}
