# Implementation plan and release gates

Accepted baseline: October 13, 2026. Build from `master`; preserve the earlier
deterministic-capability branches. Target macOS/Linux, native Rust, fullscreen
terminal interaction, approved host commands, Anthropic/OpenAI, and an SDK for
a future Rust desktop client.

## Delivery sequence

1. Record research and architecture; create the five-crate workspace, public
   contracts, fake provider fixtures and macOS/Linux CI.
2. Prove the shared engine headlessly: permissions, prepare/revalidation,
   durable sessions, cancellation, workspace locks and coding tools.
3. Implement native Responses and Messages adapters with streaming fixtures,
   errors, usage, bounded retries, continuation and context handling.
4. Add fullscreen composition, expandable activity, session management,
   approval/diff controls, simple manually invoked skills and configured checks.
5. Harden with process/PTY/recovery tests, independent SDK use, installation
   documentation and packaged binaries. Live evaluations are opt-in.

## MVP release gate

- Both native adapters complete a fixture-driven inspect/edit/test workflow.
- Denied changes and commands have no side effects; Plan mode cannot mutate.
- Stale approvals and stale undo requests fail safely.
- Malformed or truncated streams never execute tools.
- Cancelled commands clean up their process groups.
- Reopening interrupted sessions never repeats a side effect automatically.
- Compaction preserves user constraints and complete tool-result sequences.
- Large output and slow subscribers remain bounded and resynchronizable.
- Terminal draft, paste, Unicode, resize, interrupt and cleanup behavior pass
  PTY tests; exit restores the original terminal screen, and sessions retain
  conversation history for resume.
- An independent Rust example drives the engine without the CLI.
- Formatting, linting and tests pass on macOS/Linux.

Deterministic fixtures assert expected file contents and exit outcomes defined
independently of the model. Live evaluations must record model IDs and bounded
budgets; fixture success is not evidence of live model task quality.

## Next release

MCP stdio; controlled lifecycle hooks; provider caching; OS/container command
isolation; session fork/search/export; read-only subagents; Gemini and explicitly
tested compatible endpoints. Introduce one capability at a time behind the
existing approval, event and recovery paths.

## Later

Worktree-isolated parallel writers; Rust desktop app; local daemon/remote
transport; IDE clients; durable schedules and monitors; plugins and distribution;
organization policy/telemetry; native Windows. Do not promise universal provider
compatibility or a stable native dynamic plugin ABI.
