//! Offline configuration commands must work before providers, workspace and
//! session storage are initialized. Every subprocess uses isolated user paths.
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    root: tempfile::TempDir,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("home")).unwrap();
        // Any attempt to initialize a session store under this path must fail.
        fs::write(root.path().join("unusable-state"), "ordinary file").unwrap();
        let config = root.path().join("nested/settings/config.toml");
        Self { root, config }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bund"));
        command
            .env_clear()
            .env("HOME", self.root.path().join("home"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config-home"))
            .env("XDG_STATE_HOME", self.root.path().join("unusable-state"))
            .current_dir(self.root.path())
            .arg("--no-keychain")
            .arg("--workspace")
            .arg(self.root.path().join("workspace-does-not-exist"))
            .arg("--state-dir")
            .arg(self.root.path().join("unusable-state/sessions"));
        command
    }

    fn with_config(&self) -> Command {
        let mut command = self.command();
        command.arg("--config").arg(&self.config);
        command
    }

    fn write(&self, text: &str) {
        fs::create_dir_all(self.config.parent().unwrap()).unwrap();
        fs::write(&self.config, text).unwrap();
    }

    fn saved(&self) -> toml::Value {
        read_toml(&self.config)
    }
}

fn read_toml(path: &Path) -> toml::Value {
    toml::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn succeed(command: &mut Command) -> Output {
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    result
}

const EXISTING: &str = r#"# Personal settings: keep this comment.
state_dir = "/saved/bund-state"
checks = ["cargo test", "cargo clippy"] # My checks stay intact.

[model]
provider = "anthropic"
model = "fixture-model"
endpoint = "https://gateway.example/v1"
context_window = 200000
max_output_tokens = 12000
reasoning = "high"

[model.capabilities]
tool_calling = true
supported_reasoning = ["high"]

[policy]
accept_edits = false
allowed_commands = ["cargo test"]

[limits]
max_rounds = 7
run_timeout_secs = 300
command_timeout_secs = 45
output_limit = 32768
"#;

#[test]
fn set_model_creates_explicit_config_without_runtime_prerequisites() {
    let fixture = Fixture::new();
    assert!(!fixture.config.exists());
    succeed(fixture.with_config().args([
        "config",
        "set-model",
        "gpt-5-nano",
        "--provider",
        "openai",
    ]));
    let saved = fixture.saved();
    assert_eq!(saved["model"]["provider"].as_str(), Some("openai"));
    assert_eq!(saved["model"]["model"].as_str(), Some("gpt-5-nano"));
    assert!(!fixture
        .root
        .path()
        .join("workspace-does-not-exist")
        .exists());
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("unusable-state")).unwrap(),
        "ordinary file"
    );
}

#[test]
fn default_config_location_is_isolated_and_environment_is_not_persisted() {
    let fixture = Fixture::new();
    let mut command = fixture.command();
    command
        .env("BUND_PROVIDER", "anthropic")
        .env("BUND_MODEL", "environment-model")
        .env("BUND_ENDPOINT", "https://environment.example/v1")
        .args(["config", "set-model", "gpt-5-nano"]);
    succeed(&mut command);
    let saved = read_toml(&fixture.root.path().join("config-home/bund/config.toml"));
    assert_eq!(saved["model"]["provider"].as_str(), Some("openai"));
    assert_eq!(saved["model"]["model"].as_str(), Some("gpt-5-nano"));
    assert!(saved["model"].get("endpoint").is_none());
    assert!(!fixture.config.exists());
}

#[test]
fn same_model_preserves_settings_comments_and_accepts_config_after_subcommands() {
    let fixture = Fixture::new();
    fixture.write(EXISTING);
    let original = fixture.saved();
    let mut command = fixture.command();
    command
        .args(["config", "set-model", "fixture-model", "--config"])
        .arg(&fixture.config)
        .env("BUND_PROVIDER", "openai")
        .env("BUND_MODEL", "do-not-save")
        .env("BUND_ENDPOINT", "https://do-not-save.example/v1");
    succeed(&mut command);
    assert_eq!(fixture.saved(), original);
    let text = fs::read_to_string(&fixture.config).unwrap();
    assert!(text.contains("# Personal settings: keep this comment."));
    assert!(text.contains("# My checks stay intact."));
}

#[test]
fn changing_provider_clears_inherited_model_settings_and_accepts_explicit_endpoint() {
    let fixture = Fixture::new();
    fixture.write(EXISTING);
    let original = fixture.saved();
    succeed(fixture.with_config().args([
        "config",
        "set-model",
        "gpt-5-nano",
        "--provider",
        "openai",
    ]));
    let saved = fixture.saved();
    let model = &saved["model"];
    assert_eq!(model["provider"].as_str(), Some("openai"));
    assert_eq!(model["model"].as_str(), Some("gpt-5-nano"));
    for key in [
        "endpoint",
        "context_window",
        "max_output_tokens",
        "reasoning",
        "capabilities",
    ] {
        assert!(model.get(key).is_none(), "inherited {key} was retained");
    }
    for key in ["state_dir", "checks", "policy", "limits"] {
        assert_eq!(saved[key], original[key], "unrelated {key} changed");
    }
    let mut command = fixture.with_config();
    command
        .args([
            "config",
            "set-model",
            "gpt-5-nano",
            "--endpoint",
            "https://explicit.example/v1",
        ])
        .env("BUND_ENDPOINT", "https://ignored-environment.example/v1");
    succeed(&mut command);
    assert_eq!(
        fixture.saved()["model"]["endpoint"].as_str(),
        Some("https://explicit.example/v1")
    );
}

#[test]
fn show_reports_saved_configuration_and_path_without_credentials_or_mutation() {
    let fixture = Fixture::new();
    fixture.write(EXISTING);
    let before = fs::read(&fixture.config).unwrap();
    let mut command = fixture.command();
    command
        .args(["config", "show", "--config"])
        .arg(&fixture.config)
        .env("BUND_PROVIDER", "openai")
        .env("BUND_MODEL", "environment-model")
        .env("OPENAI_API_KEY", "fake-key-that-must-not-be-displayed")
        .env("ANTHROPIC_API_KEY", "another-fake-secret");
    let result = succeed(&mut command);
    let stdout = String::from_utf8(result.stdout).unwrap();
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(stdout.contains("anthropic"));
    assert!(stdout.contains("fixture-model"));
    assert!(stdout.contains(fixture.config.to_str().unwrap()));
    for value in [
        "fake-key-that-must-not-be-displayed",
        "another-fake-secret",
        "environment-model",
    ] {
        assert!(!stdout.contains(value));
        assert!(!stderr.contains(value));
    }
    assert_eq!(fs::read(&fixture.config).unwrap(), before);
}

#[test]
fn invalid_inputs_and_malformed_existing_files_are_never_overwritten() {
    let fixture = Fixture::new();
    fixture.write(EXISTING);
    let before = fs::read(&fixture.config).unwrap();
    let cases: &[&[&str]] = &[
        &["config", "set-model", "gpt-5-nano", "--provider", "invalid"],
        &["config", "set-model", ""],
        &["config", "set-model", "   "],
    ];
    for args in cases {
        let output = fixture
            .with_config()
            .args(args.iter().map(OsStr::new))
            .output()
            .unwrap();
        assert!(!output.status.success(), "invalid args accepted: {args:?}");
        assert_eq!(fs::read(&fixture.config).unwrap(), before);
    }
    let malformed = "# Do not replace this file.\n[model]\nmodel = [unterminated\n";
    fixture.write(malformed);
    let output = fixture
        .with_config()
        .args(["config", "set-model", "gpt-5-nano", "--provider", "openai"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(&fixture.config).unwrap(), malformed);
}
