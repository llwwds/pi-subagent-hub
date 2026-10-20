//! Process primitives shared by every carrier runtime: executable lookup,
//! signal delivery (optionally to a process group) and the 2.1 escalation
//! ladder `SIGTERM -> 5s -> SIGKILL`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

/// Grace period before escalating to SIGKILL (2.1: 50 x 100ms).
pub const TERMINATE_GRACE: Duration = Duration::from_millis(5_000);
pub const TERMINATE_POLL: Duration = Duration::from_millis(100);
pub const TERMINATE_POLLS: usize = 50;

#[cfg(unix)]
mod signals {
    pub const SIGTERM: i32 = libc::SIGTERM;
    pub const SIGKILL: i32 = libc::SIGKILL;
}

#[cfg(unix)]
pub use signals::{SIGKILL, SIGTERM};

pub fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// 2.1 `findExecutable`: an absolute path must itself be executable, a value
/// containing a separator but not absolute is rejected (never resolved against
/// the cwd), everything else is scanned on `PATH`.
pub fn find_executable(name_or_path: &Path, env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if name_or_path.is_absolute() {
        return is_executable(name_or_path).then(|| name_or_path.to_path_buf());
    }
    let text = name_or_path.to_string_lossy();
    if text.contains('/') || text.contains('\\') {
        return None;
    }
    let path_env = env.get("PATH")?;
    for directory in std::env::split_paths(std::ffi::OsStr::new(path_env.as_str())) {
        if directory.as_os_str().is_empty() {
            continue;
        }
        let candidate = directory.join(name_or_path);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
pub fn signal_process(pid: u32, signal: i32, group: bool) -> std::io::Result<()> {
    if pid == 0 {
        return Ok(());
    }
    let target: i32 = if group { -(pid as i32) } else { pid as i32 };
    // Safety: `kill` is async-signal-safe and only takes plain integers.
    let result = unsafe { libc::kill(target, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    // A process that already exited is not an error (2.1 ignores ESRCH).
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(not(unix))]
pub fn signal_process(_pid: u32, _signal: i32, _group: bool) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "signal delivery is only implemented on unix",
    ))
}

#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0
}

#[cfg(not(unix))]
pub fn is_process_alive(_pid: u32) -> bool {
    false
}

/// SIGTERM, wait up to 5s, then SIGKILL. `alive` reports whether the child is
/// still running; the loop mirrors the 2.1 50 x 100ms poll.
pub async fn terminate(pid: u32, group: bool, alive: impl Fn() -> bool) {
    if pid == 0 || !alive() {
        return;
    }
    let _ = signal_process(pid, SIGTERM, group);
    for _ in 0..TERMINATE_POLLS {
        if !alive() {
            return;
        }
        tokio::time::sleep(TERMINATE_POLL).await;
    }
    if alive() {
        let _ = signal_process(pid, SIGKILL, group);
    }
}

/// Exit code plus the 2.1 signal name (`SIGTERM`, `SIGKILL`, ...).
pub fn decode_exit(status: ExitStatus) -> (Option<i32>, Option<String>) {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return (status.code(), Some(signal_name(signal).to_string()));
        }
    }
    (status.code(), None)
}

pub fn signal_name(signal: i32) -> &'static str {
    match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        6 => "SIGABRT",
        8 => "SIGFPE",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => "SIGUNKNOWN",
    }
}

/// Render an optional numeric value the way JavaScript interpolation does
/// (`null` for `undefined`/`null`), so rejected-request messages stay identical.
pub fn js_optional(value: Option<i32>) -> String {
    match value {
        Some(number) => number.to_string(),
        None => "null".to_string(),
    }
}

pub fn js_optional_str(value: Option<&str>) -> String {
    match value {
        Some(text) => text.to_string(),
        None => "null".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{find_executable, is_process_alive, js_optional, js_optional_str, signal_name};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    fn path_env(entries: &[&Path]) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        let joined = std::env::join_paths(entries).expect("join paths");
        map.insert("PATH".to_string(), joined.to_string_lossy().into_owned());
        map
    }

    #[test]
    fn absolute_paths_must_be_executable_and_relative_segments_are_rejected() {
        let env = path_env(&[]);
        assert_eq!(find_executable(Path::new("/bin/sh"), &env), Some(PathBuf::from("/bin/sh")));
        assert_eq!(find_executable(Path::new("/bin/definitely-missing-pihub"), &env), None);
        assert_eq!(find_executable(Path::new("sub/dir/tool"), &env), None);
    }

    #[test]
    fn finds_tools_on_path_and_reports_own_pid_alive() {
        let env = path_env(&[Path::new("/bin"), Path::new("/usr/bin")]);
        assert_eq!(find_executable(Path::new("sh"), &env), Some(PathBuf::from("/bin/sh")));
        assert_eq!(find_executable(Path::new("pihub-missing-tool"), &env), None);
        assert!(is_process_alive(std::process::id()));
        assert!(!is_process_alive(0));
    }

    #[test]
    fn javascript_style_option_rendering_and_signal_names() {
        assert_eq!(js_optional(None), "null");
        assert_eq!(js_optional(Some(0)), "0");
        assert_eq!(js_optional_str(None), "null");
        assert_eq!(js_optional_str(Some("SIGKILL")), "SIGKILL");
        assert_eq!(signal_name(15), "SIGTERM");
        assert_eq!(signal_name(9), "SIGKILL");
    }
}
