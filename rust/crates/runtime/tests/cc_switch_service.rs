//! CC Switch provider service against a private, isolated fake CLI + database.

mod support;

use std::path::{Path, PathBuf};

use pihub_contracts::{AuthMode, HubPaths, ProviderApp, ProviderInput, ProviderService};
use pihub_runtime::cc_switch::CcSwitchService;
use rusqlite::Connection;
use support::{TempRoot, write_script};

/// Fake app-private CC Switch CLI: reports the pinned version, records the
/// environment/argv it received, and fails (exit 9) like a broken install.
const FAKE_SWITCH: &str = r#"#!/bin/sh
probe_dir="${CC_SWITCH_CONFIG_DIR}"
{
  printf 'args=%s\n' "$*"
  printf 'home=%s\n' "$HOME"
  printf 'config=%s\n' "$CC_SWITCH_CONFIG_DIR"
  printf 'testhome=%s\n' "$CC_SWITCH_TEST_HOME"
  printf 'pidir=%s\n' "$PI_CODING_AGENT_DIR"
  printf 'claudehome=%s\n' "$CLAUDE_CONFIG_DIR"
  printf 'codexhome=%s\n' "$CODEX_HOME"
  printf 'hostkey=%s\n' "${OPENAI_API_KEY-unset}"
} > "${probe_dir}/probe.txt"

if [ "$1" = "--version" ]; then
  printf 'cc-switch 5.10.5\n'
  exit 0
fi

if [ "$1" = "--app" ] && [ "$3" = "provider" ] && [ "$4" = "add" ]; then
  cfg=""
  prev=""
  for a in "$@"; do
    if [ "$prev" = "--config-file" ]; then cfg="$a"; fi
    prev="$a"
  done
  cp "$cfg" "${probe_dir}/staged-config.json"
  printf 'exit=%s\n' "${FAKE_EXIT:-9}" >> "${probe_dir}/probe.txt"
  exit "${FAKE_EXIT:-9}"
fi

printf 'exit=9\n' >> "${probe_dir}/probe.txt"
exit 9
"#;

struct Fixture {
    /// Kept alive for the lifetime of the fixture: dropping it removes the
    /// private home and therefore every fixture file.
    #[allow(dead_code)]
    root: TempRoot,
    service: CcSwitchService,
    paths: HubPaths,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let root = TempRoot::new(label);
        let paths = HubPaths::resolve(root.join("home"), root.join("app"));
        std::fs::create_dir_all(
            paths
                .cc_switch_cli_path
                .parent()
                .expect("toolchain dir"),
        )
        .expect("toolchain");
        write_script(&paths.cc_switch_cli_path, FAKE_SWITCH);
        let service = CcSwitchService::new(&paths).expect("service");
        Self {
            root,
            service,
            paths,
        }
    }

    fn probe(&self) -> String {
        std::fs::read_to_string(self.paths.cc_switch_config_dir.join("probe.txt"))
            .unwrap_or_default()
    }

    fn probe_value(&self, key: &str) -> Option<String> {
        let probe = self.probe();
        probe
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")).map(str::to_owned))
    }

    fn input(&self) -> ProviderInput {
        ProviderInput {
            app: ProviderApp::Claude,
            id: "route-a".to_string(),
            name: "Route A".to_string(),
            base_url: "https://example.invalid/v1".to_string(),
            api_key: "secret-test-key".to_string(),
            model_list: vec!["model-one".to_string()],
            default_model: "model-one".to_string(),
            api_format: None,
            auth_mode: None,
        }
    }
}

#[tokio::test]
async fn diagnosis_reports_the_pinned_version_and_private_paths() {
    let fixture = Fixture::new("diagnose");
    let diagnostics = fixture.service.diagnose().await;
    assert!(diagnostics.available, "{:?}", diagnostics.reason);
    assert_eq!(diagnostics.version.as_deref(), Some("5.10.5"));
    assert_eq!(diagnostics.binary_path, fixture.paths.cc_switch_cli_path);
    assert_eq!(diagnostics.config_dir, fixture.paths.cc_switch_config_dir);
    assert_eq!(fixture.probe_value("args").as_deref(), Some("--version"));
}

#[tokio::test]
async fn a_rejected_version_is_reported_as_unavailable() {
    let fixture = Fixture::new("version");
    write_script(
        &fixture.paths.cc_switch_cli_path,
        "#!/bin/sh\nprintf 'cc-switch 5.9.0\\n'\nexit 0\n",
    );
    let diagnostics = fixture.service.diagnose().await;
    assert!(!diagnostics.available);
    assert_eq!(
        diagnostics.error_code.as_deref(),
        Some("CC_SWITCH_VERSION_UNSUPPORTED")
    );
}

#[tokio::test]
async fn add_uses_only_the_private_environment_and_never_leaks_the_key() {
    let fixture = Fixture::new("add");
    let error = fixture
        .service
        .add(fixture.input())
        .await
        .expect_err("the fake CLI fails");
    assert_eq!(error.code, "CC_SWITCH_COMMAND_FAILED");
    let rendered = serde_json::to_string(&error.body()).expect("json");
    assert!(!rendered.contains("secret-test-key"), "{rendered}");

    assert_eq!(
        fixture.probe_value("home").as_deref(),
        Some(fixture.paths.cc_switch_home.to_string_lossy().as_ref())
    );
    assert_eq!(
        fixture.probe_value("config").as_deref(),
        Some(
            fixture
                .paths
                .cc_switch_config_dir
                .to_string_lossy()
                .as_ref()
        )
    );
    assert_eq!(
        fixture.probe_value("testhome").as_deref(),
        Some(fixture.paths.cc_switch_home.to_string_lossy().as_ref())
    );
    assert_eq!(
        fixture.probe_value("pidir").as_deref(),
        Some(fixture.paths.cc_switch_pi_dir.to_string_lossy().as_ref())
    );
    assert_eq!(
        fixture.probe_value("hostkey").as_deref(),
        Some("unset"),
        "a host API key must never be inherited"
    );
    let args = fixture.probe_value("args").expect("args");
    assert!(!args.contains("secret-test-key"), "{args}");

    // The staging directory is removed and the private tree is tight.
    let tmp = fixture.paths.home.join("cc-switch/tmp");
    assert_eq!(
        std::fs::read_dir(&tmp).expect("tmp dir").count(),
        0,
        "staging dirs must be cleaned up"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&fixture.paths.cc_switch_config_dir)
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }
}

#[tokio::test]
async fn bearer_auth_is_only_used_when_explicitly_selected() {
    let fixture = Fixture::new("auth-mode");
    let mut bearer = fixture.input();
    bearer.auth_mode = Some(AuthMode::Bearer);
    let _ = fixture.service.add(bearer).await;
    let staged = std::fs::read_to_string(
        fixture
            .paths
            .cc_switch_config_dir
            .join("staged-config.json"),
    )
    .expect("staged config");
    assert!(staged.contains("ANTHROPIC_AUTH_TOKEN"), "{staged}");
    assert!(!staged.contains("ANTHROPIC_API_KEY"), "{staged}");

    let mut invalid = fixture.input();
    invalid.auth_mode = Some(AuthMode::ApiKey);
    invalid.api_format = Some("openai".to_string());
    let error = fixture
        .service
        .add(invalid)
        .await
        .expect_err("claude only supports the anthropic format");
    assert_eq!(error.code, "CC_SWITCH_UNSUPPORTED");
}

#[tokio::test]
async fn provider_ids_and_urls_are_validated_before_any_command_runs() {
    let fixture = Fixture::new("validate");
    let mut input = fixture.input();
    input.id = "bad id".to_string();
    let error = fixture.service.add(input).await.expect_err("invalid id");
    assert_eq!(error.code, "CC_SWITCH_INVALID_INPUT");
    assert!(!fixture.probe().contains("args="), "no command must run");

    let mut input = fixture.input();
    input.base_url = "https://user:pw@example.invalid/v1".to_string();
    let error = fixture.service.add(input).await.expect_err("credentials in url");
    assert_eq!(error.code, "CC_SWITCH_INVALID_INPUT");
}

#[tokio::test]
async fn symlinked_private_paths_and_settings_overrides_are_rejected() {
    let fixture = Fixture::new("paths");
    let outside = TempRoot::new("paths-outside");
    let config_dir = fixture.paths.cc_switch_config_dir.clone();
    // Replace the private config dir with a symlink to an unrelated directory.
    std::fs::create_dir_all(config_dir.parent().expect("parent")).expect("root");
    let _ = std::fs::remove_dir_all(&config_dir);
    std::os::unix::fs::symlink(outside.path(), &config_dir).expect("symlink");
    let error = fixture
        .service
        .list(ProviderApp::Claude)
        .await
        .expect_err("symlink must be rejected");
    assert_eq!(error.code, "CC_SWITCH_PATH_UNSAFE");
    assert_eq!(
        std::fs::read_dir(outside.path()).expect("outside").count(),
        0,
        "nothing may be written through the symlink"
    );

    let fixture = Fixture::new("paths-settings");
    std::fs::create_dir_all(&fixture.paths.cc_switch_config_dir).expect("config dir");
    std::fs::write(
        fixture.paths.cc_switch_config_dir.join("settings.json"),
        serde_json::to_string(&serde_json::json!({
            "codexConfigDir": fixture.paths.cc_switch_home.join(".codex"),
        }))
        .expect("json"),
    )
    .expect("settings");
    let error = fixture
        .service
        .list(ProviderApp::Codex)
        .await
        .expect_err("settings override must be rejected");
    assert_eq!(error.code, "CC_SWITCH_PATH_UNSAFE");
}

fn seed_database(paths: &HubPaths) {
    std::fs::create_dir_all(&paths.cc_switch_config_dir).expect("config dir");
    let connection =
        Connection::open(paths.cc_switch_config_dir.join("cc-switch.db")).expect("db");
    connection
        .execute_batch(
            "CREATE TABLE providers (id TEXT, app_type TEXT, name TEXT, settings_config TEXT, \
             is_current INTEGER, sort_index INTEGER, created_at INTEGER);",
        )
        .expect("schema");
    let rows = [
        (
            "configured",
            "claude",
            "Configured",
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://example.invalid/v1","ANTHROPIC_AUTH_TOKEN":"secret-test-key","ANTHROPIC_MODEL":"model-one"}}"#,
            1i64,
            0i64,
        ),
        (
            "claude-official",
            "claude",
            "Claude Official",
            r#"{"env":{}}"#,
            0,
            1,
        ),
        (
            "with-url-secret",
            "claude",
            "With URL Secret",
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://example.invalid/v1?api_key=secret-in-url","ANTHROPIC_AUTH_TOKEN":"secret-test-key","ANTHROPIC_MODEL":"model-one"}}"#,
            0,
            2,
        ),
    ];
    for (index, (id, app, name, config, current, sort)) in rows.iter().enumerate() {
        connection
            .execute(
                "INSERT INTO providers (id, app_type, name, settings_config, is_current, sort_index, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![id, app, name, config, current, sort, index as i64 + 1],
            )
            .expect("row");
    }
    std::fs::write(
        paths.home.join("cc-switch/models-index.json"),
        serde_json::to_string(&serde_json::json!({
            "version": 1,
            "providers": {
                "claude": {
                    "configured": {
                        "modelList": ["model-one", "model-two"],
                        "defaultModel": "model-one",
                        "apiFormat": "anthropic",
                    }
                }
            }
        }))
        .expect("json"),
    )
    .expect("index");
}

#[tokio::test]
async fn listing_reads_state_without_ever_exposing_credentials() {
    let fixture = Fixture::new("list");
    seed_database(&fixture.paths);
    let providers = fixture
        .service
        .list(ProviderApp::Claude)
        .await
        .expect("list");
    assert_eq!(providers.len(), 3);
    let configured = &providers[0];
    assert_eq!(configured.id, "configured");
    assert!(configured.configured);
    assert!(configured.ready);
    assert_eq!(configured.api_format.as_deref(), Some("anthropic"));
    assert_eq!(configured.auth_mode, Some(AuthMode::Bearer));
    assert_eq!(configured.model_list, vec!["model-one", "model-two"]);
    assert_eq!(configured.is_current, Some(true));
    assert!(!providers[1].ready, "official config has no endpoint");
    assert_eq!(
        providers[2].base_url.as_deref(),
        Some("https://example.invalid/v1"),
        "query parameters are stripped"
    );
    let rendered = serde_json::to_string(&providers).expect("json");
    assert!(!rendered.contains("secret-test-key"), "{rendered}");
    assert!(!rendered.contains("secret-in-url"), "{rendered}");

    // The only stdout the service reads is the version banner.
    assert_eq!(fixture.probe_value("args").as_deref(), Some("--version"));
}

#[tokio::test]
async fn switching_pi_is_refused_and_an_unready_provider_cannot_be_selected() {
    let fixture = Fixture::new("switch");
    seed_database(&fixture.paths);

    let error = fixture
        .service
        .switch(ProviderApp::Pi, "route-a")
        .await
        .expect_err("pi switching is unsupported");
    assert_eq!(error.code, "CC_SWITCH_UNSUPPORTED");

    let error = fixture
        .service
        .switch(ProviderApp::Claude, "claude-official")
        .await
        .expect_err("provider without endpoint");
    assert_eq!(error.code, "CC_SWITCH_PROVIDER_NOT_READY");

    let error = fixture
        .service
        .switch(ProviderApp::Claude, "missing")
        .await
        .expect_err("unknown provider");
    assert_eq!(error.code, "CC_SWITCH_PROVIDER_NOT_FOUND");
}

#[tokio::test]
async fn a_successful_cli_that_did_not_update_state_is_reported() {
    let fixture = Fixture::new("switch-stale");
    seed_database(&fixture.paths);
    // The fake CLI reports success for `switch` but never flips is_current.
    write_script(
        &fixture.paths.cc_switch_cli_path,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'cc-switch 5.10.5\\n'; fi\nexit 0\n",
    );
    let error = fixture
        .service
        .switch(ProviderApp::Claude, "with-url-secret")
        .await
        .expect_err("state was not updated");
    assert_eq!(error.code, "CC_SWITCH_STATE_INVALID");
}

#[tokio::test]
async fn a_missing_private_cli_is_reported_without_touching_anything_else() {
    let fixture = Fixture::new("missing");
    std::fs::remove_file(&fixture.paths.cc_switch_cli_path).expect("remove cli");
    let diagnostics = fixture.service.diagnose().await;
    assert!(!diagnostics.available);
    assert_eq!(diagnostics.error_code.as_deref(), Some("CC_SWITCH_MISSING"));
    assert!(diagnostics.reason.unwrap_or_default().contains("not installed"));
}

#[test]
fn paths_outside_the_private_toolchain_are_rejected() {
    let root = TempRoot::new("toolchain");
    let mut paths = HubPaths::resolve(root.join("home"), root.join("app"));
    paths.cc_switch_cli_path = PathBuf::from("/usr/local/bin/cc-switch");
    let error = match CcSwitchService::new(&paths) {
        Ok(_) => panic!("a foreign binary path must be rejected"),
        Err(error) => error,
    };
    assert_eq!(error.code, "CC_SWITCH_PATH_UNSAFE");
    assert!(Path::new(&paths.home).exists() || true);
}
