use crate::args::Options;
use anyhow::{Context, Result};
use bund_core::{Mode, ModelCapabilities, ModelProfile, SessionOptions};
use serde::Deserialize;
use std::{
    io::Write,
    path::{Path, PathBuf},
};
use toml_edit::{DocumentMut, Item, Table, TableLike, Value};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    state_dir: Option<PathBuf>,
    #[serde(default)]
    model: ModelConfig,
    #[serde(default)]
    policy: PolicyConfig,
    #[serde(default)]
    limits: LimitsConfig,
    #[serde(default)]
    checks: Vec<String>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelConfig {
    provider: Option<String>,
    model: Option<String>,
    endpoint: Option<String>,
    context_window: Option<usize>,
    max_output_tokens: Option<u32>,
    reasoning: Option<String>,
    capabilities: Option<ModelCapabilities>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyConfig {
    #[serde(default)]
    accept_edits: bool,
    #[serde(default)]
    allowed_commands: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitsConfig {
    max_rounds: Option<u32>,
    run_timeout_secs: Option<u64>,
    command_timeout_secs: Option<u64>,
    output_limit: Option<usize>,
}

pub struct Settings {
    pub options: SessionOptions,
    pub state_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedModel {
    pub path: PathBuf,
    pub provider: String,
    pub model: String,
}

fn config_path(args: &Options) -> Result<PathBuf> {
    if let Some(path) = &args.config {
        return Ok(path.clone());
    }
    let base = if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(path)
    } else {
        dirs::home_dir()
            .context("cannot determine home directory; pass --config PATH")?
            .join(".config")
    };
    Ok(base.join("bund/config.toml"))
}

fn read_saved_config(path: &Path) -> Result<(String, Config)> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot read configuration: {}", path.display()))
        }
    };
    let config = toml::from_str(&text).with_context(|| {
        format!(
            "invalid configuration: {}; file was not changed",
            path.display()
        )
    })?;
    Ok((text, config))
}

/// Inspect saved defaults only; environment overrides and workspace state do not
/// describe what a subsequent terminal will load from this file.
pub fn saved_model(args: &Options) -> Result<SavedModel> {
    let path = config_path(args)?;
    let (_, config) = read_saved_config(&path)?;
    Ok(SavedModel {
        path,
        provider: config.model.provider.unwrap_or_else(|| "openai".into()),
        model: config.model.model.unwrap_or_default(),
    })
}

fn set_string(table: &mut dyn TableLike, key: &str, text: &str) {
    if let Some(value) = table.get_mut(key).and_then(Item::as_value_mut) {
        if value.as_str() == Some(text) {
            return;
        }
        let mut replacement = Value::from(text);
        *replacement.decor_mut() = value.decor().clone();
        *value = replacement;
    } else {
        table.insert(key, toml_edit::value(text));
    }
}

/// Atomically replace a private config file without serializing environment
/// overrides, credentials, or unrelated command-line options into it.
pub fn set_default_model(args: &Options, model: &str) -> Result<SavedModel> {
    anyhow::ensure!(
        !model.trim().is_empty() && model == model.trim() && !model.chars().any(char::is_control),
        "model must be a nonempty name without surrounding whitespace or control characters"
    );
    let path = config_path(args)?;
    let (original, config) = read_saved_config(&path)?;
    let saved_provider = config.model.provider.as_deref().unwrap_or("openai");
    let provider = args.provider.as_deref().unwrap_or(saved_provider);
    anyhow::ensure!(
        ["openai", "anthropic"].contains(&provider),
        "unsupported provider {provider:?}; choose openai or anthropic"
    );
    let mut document: DocumentMut = original.parse().with_context(|| {
        format!(
            "cannot edit configuration: {}; file was not changed",
            path.display()
        )
    })?;
    let table = document
        .as_table_mut()
        .entry("model")
        .or_insert(Item::Table(Table::new()))
        .as_table_like_mut()
        .context("[model] must be a table; file was not changed")?;
    if provider != saved_provider || config.model.model.as_deref() != Some(model) {
        for key in [
            "context_window",
            "max_output_tokens",
            "reasoning",
            "capabilities",
        ] {
            table.remove(key);
        }
    }
    if provider != saved_provider {
        table.remove("endpoint");
    }
    set_string(table, "provider", provider);
    set_string(table, "model", model);
    if let Some(endpoint) = &args.endpoint {
        set_string(table, "endpoint", endpoint);
    }
    let updated = document.to_string();
    // Validate the resulting shape before touching the filesystem, including
    // unusual but valid TOML layouts such as an inline [model] table.
    toml::from_str::<Config>(&updated)
        .context("updated model configuration is invalid; file was not changed")?;
    let destination = match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => path
            .canonicalize()
            .with_context(|| format!("cannot resolve configuration symlink: {}", path.display()))?,
        Ok(_) => path.clone(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => path.clone(),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot inspect configuration: {}", path.display()))
        }
    };
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent).with_context(|| {
        format!(
            "cannot create configuration directory: {}",
            parent.display()
        )
    })?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "cannot create a private configuration file in {}",
            parent.display()
        )
    })?;
    temporary
        .write_all(updated.as_bytes())
        .context("cannot write updated configuration; original file was not changed")?;
    temporary
        .as_file()
        .sync_all()
        .context("cannot sync updated configuration; original file was not changed")?;
    temporary
        .persist(&destination)
        .with_context(|| format!("cannot replace configuration: {}", path.display()))?;
    Ok(SavedModel {
        path,
        provider: provider.into(),
        model: model.into(),
    })
}

pub fn load(args: &Options) -> Result<Settings> {
    let home = dirs::home_dir()
        .context("cannot determine home directory; pass --state-dir and --config")?;
    let config_path = config_path(args)?;
    let config: Config = match std::fs::read_to_string(&config_path) {
        Ok(text) => toml::from_str(&text)
            .with_context(|| format!("invalid configuration: {}", config_path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && args.config.is_none() => {
            Config::default()
        }
        Err(e) => {
            return Err(e)
                .with_context(|| format!("cannot read configuration: {}", config_path.display()))
        }
    };
    let configured_provider = config
        .model
        .provider
        .clone()
        .unwrap_or_else(|| "openai".into());
    let provider = args
        .provider
        .clone()
        .or_else(|| std::env::var("BUND_PROVIDER").ok())
        .unwrap_or_else(|| configured_provider.clone());
    anyhow::ensure!(
        ["openai", "anthropic"].contains(&provider.as_str()),
        "unsupported provider {provider:?}; choose openai or anthropic"
    );
    let same_provider = provider == configured_provider;
    let model = args
        .model
        .clone()
        .or_else(|| std::env::var("BUND_MODEL").ok())
        .or_else(|| {
            same_provider
                .then_some(config.model.model.clone())
                .flatten()
        })
        .unwrap_or_default();
    let same_model = same_provider && config.model.model.as_deref().is_none_or(|id| id == model);
    let mut profile = ModelProfile::new(provider, model);
    profile.endpoint = args
        .endpoint
        .clone()
        .or_else(|| std::env::var("BUND_ENDPOINT").ok())
        .or_else(|| same_provider.then_some(config.model.endpoint).flatten());
    if let Some(n) = config.model.context_window.filter(|_| same_model) {
        anyhow::ensure!(n > 0, "context_window must be positive");
        profile.context_window = n;
    }
    if let Some(n) = config.model.max_output_tokens.filter(|_| same_model) {
        anyhow::ensure!(n > 0, "max_output_tokens must be positive");
        profile.max_output_tokens = n;
    }
    profile.reasoning = same_model.then_some(config.model.reasoning).flatten();
    if let Some(capabilities) = config.model.capabilities.filter(|_| same_model) {
        profile.capabilities = capabilities;
    }
    let workspace = args
        .workspace
        .clone()
        .unwrap_or(std::env::current_dir()?)
        .canonicalize()
        .context("workspace does not exist")?;
    anyhow::ensure!(workspace.is_dir(), "workspace must be a directory");
    let mut options = SessionOptions::new(workspace, profile);
    options.policy.mode = if args.plan { Mode::Plan } else { Mode::Act };
    options.policy.accept_edits = args.accept_edits || config.policy.accept_edits;
    options.policy.allowed_commands = config.policy.allowed_commands;
    options
        .policy
        .allowed_commands
        .extend(args.allow_command.iter().cloned());
    options.checks = config.checks;
    options.checks.extend(args.check.iter().cloned());
    if let Some(n) = args.max_rounds.or(config.limits.max_rounds) {
        anyhow::ensure!(n > 0, "max_rounds must be positive");
        options.limits.max_rounds = n;
    }
    if let Some(n) = args.timeout.or(config.limits.run_timeout_secs) {
        anyhow::ensure!(n > 0, "timeout must be positive");
        options.limits.run_timeout_secs = n;
    }
    if let Some(n) = args.command_timeout.or(config.limits.command_timeout_secs) {
        anyhow::ensure!(n > 0, "command_timeout_secs must be positive");
        options.limits.command_timeout_secs = n;
    }
    if let Some(n) = args.output_limit.or(config.limits.output_limit) {
        anyhow::ensure!(
            (1..=1_048_576).contains(&n),
            "output_limit must be between 1 and 1048576 bytes"
        );
        options.limits.output_limit = n;
    }
    let state_base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"));
    let state_dir = if let Some(path) = &args.state_dir {
        path.clone()
    } else if let Some(path) = config.state_dir {
        anyhow::ensure!(
            path.is_absolute(),
            "state_dir in {} must be an absolute path (without ~)",
            config_path.display()
        );
        path
    } else {
        state_base.join("bund")
    };
    Ok(Settings { options, state_dir })
}

pub fn require_model(profile: &ModelProfile) -> Result<()> {
    anyhow::ensure!(!profile.model.trim().is_empty(), "Save a default with `bund config set-model MODEL --provider openai` (or --provider anthropic), or choose one for this run with --model MODEL or BUND_MODEL. Set OPENAI_API_KEY or ANTHROPIC_API_KEY for its provider.");
    Ok(())
}

/// Provider credentials must never follow an endpoint inherited from another provider.
pub fn update_profile(
    current: &ModelProfile,
    provider: Option<&str>,
    model: Option<&str>,
    endpoint: Option<&str>,
) -> ModelProfile {
    let provider = provider.unwrap_or(&current.provider);
    let mut profile = if provider == current.provider {
        current.clone()
    } else {
        ModelProfile::new(provider, model.unwrap_or(&current.model))
    };
    if let Some(model) = model {
        if model != profile.model {
            let mut replacement = ModelProfile::new(provider, model);
            replacement.endpoint = profile.endpoint;
            profile = replacement;
        }
        profile.model = model.into();
    }
    if let Some(endpoint) = endpoint {
        profile.endpoint = Some(endpoint.into());
    }
    profile
}

#[cfg(test)]
mod tests {
    use super::*;
    fn saved_options(path: PathBuf) -> Options {
        Options {
            config: Some(path),
            workspace: Some(PathBuf::from("/missing-workspace")),
            state_dir: Some(PathBuf::from("/unusable-state")),
            ..Options::default()
        }
    }

    const COMMENTED_CONFIG: &str = "# My settings\nstate_dir = '/chosen/state' # keep location\nchecks = ['cargo test']\n\n[model] # chosen model\nprovider = 'openai' # keep provider comment\nmodel = 'old' # keep model comment\nendpoint = 'https://gateway.example/v1'\ncontext_window = 64000\nmax_output_tokens = 4096\nreasoning = 'high'\n\n[model.capabilities]\ntool_calling = true\nsupported_reasoning = ['high']\n\n[policy] # approval choices\naccept_edits = false\nallowed_commands = ['cargo test']\n\n[limits]\nmax_rounds = 4 # bounded work\n";

    #[test]
    fn saving_same_model_preserves_settings_comments_and_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, COMMENTED_CONFIG).unwrap();
        let args = saved_options(path.clone());
        let saved = set_default_model(&args, "old").unwrap();
        assert_eq!(saved, saved_model(&args).unwrap());
        assert_eq!(saved.provider, "openai");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), COMMENTED_CONFIG);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn changing_saved_model_resets_only_its_specific_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, COMMENTED_CONFIG).unwrap();
        let args = saved_options(path.clone());
        set_default_model(&args, "new").unwrap();
        let updated = std::fs::read_to_string(path).unwrap();
        let config: Config = toml::from_str(&updated).unwrap();
        assert_eq!(config.model.model.as_deref(), Some("new"));
        assert_eq!(
            config.model.endpoint.as_deref(),
            Some("https://gateway.example/v1")
        );
        assert!(config.model.context_window.is_none());
        assert!(config.model.max_output_tokens.is_none());
        assert!(config.model.reasoning.is_none());
        assert!(config.model.capabilities.is_none());
        assert_eq!(config.state_dir, Some(PathBuf::from("/chosen/state")));
        assert_eq!(config.policy.allowed_commands, ["cargo test"]);
        assert_eq!(config.limits.max_rounds, Some(4));
        for comment in [
            "# My settings",
            "# keep location",
            "# chosen model",
            "# keep provider comment",
            "# keep model comment",
            "# approval choices",
            "# bounded work",
        ] {
            assert!(updated.contains(comment), "missing {comment}: {updated}");
        }
    }

    #[test]
    fn saved_provider_change_clears_old_endpoint_but_explicit_endpoint_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, COMMENTED_CONFIG).unwrap();
        let mut args = saved_options(path.clone());
        args.provider = Some("anthropic".into());
        set_default_model(&args, "chosen").unwrap();
        let (_, config) = read_saved_config(&path).unwrap();
        assert_eq!(config.model.provider.as_deref(), Some("anthropic"));
        assert!(config.model.endpoint.is_none());
        args.provider = Some("openai".into());
        args.endpoint = Some("https://new-gateway.example/v1".into());
        set_default_model(&args, "next").unwrap();
        let (_, config) = read_saved_config(&path).unwrap();
        assert_eq!(config.model.endpoint, args.endpoint);
    }

    #[test]
    fn invalid_or_unwritable_config_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let args = saved_options(path.clone());
        for invalid in ["[model\nmodel='old'", "[policy]\nallow_everything=true\n"] {
            std::fs::write(&path, invalid).unwrap();
            assert!(set_default_model(&args, "next").is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
        }
        std::fs::write(&path, COMMENTED_CONFIG).unwrap();
        assert!(set_default_model(&args, "\n").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), COMMENTED_CONFIG);
        let blocked = saved_options(path.join("config.toml"));
        assert!(set_default_model(&blocked, "next").is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), COMMENTED_CONFIG);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "failure must not leave temporary files"
        );
    }

    #[test]
    fn saved_defaults_can_be_created_without_workspace_or_state_access() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new/config.toml");
        let mut args = saved_options(path.clone());
        args.model = Some("transient-command-override".into());
        assert_eq!(saved_model(&args).unwrap().model, "");
        assert!(!path.exists(), "show must not create files");
        set_default_model(&args, "chosen").unwrap();
        assert_eq!(saved_model(&args).unwrap().model, "chosen");
        let (_, config) = read_saved_config(&path).unwrap();
        assert!(config.state_dir.is_none());
        assert!(config.model.endpoint.is_none());
    }

    #[test]
    fn inline_model_table_and_symlink_target_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "model = { provider = 'openai', model = 'old', reasoning = 'high' } # inline comment\n",
        )
        .unwrap();
        let args = saved_options(path.clone());
        set_default_model(&args, "new").unwrap();
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("# inline comment"));
        #[cfg(unix)]
        {
            let link = dir.path().join("linked.toml");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            set_default_model(&saved_options(link.clone()), "via-link").unwrap();
            assert!(std::fs::symlink_metadata(link)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(saved_model(&args).unwrap().model, "via-link");
        }
    }
    #[test]
    fn provider_change_clears_inherited_endpoint_and_reasoning() {
        let mut current = ModelProfile::new("openai", "old-model");
        current.endpoint = Some("https://old.example/v1".into());
        current.reasoning = Some("high".into());
        let changed = update_profile(&current, Some("anthropic"), Some("new-model"), None);
        assert_eq!(changed.provider, "anthropic");
        assert_eq!(changed.model, "new-model");
        assert_eq!(changed.endpoint, None);
        assert_eq!(changed.reasoning, None);
        assert_eq!(
            update_profile(&current, None, Some("other-model"), None).endpoint,
            current.endpoint
        );
        assert_eq!(
            update_profile(
                &current,
                Some("anthropic"),
                Some("new-model"),
                Some("https://new.example/v1")
            )
            .endpoint
            .as_deref(),
            Some("https://new.example/v1")
        );
    }
    #[test]
    fn config_rejects_typo_in_permission_field() {
        assert!(toml::from_str::<Config>("[policy]\nallow_commands=['rm -rf .']").is_err());
    }
    #[test]
    fn model_change_resets_model_specific_settings_but_keeps_provider_endpoint() {
        let mut current = ModelProfile::new("openai", "old");
        current.endpoint = Some("https://gateway.example/v1".into());
        current.context_window = 64_000;
        current.reasoning = Some("high".into());
        current.capabilities.supported_reasoning = Some(vec!["high".into()]);
        let next = update_profile(&current, None, Some("new"), None);
        assert_eq!(next.endpoint, current.endpoint);
        assert_eq!(next.context_window, 128_000);
        assert_eq!(next.reasoning, None);
        assert_eq!(next.capabilities, ModelCapabilities::default());
        assert_eq!(update_profile(&current, None, Some("old"), None), current);
    }
    #[test]
    fn model_capabilities_are_configurable_and_validate_reasoning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[model]\nprovider='openai'\nmodel='fixture'\nreasoning='high'\n[model.capabilities]\ntool_calling=true\nsupported_reasoning=['low']\n").unwrap();
        let args = Options {
            workspace: Some(dir.path().into()),
            config: Some(path),
            ..Options::default()
        };
        let profile = load(&args).unwrap().options.profile;
        assert!(profile.validate_capabilities(true).is_err());
        let mut compatible = profile;
        compatible.reasoning = Some("low".into());
        assert!(compatible.validate_capabilities(true).is_ok());
        compatible.capabilities.tool_calling = false;
        assert!(compatible.validate_capabilities(true).is_err());
        assert!(compatible.validate_capabilities(false).is_ok());
    }
    #[test]
    fn no_model_is_not_silently_replaced() {
        assert!(require_model(&ModelProfile::new("openai", "")).is_err());
    }
    #[test]
    fn configured_state_directory_is_stable_across_workspaces_and_cli_can_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let state = dir.path().join("Application Support/bund");
        std::fs::write(
            &path,
            toml::to_string(&serde_json::json!({ "state_dir": state })).unwrap(),
        )
        .unwrap();
        let mut args = Options {
            workspace: Some(dir.path().into()),
            config: Some(path.clone()),
            ..Options::default()
        };
        assert_eq!(load(&args).unwrap().state_dir, state);
        let other = dir.path().join("another-project");
        std::fs::create_dir(&other).unwrap();
        args.workspace = Some(other);
        assert_eq!(load(&args).unwrap().state_dir, state);
        args.state_dir = Some(dir.path().join("override"));
        assert_eq!(
            load(&args).unwrap().state_dir,
            args.state_dir.clone().unwrap()
        );
        args.state_dir = None;
        std::fs::write(path, "state_dir = '~/bund-state'\n").unwrap();
        assert!(load(&args)
            .err()
            .unwrap()
            .to_string()
            .contains("absolute path"));
    }
    #[test]
    fn cli_limits_override_config_and_zero_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[limits]\nmax_rounds=4\nrun_timeout_secs=20\ncommand_timeout_secs=8\noutput_limit=2048\n").unwrap();
        let mut args = Options {
            workspace: Some(dir.path().to_path_buf()),
            config: Some(path),
            command_timeout: Some(11),
            ..Options::default()
        };
        let settings = load(&args).unwrap();
        assert_eq!(settings.options.limits.max_rounds, 4);
        assert_eq!(settings.options.limits.run_timeout_secs, 20);
        assert_eq!(settings.options.limits.command_timeout_secs, 11);
        assert_eq!(settings.options.limits.output_limit, 2048);
        args.command_timeout = Some(0);
        assert!(load(&args).is_err());
        args.command_timeout = Some(1);
        args.output_limit = Some(1_048_577);
        assert!(load(&args).is_err());
    }
}
