//! `quotaline install` / `uninstall`: merge (or remove) the `statusLine` block in
//! `~/.claude/settings.json`, cross-platform, without disturbing any other keys. The binary
//! wires its own absolute path, so there is no interpreter path to detect. Existing settings
//! are backed up first; an invalid-JSON settings file is never overwritten.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Number, Value};

use crate::history::home_dir;

fn settings_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CLAUDE_SETTINGS") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    home_dir().map(|h| h.join(".claude").join("settings.json"))
}

/// Quote the command path for the shell Claude Code runs it in (only if it has whitespace).
fn quote_cmd(path: &str) -> String {
    if path.chars().any(char::is_whitespace) {
        format!("\"{path}\"")
    } else {
        path.to_string()
    }
}

fn backup(settings: &Path) -> std::io::Result<()> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = settings
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings.json".to_string());
    let bak = settings.with_file_name(format!("{name}.bak.{ts}"));
    fs::copy(settings, &bak)?;
    println!("backed up settings → {}", bak.display());
    Ok(())
}

fn write_atomic(path: &Path, v: &Value) -> std::io::Result<()> {
    let mut s = serde_json::to_string_pretty(v).unwrap_or_default();
    s.push('\n');
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, s)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Load settings as a JSON object, or `Err` with a message. Missing/empty → empty object.
fn load_object(settings: &Path) -> Result<Value, String> {
    if !settings.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let text = fs::read_to_string(settings)
        .map_err(|e| format!("cannot read {}: {e}", settings.display()))?;
    if text.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(v @ Value::Object(_)) => Ok(v),
        Ok(_) => Err(format!(
            "{} is not a JSON object — refusing to overwrite",
            settings.display()
        )),
        Err(_) => Err(format!(
            "{} is not valid JSON — refusing to overwrite (fix or move it, then re-run)",
            settings.display()
        )),
    }
}

pub fn install(refresh: u64) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            eprintln!("error: cannot resolve quotaline's own path: {e}");
            return 1;
        }
    };
    let settings = match settings_path() {
        Some(p) => p,
        None => {
            eprintln!("error: cannot locate ~/.claude/settings.json (no HOME)");
            return 1;
        }
    };

    let mut root = match load_object(&settings) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("error: {msg}");
            return 1;
        }
    };

    if settings.exists() {
        if let Err(e) = backup(&settings) {
            eprintln!("warning: could not back up settings: {e}");
        }
    } else if let Some(parent) = settings.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let command = quote_cmd(&exe);
    let mut block = Map::new();
    block.insert("type".into(), Value::String("command".into()));
    block.insert("command".into(), Value::String(command.clone()));
    block.insert(
        "refreshInterval".into(),
        Value::Number(Number::from(refresh)),
    );
    // as_object_mut is safe: load_object only ever returns an Object.
    root.as_object_mut()
        .unwrap()
        .insert("statusLine".into(), Value::Object(block));

    if let Err(e) = write_atomic(&settings, &root) {
        eprintln!("error: could not write {}: {e}", settings.display());
        return 1;
    }
    println!("statusLine installed → {}", settings.display());
    println!("  command: {command}");
    println!("  refreshInterval: {refresh}s");
    println!("Start a new Claude Code session (or wait ~{refresh}s) to see the status line.");
    println!("note: limits show only on Pro/Max, after the session's first API response.");
    0
}

pub fn uninstall() -> i32 {
    let settings = match settings_path() {
        Some(p) => p,
        None => {
            eprintln!("error: cannot locate ~/.claude/settings.json (no HOME)");
            return 1;
        }
    };
    if !settings.exists() {
        println!("nothing to do: {} not found", settings.display());
        return 0;
    }
    let mut root = match load_object(&settings) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("error: {msg}");
            return 1;
        }
    };
    if let Err(e) = backup(&settings) {
        eprintln!("warning: could not back up settings: {e}");
    }
    // shift_remove (not remove/swap_remove) so the other keys keep their order.
    let removed = root
        .as_object_mut()
        .unwrap()
        .shift_remove("statusLine")
        .is_some();
    if let Err(e) = write_atomic(&settings, &root) {
        eprintln!("error: could not write {}: {e}", settings.display());
        return 1;
    }
    println!(
        "{}",
        if removed {
            "statusLine removed"
        } else {
            "no statusLine block was present"
        }
    );
    0
}
