use std::path::{Component, Path, PathBuf};

/// Absolute Hub home layout, mirroring `src/paths.js:resolveHubPaths`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubPaths {
    pub app_root: PathBuf,
    pub home: PathBuf,
    pub config_dir: PathBuf,
    pub shared_pi_dir: PathBuf,
    pub models_path: PathBuf,
    pub auth_path: PathBuf,
    pub profile_path: PathBuf,
    pub state_dir: PathBuf,
    pub database_path: PathBuf,
    pub daemon_path: PathBuf,
    pub control_path: PathBuf,
    pub lock_path: PathBuf,
    pub daemon_stdout_path: PathBuf,
    pub daemon_stderr_path: PathBuf,
    pub agents_dir: PathBuf,
    pub codex_cli_path: PathBuf,
    pub claude_cli_path: PathBuf,
    pub cc_switch_cli_path: PathBuf,
    pub cc_switch_config_dir: PathBuf,
    pub cc_switch_home: PathBuf,
    pub cc_switch_pi_dir: PathBuf,
    pub pi_lock_path: PathBuf,
    pub pi_cli_path: PathBuf,
}

impl HubPaths {
    /// Resolve all paths from an explicit home and app root. `PIHUB_PI_CLI`
    /// still overrides the pinned Pi bundle path, as in the 2.1 source.
    pub fn resolve(home: impl AsRef<Path>, app_root: impl AsRef<Path>) -> Self {
        let home = absolute_normalized(home.as_ref());
        let app_root = absolute_normalized(app_root.as_ref());
        let config_dir = home.join("config");
        let shared_pi_dir = config_dir.join("pi-shared");
        let state_dir = home.join("state");
        let cc_switch_config_dir = home.join("cc-switch").join("data");
        let pi_cli_path = std::env::var_os("PIHUB_PI_CLI")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                app_root
                    .join("vendor")
                    .join("pi-runtime")
                    .join("node_modules")
                    .join("@earendil-works")
                    .join("pi-coding-agent")
                    .join("dist")
                    .join("bundle")
                    .join("cli.js")
            });
        Self {
            app_root: app_root.clone(),
            home: home.clone(),
            config_dir: config_dir.clone(),
            shared_pi_dir: shared_pi_dir.clone(),
            models_path: shared_pi_dir.join("models.json"),
            auth_path: shared_pi_dir.join("auth.json"),
            profile_path: config_dir.join("default-profile.json"),
            state_dir: state_dir.clone(),
            database_path: state_dir.join("hub.sqlite"),
            daemon_path: state_dir.join("daemon.json"),
            control_path: state_dir.join("control.json"),
            lock_path: state_dir.join("daemon.lock"),
            daemon_stdout_path: state_dir.join("daemon.stdout.log"),
            daemon_stderr_path: state_dir.join("daemon.stderr.log"),
            agents_dir: home.join("agents"),
            codex_cli_path: home
                .join("toolchains")
                .join("codex")
                .join("node_modules")
                .join(".bin")
                .join("codex"),
            claude_cli_path: home
                .join("toolchains")
                .join("claude")
                .join("node_modules")
                .join(".bin")
                .join("claude"),
            cc_switch_cli_path: home
                .join("toolchains")
                .join("cc-switch-cli")
                .join("cc-switch"),
            cc_switch_config_dir,
            cc_switch_home: home.join("cc-switch").join("home"),
            cc_switch_pi_dir: home.join("cc-switch").join("pi-agent"),
            pi_lock_path: app_root.join("vendor").join("pi.lock.json"),
            pi_cli_path,
        }
    }

    /// `PIHUB_HOME` if set, otherwise the platform default home.
    pub fn from_env(app_root: impl AsRef<Path>) -> Self {
        let home = std::env::var_os("PIHUB_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(default_home);
        Self::resolve(home, app_root)
    }

    pub fn default_home() -> PathBuf {
        default_home()
    }

    /// True when the Hub home is outside (and not equal to) the source tree.
    pub fn is_state_home_outside_app(&self) -> bool {
        match self.home.strip_prefix(&self.app_root) {
            // Equal or nested inside the source tree is rejected.
            Ok(_) => false,
            Err(_) => true,
        }
    }
}

pub fn default_home() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        return home_env().join("Library/Application Support").join("pi-subagent-hub");
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local).join("pi-subagent-hub");
        }
        return home_env().join("AppData").join("Local").join("pi-subagent-hub");
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(data).join("pi-subagent-hub");
        }
        home_env().join(".local").join("share").join("pi-subagent-hub")
    }
}

fn home_env() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn absolute_normalized(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    normalize_lexical(&absolute)
}

pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => output.push(prefix.as_os_str()),
            Component::RootDir => output.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    output.push("..");
                }
            }
            Component::Normal(part) => output.push(part),
        }
    }
    output
}
