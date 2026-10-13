# Embedded SDK example

Run `cargo run -p bund-store --example sdk` from the repository root.

The example uses a deterministic provider fixture, real workspace tools and
SQLite in temporary directories. It reads a file, requests an approved edit,
runs an approved validation command, resumes the persisted session with a new
engine, and cancels a running host command. It asserts the edited file and final
states. No API keys, network access or `bund-cli` dependency are needed.

The event loop demonstrates snapshot-plus-cursor attachment and durable event
replay after a slow subscriber falls behind. Its approval policy recognizes
only this example's exact fixture operations; an interactive SDK host should
present approval previews and forward the user's decision instead.
