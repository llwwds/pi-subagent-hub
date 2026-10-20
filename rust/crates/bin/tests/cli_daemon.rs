//! Native CLI + daemon integration, isolated under a private `/tmp` home.
//!
//! This test never starts a model and never touches the production home. It
//! proves the Rust binary can initialize config, start its own daemon (bound to
//! a random loopback port), answer status, and stop through the HTTP control
//! socket.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const RUN_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(8);

struct Isolated {
    _temp: tempfile::TempDir,
    home: PathBuf,
    app_root: PathBuf,
}

impl Isolated {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp home");
        let home = temp.path().join("hub-home");
        let app_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("repository root")
            .to_path_buf();
        Self {
            _temp: temp,
            home,
            app_root,
        }
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("PIHUB_HOME", self.home.display().to_string()),
            ("PIHUB_APP_ROOT", self.app_root.display().to_string()),
            // Keep the test deterministic even if the caller has a Pi override.
            ("PIHUB_PI_CLI", self.app_root.join("vendor/pi-runtime/missing-cli.js").display().to_string()),
        ]
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pihub"));
        command
            .args(args)
            .envs(self.env())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        run_bounded(command, RUN_TIMEOUT, &format!("pihub {}", args.join(" ")))
    }

    fn json(&self, args: &[&str]) -> Value {
        let (status, stdout, stderr) = self.run(args);
        assert_eq!(status, 0, "command failed: {args:?}\nstdout={stdout}\nstderr={stderr}");
        let value: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|error| {
            panic!("invalid JSON envelope for {args:?}: {error}\nstdout={stdout}\nstderr={stderr}")
        });
        assert_eq!(value["ok"], true, "envelope not ok for {args:?}: {value}");
        value
    }
}

fn run_bounded(mut command: Command, timeout: Duration, label: &str) -> (i32, String, String) {
    let mut child = command.spawn().expect("spawn pihub");
    let started = Instant::now();
    loop {
        match child.try_wait().expect("wait pihub") {
            Some(status) => {
                let output = child.wait_with_output().expect("read pihub output");
                return (
                    status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&output.stdout).into_owned(),
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                );
            }
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
                let output = child.wait_with_output().expect("read killed output");
                panic!(
                    "timeout after {timeout:?}: {label}\nstdout={}\nstderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

struct DaemonGuard {
    home: PathBuf,
    pid: Option<u32>,
}

impl DaemonGuard {
    fn from_home(home: &Path) -> Self {
        let pid = read_pid(&home.join("state/daemon.json"));
        Self {
            home: home.to_path_buf(),
            pid,
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let Some(pid) = self.pid else {
            return;
        };
        if !process_alive(pid) {
            return;
        }
        // Best-effort cleanup of a test-only daemon. Never touches production.
        let _ = Command::new("kill").arg(pid.to_string()).status();
        let deadline = Instant::now() + STOP_TIMEOUT;
        while Instant::now() < deadline && process_alive(pid) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_file(self.home.join("state/daemon.json"));
        let _ = std::fs::remove_file(self.home.join("state/daemon.lock"));
    }
}

fn read_pid(path: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.get("pid")?.as_u64().map(|pid| pid as u32)
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[test]
fn native_cli_config_and_daemon_round_trip() {
    let isolated = Isolated::new();

    let (status, stdout, _) = isolated.run(&["--version"]);
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), pihub_contracts::APP_VERSION);

    isolated.json(&["config", "init", "--json"]);
    isolated.json(&["config", "validate", "--json"]);
    let paths = isolated.json(&["config", "paths", "--json"]);
    assert_eq!(
        paths["data"]["databasePath"],
        Value::String(isolated.home.join("state/hub.sqlite").display().to_string())
    );

    let started = isolated.json(&["daemon", "start", "--json"]);
    assert_eq!(started["data"]["started"], true, "first start should spawn: {started}");
    let mut guard = DaemonGuard::from_home(&isolated.home);
    assert!(guard.pid.is_some(), "daemon.json must publish a pid");

    let status = isolated.json(&["daemon", "status", "--json"]);
    assert_eq!(status["data"]["running"], true, "daemon should be healthy: {status}");

    let stopped = isolated.json(&["daemon", "stop", "--json"]);
    assert_eq!(stopped["data"]["stopping"], true, "stop route should acknowledge: {stopped}");

    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        if guard.pid.is_some_and(|pid| !process_alive(pid)) {
            guard.pid = None;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(guard.pid.is_none(), "daemon did not exit after stop");
}
