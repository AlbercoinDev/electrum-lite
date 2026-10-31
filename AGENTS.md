# Project guidance

Electrum Lite is an experimental Bitcoin thin server, developed from scratch in
Rust. The current README and `docs/project-status.md` describe the product,
implementation, operational boundaries and known limitations. Keep project
descriptions focused on this application and its own design.

Keep version `0.0.1-beta.1` unchanged unless explicitly requested. Do not alter
existing Git tags. Preserve the app's safety requirements: fail closed,
verify network identity and witness data, distinguish repeated TXID occurrences,
bound resources, prohibit mainnet pruning, use isolated disposable test nodes,
and require explicit backups and new destinations. Never delete user data unless
explicitly requested.

Before changes, inspect the affected code and preserve unrelated user edits. Keep
documented limitations explicit. Do not claim production readiness, full public
network synchronization, privacy guarantees, measured mainnet capacity or GUI
interoperability without corresponding evidence.

For Rust changes, run:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

The regtest smoke must use only its own disposable nodes. Never aim smoke tests,
pruning or fault injection at an existing node or valuable data directory.
