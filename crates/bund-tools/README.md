# Workspace tools

`built_in_tools()` returns `read_file`, `list_files`, `search`, `apply_patch`,
`run_command`, `git_status` and `git_diff`. The permission/approval boundary lives
in `bund-core`. Calling a low-level tool directly requires host authorization.

`apply_patch` accepts `{ "path": "relative/file", "content": "entire new file" }`.
It does not accept unified-diff input. It generates a unified diff for review,
binds its fingerprint to the old bytes and normalized arguments, checks them
again before writing, and atomically replaces the file. Parents must already
exist. New content is limited to 256 KiB and existing files to 2 MiB. Binary
files, traversal, symlink components, special files and writes inside `.git`
are refused. File permissions are retained. New files use mode 0644.

File tools operate within the canonical workspace. Listing/search skip
`.git`, `target`, `node_modules` and symlinks. Listing stops at 5000 files;
search stops after 16 MiB. Search uses Rust regular expressions. These are
explicit MVP limits, not a full `.gitignore`/ripgrep implementation.

`ApplyPatch::undo_tool` creates a host-only write operation from an `EditRecord`.
It is absent from model tool definitions. The host reviews/approves its reverse
diff with the same lifecycle as an ordinary edit. Restoration refuses files
whose current hash differs from Bund's recorded post-edit hash. Undoing a new
file removes it. Direct `guarded_undo` is also available to authorized SDK hosts.

Host commands run in a process group through `/bin/sh -c` with closed stdin.
Only PATH, HOME, TMPDIR, USER, LOGNAME, LANG, LC_ALL, CARGO_HOME and RUSTUP_HOME
are inherited. API tokens, agent sockets and shell startup variables are not.
Commands still run as the user's OS identity and can read host credentials and
other files themselves: this is **approved host execution, not a sandbox**.
Timeout/cancellation terminates the group with TERM then KILL; descendants are
also stopped when the initial process exits. Processes that deliberately escape
their process group require a future sandbox boundary.

Combined stdout/stderr defaults to 64 KiB for model output and live display.
The configured output limit applies to command output, live output and file
inspection; hosts may raise it to the absolute maximum of 1 MiB.
An owner-only local artifact retains up to 8 MiB. Bounded live output is best
effort: slow subscribers do not block execution, and omissions are identified
in the final result. The result always identifies the artifact and exit status.
Read-only Git operations use `/usr/bin/git`, disable optional index writes,
global/system configuration, hooks, fsmonitor, external diff and textconv.
Repositories that configure executable clean/process filters are refused by
read-only Git tools; an explicitly approved host command is required instead.
