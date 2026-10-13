# Research baseline: October 13, 2026

## Harness engineering

[Birgitta Böckeler's article on MartinFowler.com](https://martinfowler.com/articles/harness-engineering.html)
distinguishes guidance before action from feedback afterward. Bund translates
this into repository instructions and tool contracts, then executable checks,
observable results and human review. Computational controls can enforce an
execution boundary; prompts and semantic model judgments cannot guarantee it.
A deterministic harness means predictable state transitions and verifiable
execution records, not deterministic model answers. Passing tests alone does
not prove that a task's intended behavior was understood.

## Claude Code feature-family mapping

This is a dated inventory of public documented behavior, not copied internals
or a promise of command-for-command compatibility.

| Family | Bund MVP | Future |
| --- | --- | --- |
| [Agent loop](https://code.claude.com/docs/en/how-claude-code-works) | Inspect, act, verify, cancel, bounded runs | Queued steering, background tasks, goals |
| [Tools](https://code.claude.com/docs/en/tools-reference) | Files, search, patch, command, Git inspection | Web, LSP, notebooks, images, browser |
| [Interactive UI](https://code.claude.com/docs/en/interactive-mode) | Inline stream, multiline input, history, approval, diffs | Vim, richer navigation, themes, voice |
| [Permissions](https://code.claude.com/docs/en/permissions) | Plan, reviewed edits, exact command grants | Persistent scoped policies, administration |
| [Sandboxing](https://code.claude.com/docs/en/sandboxing) | Approved host execution explicitly identified | OS/container isolation and network policy |
| [Memory](https://code.claude.com/docs/en/memory) | Scoped instructions and context compaction | Automatic memory, retrieval, lazy discovery |
| [Sessions](https://code.claude.com/docs/en/sessions) | Durable save/list/resume | Fork, search, export, shared sessions |
| [Checkpoints](https://code.claude.com/docs/en/checkpointing) | Direct-edit preimages and guarded undo | Broader checkpoint/review workflows |
| [Skills](https://code.claude.com/docs/en/skills) | User-invoked local Markdown | Discovery and packaging |
| [Hooks](https://code.claude.com/docs/en/hooks-guide) | Configured validation commands | Controlled lifecycle hooks |
| [MCP](https://code.claude.com/docs/en/mcp) | Internal extension traits | stdio, then HTTP/OAuth/resources/prompts |
| [Plugins](https://code.claude.com/docs/en/plugins/overview) | Statically linked extensions | Versioned plugin bundles/distribution |
| [Subagents](https://code.claude.com/docs/en/sub-agents) | Single agent | Read-only workers, then isolated writers |
| [Worktrees](https://code.claude.com/docs/en/worktrees) | One writable workspace owner | Worktree-isolated parallel agents |
| [Headless](https://code.claude.com/docs/en/headless) | Text/JSONL with shared engine | CI integrations, schedules, triggers |
| [Other clients](https://code.claude.com/docs/en/platforms) | Terminal + Rust SDK | Desktop, IDE, service and remote clients |

## API integration findings

- [Anthropic streaming](https://platform.claude.com/docs/en/build-with-claude/streaming)
  requires indexed content-block assembly and terminal-message handling.
  [Tool results](https://platform.claude.com/docs/en/agents-and-tools/tool-use/handle-tool-calls)
  correlate through `tool_use_id` and follow the tool-use message. Preserve
  [thinking signatures](https://platform.claude.com/docs/en/build-with-claude/thinking).
- [OpenAI function calling](https://developers.openai.com/api/docs/guides/function-calling)
  uses `call_id` for returned outputs. Preserve completed output items and
  required [reasoning state](https://developers.openai.com/api/docs/guides/reasoning).
  [Semantic stream events](https://developers.openai.com/api/docs/guides/streaming-responses)
  distinguish deltas, item completion and whole-response completion.
- A portable transcript plus opaque adapter state avoids flattening away
  vendor-specific requirements. Native GPT support is Bund's own capability;
  Claude distribution through cloud providers is not native GPT support.
- Future compatible endpoints need conformance tests. For example,
  [Ollama's compatibility documentation](https://docs.ollama.com/api/openai-compatibility)
  documents limitations on stateful Responses behavior.

## Rust terminal choice

[Ratatui's inline viewport](https://docs.rs/ratatui/latest/ratatui/struct.Terminal.html#method.insert_before)
supports permanent output above a live working region. Use one renderer and
one Crossterm event source. Subprocesses send events rather than writing to the
terminal. Native scrollback and actual cursor behavior require PTY tests;
widget snapshots alone are insufficient.
