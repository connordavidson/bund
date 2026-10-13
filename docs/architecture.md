# Architecture and extension contract

Bund owns its agent loop. Vendor SDKs or other coding agents do not execute
workspace tools on its behalf. The initial client is an inline terminal, but
authorization, state transitions, persistence, and provider access are reusable
Rust library behavior.

```text
bund-cli (inline UI / headless)
          |
          v
bund-core (Engine, SessionHandle, commands, snapshots, events)
          ^                 ^                  ^
bund-providers          bund-tools          bund-store
native HTTP/SSE         files/processes     SQLite/OS locks
```

The concrete crates implement core traits. Core has no dependency on terminal,
HTTP, or SQLite types. A desktop client composes the same crates; it does not
spawn or scrape the CLI. The executable SDK example demonstrates this boundary.

## Public SDK

Construct `EngineDependencies` with providers, tools and a `SessionStore`, then
`Engine::new`. Use `create_session` or `open_session` to obtain a cloneable
`SessionHandle`. `submit`, `run_tool`, `run_checks`, `undo`, and `compact` return
a run ID promptly; results arrive through events. Only one run is active per
session. A second submission returns `Busy` rather than losing a draft.

`attach` atomically captures a snapshot, event cursor, and live subscription.
Tokio broadcast `Lagged` is a resynchronization signal: recover using
`events_after` or reattach. Snapshots include pending approvals. Public snapshots
exclude opaque provider continuation bytes. Clients submit `answer_approval`
or `cancel`; they never implement their own authorization decisions inside
the execution path.

Use `Provider` for a statically linked vendor adapter, `Tool` for a local tool,
`CommandExecutor` for another execution environment, and `SessionStore` for
another persistence implementation. Extensions are trusted Rust code. These
traits are not a stable dynamic-library ABI. Future runtime extensions should
use MCP/subprocesses or a deliberately designed WASM interface.

The Rust API is versioned as 0.x. Serialized session/event versions and SQLite
migrations are separate contracts. A daemon is deliberately deferred: stable
IDs, cursors, snapshots, commands and owned records provide its future boundary.

## Execution and durability

A session serializes state transitions and has one active run. Read, prepare,
approval, revalidation, start, execution and result are separate stages.
Tools execute only after a complete provider response has passed validation.
Approvals bind to normalized operations and observed preconditions. All writes
and commands are checked again at dispatch, including calls invented by a model
that were not advertised in Plan mode.

SQLite uses WAL and full synchronous commits. Snapshot changes and ordered
events commit together before publication; a started event commits before any
side effect. Cross-process workspace locks are independent of the chosen state
directory. Resuming an interrupted operation closes incomplete protocol calls
with an explicit unknown outcome and never automatically reexecutes them.

A journal error stops the run and waits for command cleanup. If even the final
failure event cannot be saved, subscribers receive an explicitly non-durable
failure diagnostic; the on-disk state remains available for crash recovery.

The crash window between starting a tool and recording its completion prevents
an exactly-once claim. Reconciliation requires inspecting actual workspace
state. Direct edits use per-file atomic replacement and recorded preimages;
undo requires the current file to match the recorded postimage. Commands may
modify arbitrary host resources and are outside direct-edit undo coverage.

## Provider boundary

Anthropic Messages and OpenAI Responses have independent encoders and parsers.
The portable transcript retains text, calls and results. Adapter-owned state
preserves signed/encrypted continuation items under provider/endpoint/model
identity. Switching profiles is explicit and drops foreign continuation bytes.
There is no automatic provider failover. OpenAI requests use `store:false`.
Model profiles declare context/output budgets and tool/reasoning capabilities.
These are user-supplied declarations; unknown model support remains subject to
API validation. Explicit incompatibilities fail before sending a request.

Streaming progress is presentation-only. EOF, incomplete arguments and errors
cannot authorize tools. Protocol fixtures exercise both adapters without API
spend. Local usage records are accounting metadata, not billing guarantees.

Context budgeting conservatively estimates serialized UTF-8 bytes rather than
claiming exact tokenization. Output reservations and a safety margin reduce
available input. Compaction summarizes only complete call/result sequences and
retains user constraints. If instructions/current work cannot fit, the engine
stops instead of silently dropping required context.

## Terminal boundary

One Crossterm event source and Ratatui inline renderer own the terminal. No
alternate screen is entered. Complete output enters native scrollback; only
the working region is redrawn. The composer survives streaming and approval
requests. Model and process output is sanitized before display. Headless output
uses the same engine without terminal initialization.

## Trust and limits

Plan mode allows inspection tools only. Act mode asks for direct edits and exact
host commands unless the human supplied a matching grant. Repository instructions
and skills supply guidance, not execution grants. Provider credentials are not
passed to child processes by default. Approval is authorization, not a sandbox.

Default limits: 30 model rounds, 15 minutes per run, 120 seconds per command,
64 KiB of command output in model context. Process groups are supervised on
macOS/Linux so timeout and cancellation clean up descendants. Private artifacts
are bounded, too. Native Windows and OS sandbox backends are future work.
