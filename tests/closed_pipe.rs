//! Each command against a stdout whose reader has already gone.
#![cfg(unix)]

use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;

/// Off Linux, std creates a pipe and marks it close-on-exec in separate calls
/// (`library/std/src/sys/pipe/unix.rs`), so a test spawning in between hands its child the
/// read end and the pipe never closes. Every spawn here goes through `run`, under this lock.
static SPAWN: Mutex<()> = Mutex::new(());

struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("quotaline-pipe-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Runs the binary with stdout on a pipe whose read end is already closed.
fn run(args: &[&str], dir: &TempDir, stdin: Stdio) -> Output {
    let _spawning = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    Command::new(env!("CARGO_BIN_EXE_quotaline"))
        .args(args)
        .env("CODEX_HOME", &dir.0)
        .env("CTT_STATE_DIR", &dir.0)
        .stdin(stdin)
        .stdout(writer)
        .output()
        .unwrap()
}

fn assert_ends_by_sigpipe(out: &Output) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert_eq!(out.status.signal(), Some(13), "{:?}", out.status);
}

#[test]
fn the_claude_report_ends_by_sigpipe() {
    let d = TempDir::new("claude");
    assert_ends_by_sigpipe(&run(&["report"], &d, Stdio::null()));
}

#[test]
fn the_codex_report_ends_by_sigpipe() {
    let d = TempDir::new("codex");
    fs::create_dir_all(d.0.join("sessions")).unwrap();
    assert_ends_by_sigpipe(&run(&["report", "--codex"], &d, Stdio::null()));
}

#[test]
fn the_status_line_logs_its_sample_into_a_closed_pipe() {
    let d = TempDir::new("statusline");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let payload = d.0.join("payload.json");
    fs::write(
        &payload,
        format!(
            r#"{{"session_id":"s1","rate_limits":{{"five_hour":{{"used_percentage":11,"resets_at":{}}}}}}}"#,
            now + 3600
        ),
    )
    .unwrap();
    let out = run(&[], &d, Stdio::from(fs::File::open(&payload).unwrap()));
    assert!(out.status.success(), "{:?}", out.status);
    assert!(d.0.join("usage-history.json").exists());
}
