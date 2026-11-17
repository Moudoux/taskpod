pub mod ast;
pub mod exec;
pub mod parser;
pub mod remote;
pub mod secrets;
pub mod tty;
pub mod ui;
pub mod vars;

#[cfg(all(test, unix))]
mod test_util {
    use std::path::Path;

    /// Write an executable script. It's written by another process, so no
    /// command forked meanwhile by a parallel test holds it open for writing,
    /// which would make running it fail with "text file busy".
    pub fn write_script(path: &Path, content: &str) {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, content).unwrap();
        let status = std::process::Command::new("install")
            .args(["-m", "755"])
            .arg(&tmp)
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }
}
