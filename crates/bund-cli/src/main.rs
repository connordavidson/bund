use anyhow::{Context, Result};
use bund_cli::{
    args::{Cli, Command},
    config, output, ui,
};
use bund_core::{Engine, EngineDependencies, Mode, Provider};
use bund_providers::{AnthropicProvider, OpenAiProvider};
use bund_store::SqliteStore;
use clap::Parser;
use std::{
    io::{self, IsTerminal, Read},
    sync::Arc,
};

#[tokio::main]
async fn main() {
    let code = match execute(Cli::parse()).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("bund: {}", output::clean(&format!("{error:#}")));
            1
        }
    };
    std::process::exit(code);
}

async fn execute(cli: Cli) -> Result<i32> {
    let settings = config::load(&cli.options)?;
    let mut providers: Vec<Arc<dyn Provider>> = Vec::new();
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        if !key.is_empty() {
            providers.push(Arc::new(OpenAiProvider::new(key)));
        }
    }
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        if !key.is_empty() {
            providers.push(Arc::new(AnthropicProvider::new(key)));
        }
    }
    let store = Arc::new(
        SqliteStore::open(settings.state_dir.clone()).with_context(|| {
            format!(
                "cannot open Bund session storage at {}; choose a writable folder with --state-dir PATH or top-level state_dir in your user configuration",
                settings.state_dir.display()
            )
        })?,
    );
    let engine = Engine::new(EngineDependencies {
        providers,
        tools: bund_tools::built_in_tools(),
        store,
    })?;
    match cli.command {
        Some(Command::Sessions { json }) => {
            let sessions = engine.list_sessions().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else if sessions.is_empty() {
                println!("No saved sessions.");
            } else {
                for session in sessions {
                    println!(
                        "{}  {:?}  {}  {} / {}",
                        session.id,
                        session.status,
                        output::clean(&session.workspace.display().to_string()),
                        output::clean(&session.profile.provider),
                        output::clean(&session.profile.model)
                    );
                }
            }
            Ok(0)
        }
        Some(Command::Run {
            prompt,
            output: format,
        }) => {
            config::require_model(&settings.options.profile)?;
            let prompt = read_prompt(prompt)?;
            let handle = engine.create_session(settings.options).await?;
            output::headless(handle, prompt, format).await
        }
        Some(Command::Resume { id }) => {
            anyhow::ensure!(cli.options.allow_command.is_empty() && cli.options.check.is_empty() && cli.options.max_rounds.is_none() && cli.options.timeout.is_none() && cli.options.command_timeout.is_none() && cli.options.output_limit.is_none(), "Resume preserves saved command grants, checks and limits; start a new session to replace them.");
            let handle = engine.open_session(&id).await?;
            if cli.options.model.is_some()
                || cli.options.provider.is_some()
                || cli.options.endpoint.is_some()
            {
                let profile = config::update_profile(
                    &handle.snapshot().await?.profile,
                    cli.options.provider.as_deref(),
                    cli.options.model.as_deref(),
                    cli.options.endpoint.as_deref(),
                );
                handle.set_profile(profile).await?;
            }
            if cli.options.plan {
                handle.set_mode(Mode::Plan).await?;
            }
            if cli.options.accept_edits {
                handle.set_accept_edits(true).await?;
            }
            ui::run(handle, settings.state_dir.join("history.jsonl"), None).await?;
            Ok(0)
        }
        None => {
            let handle = engine.create_session(settings.options).await?;
            ui::run(handle, settings.state_dir.join("history.jsonl"), cli.prompt).await?;
            Ok(0)
        }
    }
}

fn read_prompt(prompt: Option<String>) -> Result<String> {
    let mut input = String::new();
    if !io::stdin().is_terminal() {
        io::stdin()
            .take(1_048_577)
            .read_to_string(&mut input)
            .context("read piped input (UTF-8 required)")?;
        anyhow::ensure!(
            input.len() <= 1_048_576,
            "piped input exceeds 1 MiB; reference a workspace file instead"
        );
    }
    let prompt = match (prompt, input.is_empty()) {
        (Some(prompt), false) => format!("{prompt}\n\nPiped input:\n{input}"),
        (Some(prompt), true) => prompt,
        (None, _) => input,
    };
    anyhow::ensure!(
        !prompt.trim().is_empty(),
        "bund run needs a prompt argument or piped input"
    );
    Ok(prompt)
}
