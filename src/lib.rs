//! Original thin Electrum indexer. Experimental; Core owns consensus.
pub mod core;
pub mod core_blockfile;
pub mod i18n;
pub mod index;
pub mod mempool;
pub mod net;
pub mod prune;
pub mod retention;
pub mod server;
pub mod supervisor;
/// Conservative Core 31 default gate for Signet, Testnet4 and normal regtest.
/// Mainnet pruning is separately forbidden; no fastprune/unsafe override.
pub const TESTNET_PRUNE_MIN_HEIGHT: u32 = 1000;
/// Exit status reserved for a semantic audit whose Core chain changed.
pub const AUDIT_STALE_EXIT_CODE: u8 = 75;
