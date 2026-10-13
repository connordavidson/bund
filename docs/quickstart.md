# Everyday Bund flows

Install Bund using the [build instructions](../README.md#build-and-run), then
check that `bund --version` works in your terminal.

## Set up GPT on macOS

Save a default model and enter your API key. Run these commands in your shell,
outside Bund's conversation screen:

```sh
bund config set-model gpt-5-nano --provider openai
bund auth login --provider openai
```

Paste the key at the hidden prompt and press Enter. The model is saved in your
user configuration; the key is saved in macOS Keychain. Use a model available
to your API account; `gpt-5-nano` is the example here.

Check the saved settings and credential source:

```sh
bund config show
bund auth status --provider openai
```

Status reports `environment`, `macOS Keychain`, or `not configured`, without
printing the key. It does not test whether the API accepts the key.

Then open the project you want Bund to work on:

```sh
cd /path/to/your/project
bund
```

Future terminals can use the saved model and Keychain key. On Linux, supply
`OPENAI_API_KEY` through your environment before launching Bund; Keychain login
is macOS-only. Anthropic uses `--provider anthropic` and `ANTHROPIC_API_KEY`.

## Change models

| Scope | Command | Where to run it |
| --- | --- | --- |
| Default for new sessions | `bund config set-model MODEL_ID --provider openai` | Shell |
| One new session | `bund --provider openai --model MODEL_ID` | Shell |
| Current session, while idle | `/model openai MODEL_ID` | Bund composer |

Replace `MODEL_ID` with the desired model. Existing saved sessions retain their
model. Re-selecting the same default preserves its settings; changing models
resets model-specific budgets, reasoning and capabilities. Switching providers
also clears the old endpoint. Other settings, including the session folder,
remain intact.

Command-line flags override `BUND_PROVIDER`, `BUND_MODEL` and `BUND_ENDPOINT`,
which override saved defaults. `bund config show` displays saved values.

## Replace or remove a saved key

Run `bund auth login --provider openai` again to replace the Keychain key.
Restart Bund after changing a key it has already used.

An exported `OPENAI_API_KEY` takes precedence over Keychain. To use the saved key:

```sh
unset OPENAI_API_KEY
bund
```

To remove the saved key, run `bund auth logout --provider openai`. This leaves
environment variables unchanged. `bund --no-keychain` uses only environment
credentials for agent requests, which is useful for scripts and CI.

## Make a change, review it, and resume later

For a Rust project, start with a validation command:

```sh
bund --check 'cargo test'
```

Ask for a small change. The fullscreen interface keeps workspace and status
information at the top and a cyan query box at the bottom. Enter submits;
Ctrl+J adds a newline. Your draft remains visible during streaming and
approvals. Ctrl+Y approves the displayed action once; Ctrl+N or Esc denies it.
Ctrl+C cancels active work. Completed edits remain after cancellation.

Ctrl+O expands orange activity boxes beneath responses, including tools and
provider-supplied reasoning summaries; press it again to collapse them.
PageUp/PageDown scroll the conversation without following new output, and
Ctrl+End returns to the latest output. During approvals, PageUp/PageDown scroll
the review preview while the decision controls remain visible at the top.

When idle, use `/diff` to review changes, `/check` to run configured checks, and
`/exit` to leave. Checks use the same command approval flow. Exiting restores
your previous terminal screen; reopen saved conversations with:

```sh
bund sessions
bund resume SESSION_ID
```

Replace `SESSION_ID` with an ID from the list. Resume restores the session's
workspace, model, checks and permissions; it does not pick up a newly saved
default model. To choose another model explicitly:

```sh
bund resume SESSION_ID --provider openai --model MODEL_ID
```

## Fix common startup problems

| Symptom | What to do |
| --- | --- |
| `bund: command not found` | Install from the repository with `cargo install --path crates/bund-cli --locked`. Ensure `~/.cargo/bin` is on your PATH, then reopen the terminal or run `rehash` in zsh. |
| API key is not configured | Exit Bund, run `bund auth login --provider openai` on macOS, then reopen Bund. For environment credentials, export the key before starting Bund. |
| Newly saved key is not being used | Check `bund auth status --provider openai`. Unset an unwanted environment override, then restart Bund. |
| Keychain cannot be read | Unlock your login Keychain and allow Bund access when macOS prompts, or use an environment credential. |
| Permission denied opening session storage | Choose a writable folder as shown below. |

For example, on macOS:

```sh
bund --state-dir "$HOME/Library/Application Support/bund"
```

To remember that folder, add the following **before any `[section]`** in the
configuration file shown by `bund config show`. Replace `YOUR_USER` with your
macOS username; the saved path must be absolute and cannot contain `~`.

```toml
state_dir = "/Users/YOUR_USER/Library/Application Support/bund"
```

Changing this folder does not move existing sessions. Keep using the same
folder for `bund sessions` and `bund resume`, or save it in configuration.

See the [README](../README.md) for headless execution, all slash commands,
permission details, and the Rust SDK.
