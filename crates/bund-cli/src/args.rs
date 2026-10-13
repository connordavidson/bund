use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "bund",
    version,
    about = "A terminal coding agent with explicit permissions",
    after_help = "Set OPENAI_API_KEY or ANTHROPIC_API_KEY, and choose a model with --model or BUND_MODEL.\nProject instructions are context; command permissions are enforced separately."
)]
pub struct Cli {
    #[command(flatten)]
    pub options: Options,
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Optional first message for an interactive session.
    pub prompt: Option<String>,
}

#[derive(Debug, Default, Args)]
pub struct Options {
    #[arg(long, global = true)]
    pub provider: Option<String>,
    #[arg(long, global = true)]
    pub model: Option<String>,
    #[arg(long, global = true)]
    pub endpoint: Option<String>,
    #[arg(long, global = true)]
    pub workspace: Option<PathBuf>,
    #[arg(long, global = true)]
    pub state_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    /// Explore only; mutation tools and arbitrary commands are denied.
    #[arg(long, global = true)]
    pub plan: bool,
    /// Accept direct file edits; shell commands still need approval.
    #[arg(long, global = true)]
    pub accept_edits: bool,
    /// Allow this exact shell command. May be repeated; never a prefix match.
    #[arg(long, global = true)]
    pub allow_command: Vec<String>,
    /// Verification command for /check. May be repeated.
    #[arg(long, global = true)]
    pub check: Vec<String>,
    #[arg(long, global = true)]
    pub max_rounds: Option<u32>,
    #[arg(long, global = true)]
    pub timeout: Option<u64>,
    /// Maximum duration of an individual shell command, in seconds.
    #[arg(long, global = true)]
    pub command_timeout: Option<u64>,
    /// Maximum captured tool output, in bytes (1..=1048576).
    #[arg(long, global = true)]
    pub output_limit: Option<usize>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run once without a terminal UI; read piped input if present.
    Run {
        prompt: Option<String>,
        #[arg(long, value_enum, default_value = "text")]
        output: OutputFormat,
    },
    /// List saved sessions without contacting a model.
    Sessions {
        #[arg(long)]
        json: bool,
    },
    /// Resume a saved session with the inline interface.
    Resume { id: String },
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Jsonl,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn globals_work_after_subcommand_and_exact_commands_keep_spaces() {
        let c = Cli::try_parse_from([
            "bund",
            "run",
            "fix tests",
            "--output",
            "jsonl",
            "--model",
            "chosen-model",
            "--allow-command",
            "cargo test",
        ])
        .unwrap();
        assert_eq!(c.options.allow_command, ["cargo test"]);
        assert!(matches!(
            c.command,
            Some(Command::Run {
                output: OutputFormat::Jsonl,
                ..
            })
        ));
    }
    #[test]
    fn interactive_prompt_and_resume_parse() {
        assert_eq!(
            Cli::try_parse_from(["bund", "explain this repo"])
                .unwrap()
                .prompt
                .as_deref(),
            Some("explain this repo")
        );
        assert!(matches!(
            Cli::try_parse_from(["bund", "resume", "abc"])
                .unwrap()
                .command,
            Some(Command::Resume { .. })
        ));
    }
}
