use crate::args::Options;
use anyhow::{Context, Result};
use bund_core::{Mode, ModelProfile, SessionOptions};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
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

pub fn load(args: &Options) -> Result<Settings> {
    let home = dirs::home_dir()
        .context("cannot determine home directory; pass --state-dir and --config")?;
    let config_base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let default_config = config_base.join("bund/config.toml");
    let config_path = args.config.as_ref().unwrap_or(&default_config);
    let config: Config = match std::fs::read_to_string(config_path) {
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
        .or_else(|| same_provider.then_some(config.model.model).flatten())
        .unwrap_or_default();
    let mut profile = ModelProfile::new(provider, model);
    profile.endpoint = args
        .endpoint
        .clone()
        .or_else(|| std::env::var("BUND_ENDPOINT").ok())
        .or_else(|| same_provider.then_some(config.model.endpoint).flatten());
    if let Some(n) = config.model.context_window {
        anyhow::ensure!(n > 0, "context_window must be positive");
        profile.context_window = n;
    }
    if let Some(n) = config.model.max_output_tokens {
        anyhow::ensure!(n > 0, "max_output_tokens must be positive");
        profile.max_output_tokens = n;
    }
    profile.reasoning = same_provider.then_some(config.model.reasoning).flatten();
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
    let state_dir = args
        .state_dir
        .clone()
        .unwrap_or_else(|| state_base.join("bund"));
    Ok(Settings { options, state_dir })
}

pub fn require_model(profile: &ModelProfile) -> Result<()> {
    anyhow::ensure!(!profile.model.trim().is_empty(), "Choose a model with --model MODEL, BUND_MODEL, or [model].model in ~/.config/bund/config.toml. Set OPENAI_API_KEY or ANTHROPIC_API_KEY for its provider.");
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
    fn no_model_is_not_silently_replaced() {
        assert!(require_model(&ModelProfile::new("openai", "")).is_err());
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
