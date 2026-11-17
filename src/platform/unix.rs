//! What Linux and macOS share: both are Unix underneath, so one copy of
//! the program per user, waiting for an old copy after an update, and
//! downloading updates work the same way on both.

use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn lock_file() -> Option<std::fs::File> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|dir| dir.is_dir()).unwrap_or_else(std::env::temp_dir);
    let name = format!("screen-portals-{}.lock", unsafe { libc::getuid() });
    std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(name)).ok()
}

fn try_lock(file: &std::fs::File) -> bool {
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// True if another copy of the application already owns this session.
pub fn already_running() -> bool {
    // The lock is kept for the life of the process on purpose.
    static LOCK: OnceLock<Option<std::fs::File>> = OnceLock::new();
    match LOCK.get_or_init(lock_file) {
        Some(file) => !try_lock(file),
        None => false,
    }
}

/// After an update the new copy starts while the old one is still closing.
/// Wait for the old one to go; returns false if it never does.
pub fn wait_for_previous_copy(patience: Duration) -> bool {
    let Some(file) = lock_file() else { return true };
    let deadline = Instant::now() + patience;
    loop {
        if try_lock(&file) {
            // Let go again, so that only `already_running` decides who
            // owns the session (closing the file releases the lock).
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Download one address from github.com over HTTPS, refusing anything
/// larger than `limit` bytes. Uses the system's own curl, named by its full
/// path so nothing else on the search path can stand in for it.
pub fn fetch_https(url: &str, limit: u64) -> Result<Vec<u8>, String> {
    if !url.starts_with("https://github.com/") {
        return Err("updates only come from github.com".into());
    }
    let curl = ["/usr/bin/curl", "/bin/curl"].into_iter().find(|path| Path::new(path).is_file()).ok_or("curl is not installed")?;
    let output = Command::new(curl)
        .args(["--silent", "--show-error", "--fail", "--location", "--proto", "=https", "--proto-redir", "=https"])
        .args(["--max-time", "300", "--max-filesize", &limit.to_string(), "--output", "-", url])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot start the download: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    if output.stdout.len() as u64 > limit {
        return Err("the download was larger than announced".into());
    }
    Ok(output.stdout)
}
