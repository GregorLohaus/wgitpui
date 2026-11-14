//! User settings: `config` next to the session file, as `key = value` lines (`#` comments).
//! Re-read whenever the window gets focus, so edits apply without a restart.
//!
//! ```text
//! # The keys for whatever has focus, along the bottom of the window.
//! key_bar = false
//! ```

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// The bar at the bottom with the keys for the focused pane.
    pub key_bar: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { key_bar: true }
    }
}

/// Where wgitpui keeps its files: `~/Library/Application Support/wgitpui` on macOS,
/// `%APPDATA%\wgitpui` on Windows, `$XDG_CONFIG_HOME/wgitpui` (or `~/.config/wgitpui`) elsewhere.
pub fn dir() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?
    };
    Some(base.join("wgitpui"))
}

fn path() -> Option<PathBuf> {
    Some(dir()?.join("config"))
}

pub fn load() -> Config {
    path().and_then(|p| std::fs::read_to_string(p).ok()).map_or_else(Config::default, |t| parse(&t))
}

pub fn parse(text: &str) -> Config {
    let mut c = Config::default();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once('=') else { continue };
        if let ("key_bar", Some(on)) = (key.trim(), flag(value.trim())) {
            c.key_bar = on;
        }
    }
    c
}

fn flag(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Some(true),
        "false" | "off" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// `text` with `key` set to `value`: its line replaced, or added with what it's for; everything
/// else (comments too) stays as it is.
pub fn with_setting(text: &str, key: &str, value: &str) -> String {
    let is_key = |l: &str| l.split('#').next().unwrap_or("").split_once('=').is_some_and(|(k, _)| k.trim() == key);
    if text.lines().any(is_key) {
        let mut out: String = text.lines().map(|l| if is_key(l) { format!("{key} = {value}\n") } else { format!("{l}\n") }).collect();
        if !text.ends_with('\n') {
            out.pop();
        }
        return out;
    }
    let mut out = if text.trim().is_empty() { "# wgitpui settings (re-read when the window gets focus)\n".to_string() } else { text.to_string() };
    if !out.ends_with('\n') {
        out.push('\n');
    }
    let about = match key {
        "key_bar" => "# The keys for whatever has focus, along the bottom of the window.\n",
        _ => "",
    };
    out.push_str(&format!("\n{about}{key} = {value}\n"));
    out
}

/// Writes one setting into the config file.
pub fn save(key: &str, value: &str) {
    let Some(p) = path() else { return };
    let text = std::fs::read_to_string(&p).unwrap_or_default();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(&p, with_setting(&text, key, value));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes_settings_keeping_the_rest() {
        assert!(parse("").key_bar, "on by default");
        assert!(!parse("# hi\nkey_bar = off  # quiet\n").key_bar);
        assert!(parse("key_bar = maybe").key_bar, "nonsense keeps the default");
        let fresh = with_setting("", "key_bar", "false");
        assert!(fresh.starts_with("# wgitpui settings") && fresh.ends_with("key_bar = false\n"), "{fresh}");
        assert!(!parse(&fresh).key_bar);
        let edited = with_setting("# mine\nother = 1\nkey_bar = false # was off\n", "key_bar", "true");
        assert_eq!(edited, "# mine\nother = 1\nkey_bar = true\n");
        assert!(parse(&edited).key_bar);
    }
}
