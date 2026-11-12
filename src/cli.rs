//! Command-line behavior: `wgitpui [path]` from a terminal hands the repository to an already
//! running instance (it opens as a tab), or launches the app detached from the terminal so the
//! shell stays usable. Also installs the `wgitpui` command from inside the app bundle.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Socket the running GUI listens on for "open this repository" requests.
#[cfg(unix)]
fn socket_path() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support/wgitpui")
    } else {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/wgitpui")))?
    };
    let path = base.join("wgitpui.sock");
    // Unix socket paths are limited to ~104 bytes; fall back to a short per-user path.
    if path.as_os_str().len() < 100 {
        return Some(path);
    }
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { getuid() };
    Some(PathBuf::from(format!("/tmp/wgitpui-{uid}.sock")))
}

/// Asks a running instance to open these paths. Returns true if one took them.
pub fn send_to_running(paths: &[PathBuf]) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        let Some(sock) = socket_path() else { return false };
        let Ok(mut s) = UnixStream::connect(&sock) else { return false };
        let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        for p in paths {
            if writeln!(s, "open\t{}", p.display()).is_err() {
                return false;
            }
        }
        let _ = s.shutdown(std::net::Shutdown::Write);
        // Wait for the acknowledgement so we only report success once it's been received.
        let mut ack = String::new();
        BufReader::new(s).read_line(&mut ack).is_ok() && ack.starts_with("ok")
    }
    #[cfg(not(unix))]
    {
        let _ = paths;
        false
    }
}

/// Starts listening for open requests; `deliver` is called (from a background thread) per path.
pub fn listen(deliver: impl Fn(PathBuf) + Send + 'static) {
    #[cfg(unix)]
    {
        use std::os::unix::net::{UnixListener, UnixStream};
        let Some(sock) = socket_path() else { return };
        if let Some(dir) = sock.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let listener = match UnixListener::bind(&sock) {
            Ok(l) => l,
            Err(_) => {
                // Another instance is alive: leave it be. Otherwise it's a stale socket file.
                if UnixStream::connect(&sock).is_ok() {
                    return;
                }
                let _ = std::fs::remove_file(&sock);
                match UnixListener::bind(&sock) {
                    Ok(l) => l,
                    Err(_) => return,
                }
            }
        };
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 0) {
                    if let Some(p) = line.trim_end().strip_prefix("open\t") {
                        deliver(PathBuf::from(p));
                    }
                    line.clear();
                }
                let _ = (&stream).write_all(b"ok\n");
            }
        });
    }
    #[cfg(not(unix))]
    let _ = deliver;
}

/// The enclosing `.app` bundle when running from one (macOS), even through a symlink.
pub fn app_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).map(Path::to_path_buf)
}

/// Launches the GUI detached from this terminal, then the caller exits.
pub fn launch_detached(paths: &[PathBuf]) -> Result<(), String> {
    use std::process::{Command, Stdio};
    if let Some(bundle) = app_bundle().filter(|_| cfg!(target_os = "macos")) {
        // Through LaunchServices, so the app gets its Dock icon, menu bar and focus.
        let mut cmd = Command::new("open");
        cmd.arg("-a").arg(&bundle).arg("--args").args(paths);
        return cmd.status().map_err(|e| e.to_string()).and_then(|s| if s.success() { Ok(()) } else { Err("open failed".into()) });
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.arg("--foreground").args(paths).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe extern "C" {
            fn setsid() -> i32;
        }
        // SAFETY: setsid is async-signal-safe; it moves the child into its own session so it
        // isn't tied to (or killed with) the terminal.
        unsafe {
            cmd.pre_exec(|| {
                setsid();
                Ok(())
            });
        }
    }
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Puts a `wgitpui` command on the PATH pointing at the binary inside the app bundle.
/// Blocking (may show the macOS administrator prompt); run off the UI thread.
pub fn install_command() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?.canonicalize().map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        let target = Path::new("/usr/local/bin/wgitpui");
        let _ = std::fs::remove_file(target);
        match std::os::unix::fs::symlink(&exe, target) {
            Ok(()) => return Ok(format!("Installed {}", target.display())),
            Err(_) if cfg!(target_os = "macos") => {
                let q = |p: &Path| p.display().to_string().replace('\'', "'\\''");
                let script = format!(
                    "do shell script \"mkdir -p /usr/local/bin && ln -sf '{}' '{}'\" with administrator privileges",
                    q(&exe),
                    q(target)
                );
                let out = std::process::Command::new("osascript").args(["-e", &script]).output().map_err(|e| e.to_string())?;
                if out.status.success() {
                    return Ok(format!("Installed {}", target.display()));
                }
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                return Err(if err.contains("-128") { "Cancelled".into() } else { err });
            }
            Err(_) => {
                let home = std::env::var_os("HOME").ok_or("HOME not set")?;
                let dir = PathBuf::from(home).join(".local/bin");
                std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                let t = dir.join("wgitpui");
                let _ = std::fs::remove_file(&t);
                std::os::unix::fs::symlink(&exe, &t).map_err(|e| e.to_string())?;
                return Ok(format!("Installed {} (make sure it's on your PATH)", t.display()));
            }
        }
    }
    #[allow(unreachable_code)]
    Err(format!("Add {} to your PATH", exe.display()))
}
