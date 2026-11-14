//! Secrets from sops-encrypted files, used as `{{ secrets.name }}`.
//!
//! Files are decrypted with `sops` the first time a secret is used, so runs
//! that don't need them don't need the key either. Plaintext only lives in
//! memory, and every value is redacted from what taskpod prints.

use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};

/// The prefix of secret names in templates: `{{ secrets.github.token }}`.
pub const PREFIX: &str = "secrets.";

type Loaded = std::result::Result<BTreeMap<String, String>, String>;

#[derive(Debug, Clone)]
pub struct Secrets {
    files: Vec<PathBuf>,
    /// The program that decrypts, `sops` unless a test swaps it.
    program: String,
    values: Arc<OnceLock<Loaded>>,
}

impl Default for Secrets {
    fn default() -> Self {
        Secrets {
            files: Vec::new(),
            program: "sops".into(),
            values: Arc::default(),
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

    pub fn set_program(&mut self, program: impl Into<String>) {
        self.program = program.into();
    }

    /// The value of `name` (without the `secrets.` prefix), decrypting the files if needed.
    pub fn get(&self, name: &str) -> std::result::Result<String, String> {
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
        let mut values = BTreeMap::new();
        for file in &self.files {
            // sops may ask for a passphrase or a PIN, so it gets the terminal.
            let json = crate::ui::ui().suspend(|| decrypt(&self.program, file))?;
            let parsed: Value = serde_json::from_str(&json)
                .map_err(|e| format!("{}: sops didn't print JSON: {e}", file.display()))?;
            flatten("", &parsed, &mut values);
        }
        crate::ui::add_redactions(values.values());
        Ok(values)
    }
}

fn decrypt(program: &str, file: &Path) -> std::result::Result<String, String> {
    if !file.exists() {
        return Err(format!("secrets file {} doesn't exist", file.display()));
    }
    let mut child = Command::new(program)
        .args(["-d", "--output-type", "json"])
        .arg(file)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!(
                "decrypting {} needs `{program}`, which isn't installed (on Arch: pacman -S sops age)",
                file.display()
            ),
            _ => format!("failed to start {program}: {e}"),
        })?;
    // sops shows security key prompts ("enter PIN", "touch your key") on
    // stderr, so pass it through as it comes, and keep it for the error.
    let mut err = child.stderr.take().expect("stderr is piped");
    let echo = std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buf = [0u8; 1024];
        while let Ok(n @ 1..) = err.read(&mut buf) {
            let mut terminal = std::io::stderr();
            let _ = terminal.write_all(&buf[..n]);
            let _ = terminal.flush();
            kept.extend_from_slice(&buf[..n]);
        }
        kept
    });
    let output = child
        .wait_with_output()
        .map_err(|e| format!("failed to run {program}: {e}"))?;
    let stderr = echo.join().unwrap_or_default();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        return Err(format!(
            "failed to decrypt {}: {}",
            file.display(),
            stderr.trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| format!("{}: sops printed invalid UTF-8", file.display()))
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
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // A fake sops that prints the file as is and counts its runs.
        let fake = dir.path().join("fake-sops");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\necho x >> {}/runs\ncat \"$4\"\n",
                dir.path().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
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
}
