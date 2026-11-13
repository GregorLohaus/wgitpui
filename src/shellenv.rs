//! The environment git runs in when the app is started outside a terminal.
//!
//! Apps opened from the Dock, Finder or Spotlight get launchd's environment instead of a shell's:
//! PATH is just the system directories and SSH_AUTH_SOCK is macOS's own agent. (Started by
//! `wgitpui` in a terminal, the app inherits that terminal's environment, which `open` passes on,
//! and nothing here changes it.) git would then run without the user's tools (git-lfs, gpg,
//! hook dependencies from Homebrew or nix), and ssh without the agent holding their keys whenever
//! a shell profile points SSH_AUTH_SOCK at one (Bitwarden, 1Password, Secretive, gpg-agent).
//!
//! So before anything else starts, the app adopts the login shell's environment. If ssh would still
//! get an empty launchd agent, it takes the agent another installed shell's profile sets: someone
//! who works in fish often still has zsh as their login shell. Nothing here reads ~/.ssh; ssh does
//! that itself, as it does from a terminal (the app isn't sandboxed), and an `IdentityAgent` in
//! ~/.ssh/config takes precedence over SSH_AUTH_SOCK either way.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a shell may take to start; a slower one is killed and its environment ignored.
const TIMEOUT: Duration = Duration::from_secs(3);

/// Tells the shell which file to write its environment to.
const OUT_VAR: &str = "WGITPUI_ENV_FILE";

/// Variables describing the shell's own session or terminal rather than the user's setup.
const SESSION_VARS: &[&str] = &[
    "_",
    "PWD",
    "OLDPWD",
    "SHLVL",
    "TERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TERM_SESSION_ID",
    "COLORTERM",
    "COLUMNS",
    "LINES",
    "GPG_TTY",
    "SSH_TTY",
    "TMUX",
    "TMUX_PANE",
    "STY",
    "__CFBundleIdentifier",
    "XPC_SERVICE_NAME",
    "XPC_FLAGS",
    OUT_VAR,
];

/// Adopts the user's shell environment when the app wasn't started from a terminal.
///
/// Changes this process's environment, so it must run before any other thread exists.
pub fn adopt() {
    if std::env::var_os("TERM").is_some() {
        return; // started from a terminal, whose environment is already the user's
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let shell = std::env::var_os("SHELL")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(if cfg!(target_os = "macos") { "/bin/zsh" } else { "/bin/sh" }));
    match shell_env(&shell, home.as_deref(), TIMEOUT) {
        Some(vars) => {
            for (k, v) in vars {
                // SAFETY: no other thread exists yet (see above), so nothing reads the
                // environment while it changes.
                unsafe { std::env::set_var(k, v) };
            }
            eprintln!("wgitpui: took the environment of {}", shell.display());
        }
        None => eprintln!("wgitpui: couldn't read the environment of {}", shell.display()),
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    if let Some((sock, from)) = other_shells_agent(&shell, home.as_deref(), std::env::var_os("SSH_AUTH_SOCK"), &path) {
        eprintln!("wgitpui: SSH agent {} (set by {})", Path::new(&sock).display(), from.display());
        // SAFETY: as above.
        unsafe { std::env::set_var("SSH_AUTH_SOCK", sock) };
    }
}

/// The environment `shell` ends up with as an interactive login shell (profile and rc files read),
/// minus session variables. None if it fails or takes longer than `timeout`.
fn shell_env(shell: &Path, home: Option<&Path>, timeout: Duration) -> Option<Vec<(OsString, OsString)>> {
    // `env -0` goes to a private file, not stdout: rc files may clutter that with greetings, and a
    // background process they start could keep it open.
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let out = std::env::temp_dir().join(format!("wgitpui-env-{}-{nanos}", std::process::id()));
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&out).ok()?;
    let env = run_shell(shell, home, &out, timeout).and_then(|()| {
        let mut dump = Vec::new();
        std::fs::File::open(&out).ok()?.take(4 << 20).read_to_end(&mut dump).ok()?;
        parse_env(&dump)
    });
    let _ = std::fs::remove_file(&out);
    env
}

fn run_shell(shell: &Path, home: Option<&Path>, out: &Path, timeout: Duration) -> Option<()> {
    let mut cmd = Command::new(shell);
    // -l -i: the files a terminal's shell reads (zsh reads .zshrc only when interactive).
    // `"$VAR"` expands the same way in sh, bash, zsh and fish.
    cmd.args(["-l", "-i", "-c", &format!("/usr/bin/env -0 >\"${OUT_VAR}\"")])
        .env(OUT_VAR, out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(h) = home {
        cmd.current_dir(h);
    }
    let mut child = cmd.spawn().ok()?;
    let start = Instant::now();
    while child.try_wait().ok()?.is_none() {
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Some(())
}

/// `env -0` output minus session variables. None without a PATH: the command never ran (an rc
/// file that execs another shell, say).
fn parse_env(dump: &[u8]) -> Option<Vec<(OsString, OsString)>> {
    let vars: Vec<(OsString, OsString)> = dump
        .split(|&b| b == 0)
        .filter_map(|entry| {
            let eq = entry.iter().position(|&b| b == b'=').filter(|&i| i > 0)?;
            let (k, v) = (&entry[..eq], &entry[eq + 1..]);
            let session = SESSION_VARS.iter().any(|s| s.as_bytes() == k);
            (!session).then(|| (OsString::from_vec(k.to_vec()), OsString::from_vec(v.to_vec())))
        })
        .collect();
    vars.iter().any(|(k, _)| k == "PATH").then_some(vars)
}

/// When ssh would get launchd's agent (or none) and it holds no keys: the SSH_AUTH_SOCK another
/// installed shell's profile sets, and that shell. Nothing else is taken from that shell.
fn other_shells_agent(login_shell: &Path, home: Option<&Path>, current: Option<OsString>, path: &OsStr) -> Option<(OsString, PathBuf)> {
    if let Some(cur) = &current
        && !(is_launchd_agent(cur) && agent_keys(Path::new(cur)).unwrap_or(0) == 0)
    {
        return None;
    }
    ["fish", "zsh", "bash"]
        .into_iter()
        .filter(|name| login_shell.file_name() != Some(OsStr::new(name)))
        .filter_map(|name| find_on_path(name, path))
        .find_map(|sh| {
            let (_, sock) = shell_env(&sh, home, TIMEOUT)?.into_iter().find(|(k, _)| k == "SSH_AUTH_SOCK")?;
            (!sock.is_empty() && !is_launchd_agent(&sock) && Some(&sock) != current.as_ref()).then_some((sock, sh))
        })
}

/// macOS's per-session agent, which launchd starts on demand.
fn is_launchd_agent(sock: &OsStr) -> bool {
    let marker = b"com.apple.launchd.";
    sock.as_bytes().windows(marker.len()).any(|w| w == marker)
}

/// How many keys the agent at `sock` holds (SSH_AGENTC_REQUEST_IDENTITIES); None if it doesn't answer.
fn agent_keys(sock: &Path) -> Option<u32> {
    let mut s = UnixStream::connect(sock).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    s.set_write_timeout(Some(Duration::from_secs(1))).ok()?;
    s.write_all(&[0, 0, 0, 1, 11]).ok()?;
    // Message length, SSH_AGENT_IDENTITIES_ANSWER (12), key count.
    let mut head = [0u8; 9];
    s.read_exact(&mut head).ok()?;
    (head[4] == 12).then(|| u32::from_be_bytes([head[5], head[6], head[7], head[8]]))
}

fn find_on_path(name: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|p| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wgitpui-shellenv-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stand-in shell that ignores its arguments and runs `body` (with $WGITPUI_ENV_FILE set).
    fn fake_shell(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn get<'a>(vars: &'a [(OsString, OsString)], k: &str) -> Option<&'a OsStr> {
        vars.iter().find(|(name, _)| name == k).map(|(_, v)| v.as_os_str())
    }

    #[test]
    fn parses_env_dumps_without_session_variables() {
        let vars = parse_env(b"PATH=/opt/homebrew/bin:/usr/bin\0SSH_AUTH_SOCK=/a.sock\0TERM=xterm\0SHLVL=2\0X=a=b\0EMPTY=\0junk\0").unwrap();
        assert_eq!(get(&vars, "PATH"), Some(OsStr::new("/opt/homebrew/bin:/usr/bin")));
        assert_eq!(get(&vars, "SSH_AUTH_SOCK"), Some(OsStr::new("/a.sock")));
        assert_eq!(get(&vars, "X"), Some(OsStr::new("a=b")));
        assert_eq!(get(&vars, "EMPTY"), Some(OsStr::new("")));
        assert_eq!(get(&vars, "TERM"), None);
        assert_eq!(get(&vars, "SHLVL"), None);
        assert_eq!(vars.len(), 4);
        // Without PATH the shell never got to `env` (or it isn't one).
        assert!(parse_env(b"HOME=/x\0").is_none());
        assert!(parse_env(b"").is_none());
    }

    #[test]
    fn reads_a_shells_environment_and_gives_up_on_slow_ones() {
        let dir = scratch("read");
        let sh = fake_shell(&dir, "sh", r#"printf 'PATH=/nix/bin:/usr/bin\0SSH_AUTH_SOCK=/agent.sock\0PWD=/somewhere\0' >"$WGITPUI_ENV_FILE""#);
        let vars = shell_env(&sh, Some(&dir), TIMEOUT).unwrap();
        assert_eq!(get(&vars, "PATH"), Some(OsStr::new("/nix/bin:/usr/bin")));
        assert_eq!(get(&vars, "SSH_AUTH_SOCK"), Some(OsStr::new("/agent.sock")));
        assert_eq!(get(&vars, "PWD"), None);

        let slow = fake_shell(&dir, "slow", "sleep 10");
        let t = Instant::now();
        assert!(shell_env(&slow, None, Duration::from_millis(200)).is_none());
        assert!(t.elapsed() < Duration::from_secs(5));
        // An rc file that replaces the shell never writes anything.
        let quiet = fake_shell(&dir, "quiet", "exit 0");
        assert!(shell_env(&quiet, None, TIMEOUT).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn counts_an_agents_keys() {
        let dir = scratch("agent");
        let sock = dir.join("agent.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let agent = std::thread::spawn(move || {
            let (mut c, _) = listener.accept().unwrap();
            let mut req = [0u8; 5];
            c.read_exact(&mut req).unwrap();
            assert_eq!(req, [0, 0, 0, 1, 11]);
            c.write_all(&[0, 0, 0, 5, 12, 0, 0, 0, 2]).unwrap();
        });
        assert_eq!(agent_keys(&sock), Some(2));
        agent.join().unwrap();
        assert_eq!(agent_keys(&dir.join("missing.sock")), None);
        assert!(is_launchd_agent(OsStr::new("/private/tmp/com.apple.launchd.AbCd/Listeners")));
        assert!(!is_launchd_agent(OsStr::new("/Users/me/Library/Containers/com.bitwarden.desktop/Data/.bitwarden-ssh-agent.sock")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn takes_the_agent_another_shell_sets() {
        let dir = scratch("other");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = bin.clone().into_os_string();
        // A launchd agent that doesn't answer counts as holding no keys.
        let launchd = OsString::from(dir.join("com.apple.launchd.x/Listeners"));
        fake_shell(&bin, "zsh", r#"printf 'PATH=/usr/bin\0SSH_AUTH_SOCK=/tmp/com.apple.launchd.x/Listeners\0' >"$WGITPUI_ENV_FILE""#);
        let fish = fake_shell(&bin, "fish", r#"printf 'PATH=/usr/bin\0SSH_AUTH_SOCK=/vault/agent.sock\0' >"$WGITPUI_ENV_FILE""#);

        let got = other_shells_agent(Path::new("/bin/zsh"), None, Some(launchd.clone()), &path);
        assert_eq!(got, Some((OsString::from("/vault/agent.sock"), fish.clone())));
        // No agent at all: same.
        assert_eq!(other_shells_agent(Path::new("/bin/zsh"), None, None, &path).map(|(s, _)| s), Some(OsString::from("/vault/agent.sock")));
        // An agent the login shell set wins.
        assert_eq!(other_shells_agent(Path::new("/bin/zsh"), None, Some(OsString::from("/login/agent.sock")), &path), None);
        // The login shell isn't asked twice: fish as login shell leaves only zsh, which sets launchd's.
        assert_eq!(other_shells_agent(Path::new("/opt/homebrew/bin/fish"), None, Some(launchd), &path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
