//! Clipboard via the platform's command-line tools, so we don't need a clipboard crate.

use std::io::Write;
use std::process::{Command, Stdio};

fn candidates(copy: bool) -> &'static [&'static [&'static str]] {
    if cfg!(target_os = "macos") {
        if copy { &[&["pbcopy"]] } else { &[&["pbpaste"]] }
    } else if cfg!(windows) {
        if copy {
            &[&["clip"]]
        } else {
            &[&["powershell", "-NoProfile", "-Command", "Get-Clipboard"]]
        }
    } else if copy {
        &[&["wl-copy"], &["xclip", "-selection", "clipboard"], &["xsel", "-b", "-i"]]
    } else {
        &[&["wl-paste", "-n"], &["xclip", "-selection", "clipboard", "-o"], &["xsel", "-b", "-o"]]
    }
}

pub fn copy(text: &str) -> bool {
    for cmd in candidates(true) {
        let child = Command::new(cmd[0]).args(&cmd[1..]).stdin(Stdio::piped()).spawn();
        if let Ok(mut child) = child {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().is_ok_and(|s| s.success()) {
                return true;
            }
        }
    }
    false
}

pub fn paste() -> String {
    for cmd in candidates(false) {
        if let Ok(out) = Command::new(cmd[0]).args(&cmd[1..]).output() {
            if out.status.success() {
                return String::from_utf8_lossy(&out.stdout).into_owned();
            }
        }
    }
    String::new()
}
