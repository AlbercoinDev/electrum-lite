# Repository status and source of truth

- This is a specification-only project: no implementation, Cargo workspace, build/test commands, CI, or benchmarks exist yet. Do not describe the proposed layout or capabilities as implemented.
- Read `ELECTRUM_LITE_ENGINE_v1.0.md` before implementation. The filename is retained; revision 1.2.0-draft includes the simplified architecture, required mainnet support and GitHub distribution.
- Preserve the specification's distinctions: `[CONFIRMADO]`, `[DECIDIDO]`, `[HIPÓTESIS]`, and `[PENDIENTE]`. Example APIs, configurations, resource budgets, and pseudocode are not validated implementations.

# Implementation boundaries

- Build an original Rust indexer and Electrum server, not a fork or copied architecture/code from Electrs, Fulcrum, or bindex. Mature generic libraries are allowed; verify versions, APIs, and licenses against official sources before implementation and pin dependencies with `Cargo.lock`.
- Follow phases A–F (§9). Start with one Cargo package with library modules and CLI, not a large workspace. Once it exists, run `cargo fmt --check`, `cargo clippy`, and `cargo test` plus applicable integration tests.
- The target is one process with bounded queues, one logical confirmed-state writer, and consistent reader snapshots. Bitcoin Core owns consensus; Electrum 1.4 is the protocol baseline. Keep mempool state separate from confirmed state.
- Archive complete blocks including witness in independent Zstandard frames within bounded append-only files. Retrieve transactions and build Merkle proofs from those blocks. Use one mature transactional database for indexes, UTXO, undo and metadata; backend selection awaits a small equal-durability comparison. No custom history segments, compaction or transaction compression initially. Baseline x86-64 must not require AVX2.
- Start with cookie-authenticated RPC, not REST or ZMQ. `getblock(hash, 0)` returns hex in JSON, not raw binary. Reconcile mempool over RPC.

# Data safety and verification

- Core uses manual pruning (`prune=1`, `txindex=0`). Sync and verify archived blocks before committing index references, UTXO, undo and metadata in one durable database transaction; publish a consistent generation before considering pruning (§§5–6). Archive-only persistence is insufficient. Mainnet support is a product requirement, but real mainnet deployment/pruning must wait for safety and capacity validation.
- Support mainnet, Signet and Testnet4 with explicit network selection, isolated configuration/data and identity checks against Core. Keep initial live integration tests on Signet/Testnet4; do not mistake that test scope for dropping mainnet support.
- Publish through `AlbercoinDev/electrum-lite`; keep README installation/operation instructions aligned with verified releases. Until code exists, explicitly state that only documentation can be downloaded, not an operational server installed.
- An empty index against an already-pruned Core must fail clearly unless trusted historical data is available. Historical queries must work locally after Core deletes blocks; retain original transaction bytes including witness, headers, and ordered block TXIDs.
- Do not assume historical TXIDs are globally unique (BIP30); distinguish occurrences by block hash and position. Deduplicate history per transaction/scripthash, preserve intra-block spend order, and fail on missing prevouts. Reverse scripthash bytes only at the Electrum protocol boundary.
- Schema changes, pruning, deletion, and restoration require backup, migration/recovery, rollback, and fail-safe behavior. Never trade production durability for benchmark speed or run destructive pruning on valuable node data.
- Signet and Testnet4 are the integration networks; isolate their data/configuration and verify network identity. Regtest is auxiliary for deterministic reorg, pruning and fault-injection tests. A successful prune RPC alone is not proof: check actual prune height and block unavailability, then retrieve original transaction bytes and validate Merkle proofs without historical Core access (§8).
- TXID/Merkle-root checks alone do not protect witness bytes: verify the witness commitment where applicable and test byte-exact archive round trips. Measure total Core + archive + database disk usage and temporary peaks; compression savings and low-resource operation are unproven.
- Default services to localhost, keep RPC credentials out of the repository, and avoid wallet-identifying logs. Record reproducible tests and remaining risks; hardware budgets and performance claims remain unmeasured until demonstrated.
