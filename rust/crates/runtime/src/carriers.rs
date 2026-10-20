//! Carrier registry: capabilities, diagnosis, and runtime construction.
//!
//! The factory is the only place that knows how a carrier maps onto a process
//! shape, and it is the injection point used by `pihub-bin`. It reads paths and
//! an environment snapshot; it never touches the Hub store.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pihub_contracts::{
    Carrier, CarrierCapabilities, CarrierDiagnostic, HubError, HubResult, HubPaths, LogSink,
    PromptChannel, Runtime, RuntimeEvent, RuntimeFactory, RuntimeLaunch, RuntimeRequestKind,
};
use tokio::sync::mpsc;

pub use crate::cli_runtime::CcSwitchRoute;
use crate::cli_runtime::{CliRuntime, CliSpec};
use crate::env::RuntimeEnv;
use crate::fsx;
use crate::mount::prepare_private_hub_cli;
use crate::pi_rpc::{PiRpcProcess, PiSpec};
use crate::proc::find_executable;

/// Environment keys a Pi child may inherit from the daemon (2.1 whitelist).
const PI_ENV_WHITELIST: [&str; 12] = [
    "PATH",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "SSL_CERT_FILE",
    "NODE_EXTRA_CA_CERTS",
    "SYSTEMROOT",
];

#[derive(Debug, Clone)]
pub struct CarrierFactoryConfig {
    pub paths: HubPaths,
    pub env: RuntimeEnv,
    /// Overrides for the pinned CLI paths (tests, custom installs).
    pub codex_executable: Option<PathBuf>,
    pub claude_executable: Option<PathBuf>,
    /// Node runtime that executes the pinned Pi bundle.
    pub node_program: Option<PathBuf>,
    /// Platform `pihub` entry point bundled skills may call.
    pub hub_cli_path: Option<PathBuf>,
    /// When present, Codex/Claude turns run through the private CC Switch route.
    pub cc_switch: Option<CcSwitchRoute>,
    /// Resolved from `PIHUB_LOG_DEBUG=1`; passed down to every runtime.
    pub protocol_debug: bool,
}

impl CarrierFactoryConfig {
    /// Defaults derived from the Hub home; every isolation-sensitive path comes
    /// from `HubPaths` so no caller has to re-derive it.
    ///
    /// The bundled-skill `pihub` route defaults to *this* Rust executable, not to
    /// a JavaScript entry point: the Rust build is the platform, so a Skill that
    /// shells out to `pihub` must land on the running binary. The 2.1
    /// `bin/pihub.js` path is only used when a caller explicitly sets
    /// `hub_cli_path` (reference/fallback configuration).
    pub fn new(paths: HubPaths, env: RuntimeEnv) -> Self {
        let hub_cli_path = std::env::current_exe().ok();
        Self {
            cc_switch: Some(CcSwitchRoute {
                executable: paths.cc_switch_cli_path.clone(),
                config_dir: paths.cc_switch_config_dir.clone(),
                home: paths.cc_switch_home.clone(),
                pi_dir: paths.cc_switch_pi_dir.clone(),
                codex_cli_path: paths.codex_cli_path.clone(),
                claude_cli_path: paths.claude_cli_path.clone(),
            }),
            paths,
            env,
            codex_executable: None,
            claude_executable: None,
            node_program: None,
            hub_cli_path,
            protocol_debug: crate::protocol_log::debug_enabled(),
        }
    }

    /// Raw protocol frames in `hub.log` require the explicit `PIHUB_LOG_DEBUG=1`
    /// opt-in; the default is a bounded summary only.
    pub fn with_protocol_debug(mut self, debug: bool) -> Self {
        self.protocol_debug = debug;
        self
    }

    /// Explicit override for the bundled-skill `pihub` route (for example the
    /// 2.1 `bin/pihub.js` path when running against the reference layout).
    pub fn with_hub_cli(mut self, hub_cli_path: PathBuf) -> Self {
        self.hub_cli_path = Some(hub_cli_path);
        self
    }

    /// Disable the CC Switch route: carriers then run directly with the
    /// carrier-native provider.
    pub fn without_cc_switch(mut self) -> Self {
        self.cc_switch = None;
        self
    }
}

pub struct CarrierFactory {
    config: CarrierFactoryConfig,
}

impl CarrierFactory {
    pub fn new(config: CarrierFactoryConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &CarrierFactoryConfig {
        &self.config
    }

    fn executable_for(&self, carrier: Carrier) -> PathBuf {
        match carrier {
            Carrier::Codex => self
                .config
                .codex_executable
                .clone()
                .unwrap_or_else(|| self.config.paths.codex_cli_path.clone()),
            Carrier::Claude => self
                .config
                .claude_executable
                .clone()
                .unwrap_or_else(|| self.config.paths.claude_cli_path.clone()),
            Carrier::Pi => self.config.paths.pi_cli_path.clone(),
        }
    }

    fn node_program(&self) -> HubResult<PathBuf> {
        if let Some(program) = &self.config.node_program {
            return Ok(program.clone());
        }
        find_executable(Path::new("node"), &self.config.env.path_map()).ok_or_else(|| {
            HubError::new(
                "carrier_unavailable",
                "Node runtime is unavailable for the pinned Pi bundle",
            )
        })
    }

    fn node_dir(&self) -> Option<PathBuf> {
        self.node_program()
            .ok()
            .and_then(|program| program.parent().map(Path::to_path_buf))
    }

    fn capabilities(carrier: Carrier) -> CarrierCapabilities {
        match carrier {
            Carrier::Pi => CarrierCapabilities {
                messages: vec![
                    RuntimeRequestKind::Prompt,
                    RuntimeRequestKind::Steer,
                    RuntimeRequestKind::FollowUp,
                    RuntimeRequestKind::Abort,
                    RuntimeRequestKind::ClearQueue,
                    RuntimeRequestKind::GetState,
                ],
                events: true,
                skill_loading_selection: true,
                skill_isolation: false,
                system_prompt: Some(PromptChannel::Append),
            },
            Carrier::Codex => CarrierCapabilities {
                messages: vec![
                    RuntimeRequestKind::Prompt,
                    RuntimeRequestKind::Abort,
                    RuntimeRequestKind::ClearQueue,
                    RuntimeRequestKind::GetState,
                ],
                events: true,
                skill_loading_selection: true,
                skill_isolation: false,
                system_prompt: Some(PromptChannel::Developer),
            },
            Carrier::Claude => CarrierCapabilities {
                messages: vec![
                    RuntimeRequestKind::Prompt,
                    RuntimeRequestKind::Abort,
                    RuntimeRequestKind::ClearQueue,
                    RuntimeRequestKind::GetState,
                ],
                events: true,
                skill_loading_selection: true,
                skill_isolation: false,
                system_prompt: Some(PromptChannel::Append),
            },
        }
    }

    fn display_name(carrier: Carrier) -> &'static str {
        match carrier {
            Carrier::Pi => "Pi Agent CLI",
            Carrier::Codex => "Codex CLI",
            Carrier::Claude => "Claude Code CLI",
        }
    }

    fn diagnostic(carrier: Carrier, reason: Option<String>) -> CarrierDiagnostic {
        CarrierDiagnostic {
            id: carrier,
            display_name: Self::display_name(carrier).to_string(),
            capabilities: Self::capabilities(carrier),
            available: reason.is_none(),
            reason,
        }
    }

    /// Per-carrier availability. Never executes a carrier.
    fn check(&self, carrier: Carrier) -> Option<String> {
        match carrier {
            Carrier::Pi => {
                if self.config.paths.pi_cli_path.exists() {
                    None
                } else {
                    Some(
                        "Pi CLI bundle is unavailable; build the pinned Pi runtime first"
                            .to_string(),
                    )
                }
            }
            Carrier::Codex | Carrier::Claude => {
                let executable = self.executable_for(carrier);
                if find_executable(&executable, &self.config.env.path_map()).is_none() {
                    return Some(format!(
                        "{} executable is unavailable: {}",
                        Self::display_name(carrier),
                        executable.display()
                    ));
                }
                if let Some(route) = &self.config.cc_switch {
                    if find_executable(&route.executable, &self.config.env.path_map()).is_none() {
                        return Some("app-private CC Switch CLI is unavailable".to_string());
                    }
                }
                None
            }
        }
    }

    fn pi_spec(&self, launch: &RuntimeLaunch) -> HubResult<PiSpec> {
        let private_home = launch.state_dir.join("pi-home");
        fsx::ensure_private_dir(&private_home).map_err(|error| {
            HubError::new(
                "carrier_setup_failed",
                format!("cannot prepare {}: {error}", private_home.display()),
            )
        })?;
        let hub_bin = prepare_private_hub_cli(
            &private_home,
            Some(&self.config.paths.home),
            self.config.hub_cli_path.as_deref(),
        )?;

        // Pi only sees a narrow, explicit environment; HOME stays private.
        let mut env = self.config.env.pick(&PI_ENV_WHITELIST);
        env.insert("HOME", private_home.to_string_lossy().into_owned());
        if let Some(bin) = &hub_bin {
            env.insert(
                "PIHUB_HOME",
                self.config.paths.home.to_string_lossy().into_owned(),
            );
            let mut entries: Vec<PathBuf> = vec![bin.clone()];
            if let Some(node_dir) = self.node_dir() {
                if !entries.contains(&node_dir) {
                    entries.push(node_dir);
                }
            }
            match self.config.env.get_str("PATH") {
                Some(existing) => entries.extend(std::env::split_paths(&existing)),
                None => entries.extend([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]),
            }
            let joined = std::env::join_paths(entries.iter().map(|path| path.as_os_str()))
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "/usr/bin:/bin".to_string());
            env.insert("PATH", joined);
        }

        Ok(PiSpec {
            launch: launch.clone(),
            program: self.node_program()?,
            cli_path: self.config.paths.pi_cli_path.clone(),
            env,
            protocol_debug: self.config.protocol_debug,
        })
    }

    fn cli_spec(&self, launch: &RuntimeLaunch) -> CliSpec {
        CliSpec {
            launch: launch.clone(),
            executable: self.executable_for(launch.carrier),
            env: self.config.env.clone(),
            hub_home: Some(self.config.paths.home.clone()),
            hub_cli: self.config.hub_cli_path.clone(),
            node_dir: self.node_dir(),
            cc_switch: self.config.cc_switch.clone(),
            protocol_debug: self.config.protocol_debug,
        }
    }
}

#[async_trait::async_trait]
impl RuntimeFactory for CarrierFactory {
    async fn diagnose(&self, carrier: Carrier) -> HubResult<CarrierDiagnostic> {
        Ok(Self::diagnostic(carrier, self.check(carrier)))
    }

    async fn list(&self) -> HubResult<Vec<CarrierDiagnostic>> {
        Ok(Carrier::ALL
            .iter()
            .map(|carrier| Self::diagnostic(*carrier, self.check(*carrier)))
            .collect())
    }

    fn create(
        &self,
        launch: RuntimeLaunch,
        events: mpsc::Sender<RuntimeEvent>,
        log: Option<Arc<dyn LogSink>>,
    ) -> HubResult<Arc<dyn Runtime>> {
        // A carrier that cannot select its Skill loading set cannot honour an
        // explicit authorization; refuse instead of silently loading everything.
        let selects_skills = Self::capabilities(launch.carrier).skill_loading_selection;
        if launch.authorization.is_some() && !selects_skills {
            return Err(HubError::new(
                "carrier_cannot_select_skills",
                format!(
                    "carrier {} cannot select the Skill loading set",
                    launch.carrier
                ),
            ));
        }
        match launch.carrier {
            Carrier::Pi => {
                let spec = self.pi_spec(&launch)?;
                Ok(Arc::new(PiRpcProcess::new(spec, events, log)))
            }
            Carrier::Codex | Carrier::Claude => {
                let spec = self.cli_spec(&launch);
                let carrier = launch.carrier;
                Ok(Arc::new(CliRuntime::new(carrier, spec, events, log)?))
            }
        }
    }
}

/// Declared capabilities of every built-in carrier, without building a factory.
pub fn builtin_capabilities() -> Vec<CarrierDiagnostic> {
    let mut seen = BTreeSet::new();
    Carrier::ALL
        .iter()
        .filter(|carrier| seen.insert(carrier.as_str()))
        .map(|carrier| CarrierFactory::diagnostic(*carrier, None))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{CarrierFactory, CarrierFactoryConfig};
    use crate::env::RuntimeEnv;
    use pihub_contracts::{Carrier, HubPaths, PromptChannel, RuntimeFactory, RuntimeRequestKind};
    use std::path::{Path, PathBuf};

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            let base = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp");
            let path = base.join(format!(
                "pihub-carriers-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|value| value.as_nanos())
                    .unwrap_or_default()
            ));
            std::fs::create_dir_all(&path).expect("temp root");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn executable_fixture(root: &Path, name: &str) -> PathBuf {
        let path = root.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        path
    }

    #[test]
    fn builtin_capabilities_match_the_2_1_carrier_contract() {
        let carriers = super::builtin_capabilities();
        assert_eq!(carriers.len(), 3);

        let pi = &carriers[0];
        assert_eq!(pi.id, Carrier::Pi);
        assert_eq!(pi.display_name, "Pi Agent CLI");
        assert!(pi.capabilities.events);
        assert!(pi.capabilities.messages.contains(&RuntimeRequestKind::Steer));
        assert!(pi.capabilities.messages.contains(&RuntimeRequestKind::FollowUp));
        assert!(pi.capabilities.skill_loading_selection);
        assert!(!pi.capabilities.skill_isolation);
        assert_eq!(pi.capabilities.system_prompt, Some(PromptChannel::Append));

        let codex = &carriers[1];
        assert_eq!(codex.id, Carrier::Codex);
        assert!(!codex.capabilities.messages.contains(&RuntimeRequestKind::Steer));
        assert_eq!(codex.capabilities.system_prompt, Some(PromptChannel::Developer));

        let claude = &carriers[2];
        assert_eq!(claude.id, Carrier::Claude);
        assert_eq!(claude.capabilities.system_prompt, Some(PromptChannel::Append));
    }

    #[test]
    fn the_bundled_skill_route_defaults_to_this_rust_binary() {
        let root = TempRoot::new("route-default");
        let paths = HubPaths::resolve(root.path().join("home"), root.path().join("app"));
        let config = CarrierFactoryConfig::new(paths.clone(), RuntimeEnv::default());
        let current = std::env::current_exe().expect("current exe");
        assert_eq!(config.hub_cli_path.as_deref(), Some(current.as_path()));
        assert_ne!(
            config.hub_cli_path,
            Some(paths.app_root.join("bin").join("pihub.js")),
            "the JavaScript entry point is never the default route"
        );

        // An explicit 2.1 reference path is still possible.
        let explicit = paths.app_root.join("bin").join("pihub.js");
        let config = config.with_hub_cli(explicit.clone());
        assert_eq!(config.hub_cli_path, Some(explicit));
    }

    #[tokio::test]
    async fn diagnosis_reports_missing_carriers_without_executing_them() {
        let root = TempRoot::new("diagnose");
        let paths = HubPaths::resolve(root.path().join("home"), root.path().join("app"));
        let codex = executable_fixture(root.path(), "fake-codex");
        let mut config = CarrierFactoryConfig::new(
            paths,
            RuntimeEnv::from_strs([("PATH", "/usr/bin:/bin")]),
        );
        config.codex_executable = Some(codex);
        config.claude_executable = Some(root.path().join("missing-claude"));
        config.node_program = Some(PathBuf::from("/usr/bin/true"));
        config.cc_switch = None;
        let factory = CarrierFactory::new(config);

        let codex_diagnostic = factory.diagnose(Carrier::Codex).await.expect("diagnose");
        assert!(codex_diagnostic.available, "{:?}", codex_diagnostic.reason);

        let claude_diagnostic = factory.diagnose(Carrier::Claude).await.expect("diagnose");
        assert!(!claude_diagnostic.available);
        assert!(
            claude_diagnostic
                .reason
                .unwrap_or_default()
                .contains("Claude Code CLI executable is unavailable")
        );

        let pi_diagnostic = factory.diagnose(Carrier::Pi).await.expect("diagnose");
        assert!(!pi_diagnostic.available);
        assert!(
            pi_diagnostic
                .reason
                .clone()
                .unwrap_or_default()
                .contains("Pi CLI bundle is unavailable")
        );

        let list = factory.list().await.expect("list");
        assert_eq!(list.len(), 3);
        assert_eq!(list.iter().filter(|item| item.available).count(), 1);

        // Every diagnostic keeps the exact /v2 row shape (`label` = displayName).
        let value = pi_diagnostic.api_value();
        assert_eq!(value["id"], "pi");
        assert_eq!(value["label"], "Pi Agent CLI");
        assert_eq!(value["available"], false);
        // An unavailable carrier carries its reason string...
        assert!(
            value["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("Pi CLI bundle is unavailable"),
            "{value}"
        );

        // ...and an available one must still emit the key with `null` (2.1 parity:
        // the key is never omitted).
        let available_value = codex_diagnostic.api_value();
        assert_eq!(available_value["available"], true);
        assert!(
            available_value.get("reason").is_some(),
            "the reason key must always be present: {available_value}"
        );
        assert!(available_value["reason"].is_null(), "{available_value}");
    }

    #[tokio::test]
    async fn cc_switch_route_gates_cli_availability() {
        let root = TempRoot::new("switch-route");
        let paths = HubPaths::resolve(root.path().join("home"), root.path().join("app"));
        let cli = executable_fixture(root.path(), "fake-codex");
        let mut config = CarrierFactoryConfig::new(
            paths,
            RuntimeEnv::from_strs([("PATH", "/usr/bin:/bin")]),
        );
        config.codex_executable = Some(cli.clone());
        config.node_program = Some(PathBuf::from("/usr/bin/true"));
        // The private CC Switch binary is not installed yet.
        let factory = CarrierFactory::new(config);
        let diagnostic = factory.diagnose(Carrier::Codex).await.expect("diagnose");
        assert!(!diagnostic.available);
        assert!(
            diagnostic
                .reason
                .unwrap_or_default()
                .contains("app-private CC Switch CLI is unavailable")
        );
    }
}
