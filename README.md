# Bund

A Rust coding agent with a fullscreen terminal interface and a reusable Rust SDK.
Bund reads a repository, proposes focused changes, asks before edits and host
commands, runs configured checks, and saves sessions for later continuation.
Use Anthropic Messages or OpenAI Responses with your own API credentials.

The engine, tools, providers and store are independent of the terminal client.
A future Rust desktop application can use the same session/approval/event API.

Start with the [everyday flows guide](docs/quickstart.md) for saved model defaults,
macOS Keychain login, reviewing work, resuming sessions, and startup fixes.

## Build and run

Requirements: current stable Rust, macOS or Linux, and a model with tool calling
available to your API account. Git tools additionally require Git.

```sh
cargo build --release --locked -p bund-cli
cargo install --path crates/bund-cli --locked
```

Cargo installs `bund` into `~/.cargo/bin` by default. If that directory is not
already on your shell's PATH, add `export PATH="$HOME/.cargo/bin:$PATH"` to your
shell configuration. Confirm the installation with `bund --version`.

Choose your actual API model ID; Bund does not guess which models your account
can access. Supply credentials through your existing environment:

```sh
# OPENAI_API_KEY must already be set in your environment.
bund --provider openai --model YOUR_MODEL_ID --workspace /path/to/repository

# Or, with ANTHROPIC_API_KEY set:
bund --provider anthropic --model YOUR_MODEL_ID --workspace /path/to/repository
```

Ask for a small change, such as “Find the date parser, add a regression test for
empty input, and fix it.” Bund streams progress, previews patches and requests
approval for host commands. **Ctrl+Y approves once; Ctrl+N or Esc denies.**

API credentials and models are unnecessary for help, listing sessions, or local
inspection. `bund` can open without credentials; configure a model before
submitting a prompt. Subscription access to another coding product is not an
API credential for Bund.

Once your model is saved and its API key is available through the environment
or macOS Keychain, start Bund in any project with:

```sh
cd /path/to/project
bund
```

The current directory becomes the workspace; no launcher script is required.

## Terminal workflow

The fullscreen interface shows workspace, model, run status, and keyboard
controls at the top, a scrollable conversation in the middle, and a cyan
bordered query box fixed at the bottom. The query box keeps your draft visible
while work runs and approvals are pending. Enter submits, Alt+Enter or Ctrl+J
inserts a newline, Tab completes slash commands and `@path` references, and
Up/Down recalls history on a single-line draft. An `@path` identifies a file for
the agent to read; it does not silently attach file contents.

Routine activity is collapsed by default. **Ctrl+O** expands or collapses
orange activity boxes beneath the corresponding responses, including tools
and any reasoning summary supplied by the provider. A model may provide no
reasoning summary. **PageUp/PageDown** scroll the conversation and pause
following new output; **Ctrl+End** returns to the latest output. Expanding
activity preserves your place in the conversation and your draft.

Typing while a run is active preserves a draft. A busy submission keeps that
draft; queued steering is future work. Esc/Ctrl+C cancels active work and its
command process group. At idle, Ctrl+C clears a draft, then exits on an empty
draft. Ctrl+D on an empty composer or `/exit` also exits.

| Command | Behavior |
| --- | --- |
| `/help` | Display keyboard controls and commands |
| `/plan`, `/act` | Switch inspection-only or action mode while idle |
| `/model [provider] MODEL` | Explicitly change provider/model between runs |
| `/diff` | Inspect the current Git diff |
| `/undo` | Review and restore the latest direct edit if still unchanged |
| `/compact` | Summarize completed context, retaining original user constraints |
| `/check` | Run configured validation commands through normal permissions |
| `/skill PATH` | Invoke a workspace Markdown skill; directories use SKILL.md |
| `/accept-edits on\|off` | Control direct-edit approvals for this session |
| `/exit` | Cancel active work, restore the terminal, and exit |

Approval controls remain visible at the top. While an approval is pending,
PageUp/PageDown scroll its preview in the middle of the screen; Ctrl+O still
toggles activity. Exiting restores your previous terminal screen. Use
`bund sessions` and `bund resume SESSION_ID` to revisit saved conversations.

Project `AGENTS.md`
guidance loads by directory scope; `CLAUDE.md` is the fallback at a directory
without `AGENTS.md`. Instructions and skills never grant execution permission.

```sh
bund --plan --model YOUR_MODEL_ID
bund --model YOUR_MODEL_ID --check 'cargo test' --check 'cargo clippy'
bund sessions
bund resume SESSION_ID
```

Resume preserves the session's checks, grants and limits. Start a new session to
replace those settings; conflicting resume flags are rejected. Explicit model,
provider, Plan-mode and accept-edits changes are available on resume.

## Headless use

```sh
bund run 'Explain the repository layout' --plan --model YOUR_MODEL_ID
cat task.txt | bund run --model YOUR_MODEL_ID --output jsonl
bund run 'Fix the regression' --model YOUR_MODEL_ID \
  --accept-edits --check 'cargo test' --allow-command 'cargo test' --output jsonl
```

JSONL contains versioned, ordered session events. Text progress goes to the
terminal without control sequences. Headless mode never waits for an invisible
approval prompt: it records `approval_required`, cancels the pending run and
returns exit code **3**. Success is **0**, failure **1**, cancellation **130**.

`--allow-command` matches the entire command string and applies only at the
workspace root. It is not a prefix rule. Other commands still need approval.
`--accept-edits` authorizes direct file edits, not host commands.

## Configuration

Save your default model directly from the CLI:

```sh
bund config set-model gpt-5-nano --provider openai
bund config show
bund auth login --provider openai # macOS: hidden prompt, saved in Keychain
bund
```

`config set-model` saves your user configuration for future sessions. Omit
`--provider` to keep the saved provider (OpenAI if none is saved). It works
without an API key or a session database and preserves unrelated settings and
comments. `--config PATH` selects another configuration file. Re-selecting the
same provider/model preserves its settings; changing models clears model-specific
budgets, reasoning and capabilities, and changing providers also clears the old
endpoint. An explicit `--endpoint` saves a new endpoint. `/model` in a running
conversation changes only that session.

On macOS, `bund auth login --provider openai` prompts for your key without
echoing it and stores it as a generic password in macOS Keychain (service
`com.connordavidson.bund`, account `openai` or `anthropic`). The key is never
written to the TOML configuration. Use `bund auth status --provider openai` to
show its source or `bund auth logout --provider openai` to remove the saved key.
Omitting `--provider` uses the saved default provider. These commands work
without opening a workspace or a session database.

Nonempty `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` environment variables take
precedence over saved keys. Otherwise Bund loads the selected provider's key
from Keychain on its first request and keeps it in memory for that process.
Restart Bund after changing a key already in use. `--no-keychain` restricts
agent requests to environment credentials, which is useful for CI. Linux uses
environment credentials; persistent Keychain login is macOS-only. No credentials
are needed for configuration, saved-session listing, or opening the idle UI.

User settings live in `~/.config/bund/config.toml` (or `$XDG_CONFIG_HOME`). Use
`--config` for an explicit file. CLI flags override `BUND_PROVIDER`, `BUND_MODEL`
and `BUND_ENDPOINT`, which override model configuration. Project files cannot
silently choose endpoints, credentials or grants.

```toml
# Top-level values must precede the [table] sections.
checks = ["cargo test"]

[model]
provider = "openai" # or "anthropic"
model = "YOUR_MODEL_ID"
context_window = 128000
max_output_tokens = 8192
# endpoint = "https://your-approved-gateway.example/v1"
# reasoning = "medium" # optional; support depends on provider/model

[model.capabilities]
tool_calling = true
# supported_reasoning = ["low", "medium", "high"] # declare actual model support

[policy]
accept_edits = false
allowed_commands = []

[limits]
max_rounds = 30
run_timeout_secs = 900
command_timeout_secs = 120
output_limit = 65536
```

Optional endpoints are API **base URLs including `/v1`**. Only configure a
trusted endpoint: it receives the provider credential and task context. Bund
does not automatically fail over to another provider. Explicit profile changes
retain portable history and discard foreign opaque continuation state.

Capabilities are explicit user declarations, not a bundled model catalog.
Omitting `supported_reasoning` leaves support to provider validation; an empty
list disables reasoning settings. Declared incompatibilities fail before an API
request. Changing models resets model-specific limits, reasoning and capabilities
to defaults; an endpoint is retained only within the same provider.

Limit flags: `--max-rounds`, `--timeout`, `--command-timeout`, `--output-limit`.
All must be positive; model-facing output is capped at 1 MiB. A private command
artifact retains at most 8 MiB. The default model-facing limit is 64 KiB.
Context estimates conservatively use serialized byte sizes, not exact billing
tokens. Set profile limits appropriate to the selected model.

## Permissions, persistence and recovery

Plan mode has no general shell or mutation tools. Act mode defaults to reviewed
edits and approved commands. File tools reject symlink traversal and operate
within the workspace. Patches use complete UTF-8 file replacements with a
reviewable diff, checked base hashes and atomic per-file writes. Large/binary
files and files above the direct-edit size limit need an explicitly approved
host command.

**Approved host commands are not sandboxed.** They run as your user and can
access host files and networks. Only conventional build paths and locale are
inherited; credential environment variables are omitted by default. Commands
are noninteractive; stdin is closed and background descendants are stopped.
OS/container isolation is on the roadmap.

Sessions are stored in an owner-only SQLite directory at
`~/.local/state/bund` (or `$XDG_STATE_HOME/bund`). Override with `--state-dir`,
or save a top-level `state_dir = "/absolute/path/to/bund-state"` before any
`[section]` in your user configuration. Use an absolute path without `~` so
sessions stay in the same place when you switch projects. The command-line
option takes precedence. If the default folder is not writable, choose a
folder your account owns, such as `/Users/yourname/Library/Application Support/bund`
on macOS.
Transcripts, instructions, code and command artifacts may contain sensitive
project data. Credentials are not intentionally recorded. Model/tool context
is sent to the explicitly selected API endpoint. There is no separate telemetry.

Events and snapshots commit together. Pre-compaction contexts, including native
continuation state, are archived locally. A crash can leave a tool outcome
unknown; resume closes that incomplete call and instructs the agent to inspect
actual state, never automatically replay it. A journal failure stops execution
and surfaces a failure diagnostic; that diagnostic itself may not be durable.

Undo covers only Bund's direct edits. It refuses a file changed afterward by
you or another process; it does not revert shell commands, commits or external
effects. Validation is shown as passed, failed or not run. Additional edits
invalidate prior check results.

## Rust SDK and development

The five workspace crates are `bund-core`, `bund-providers`, `bund-tools`,
`bund-store`, and `bund-cli`. See [architecture](docs/architecture.md) for the
public session API and extension contracts.

Run the offline SDK demonstration without a CLI or API key:

```sh
cargo run --locked -p bund-store --example sdk
```

It creates a temporary repository, reads a file, approves an edit and check,
reopens the saved session, cancels a running command, and asserts the result.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

Regular tests use local protocol fixtures, real temporary files, supervised
processes and PTYs. They never call a live model. Optional, billable API smoke
tests are explicitly ignored:

```sh
# Set the corresponding API key and actual model first.
BUND_LIVE_OPENAI_MODEL=YOUR_MODEL_ID \
  cargo test -p bund-providers --test live openai_live_smoke -- --ignored
BUND_LIVE_ANTHROPIC_MODEL=YOUR_MODEL_ID \
  cargo test -p bund-providers --test live anthropic_live_smoke -- --ignored
```

CI checks macOS/Linux and builds binaries. The release workflow packages
macOS Apple Silicon and Linux x86-64 archives with checksums as downloadable
workflow artifacts; it does not automatically publish a GitHub release.

## Scope and roadmap

This MVP includes one agent, both native API adapters, reviewed workspace tools,
durable sessions, configured checks, simple skills, and the embeddable engine.
Its fullscreen transcript uses basic styling rather than a rich Markdown viewer.
MCP, general hooks, sandbox backends, subagents, desktop/IDE clients, schedules,
plugins and additional model providers are future work.

See [research](docs/research.md), [implementation plan and release gates](docs/roadmap.md),
and [tool contracts](crates/bund-tools/README.md). The earlier deterministic-tool
branches are preserved; this branch starts a separate coding-agent design from
`master`.
