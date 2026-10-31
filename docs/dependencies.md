# Dependencias

Versiones resueltas y licencias verificadas en los metadatos oficiales de crates,
registradas en Cargo.lock. Electrum Lite implementa su propio servidor; RocksDB y
rust-bitcoin se utilizan como bibliotecas genéricas.

| Biblioteca | Versión | Licencia |
|---|---|---|
| bitcoin | 0.32.102 | CC0-1.0 |
| rocksdb | 0.24.0 | Apache-2.0 |
| librocksdb-sys / RocksDB | 0.17.3 / 10.4.2 | MIT / Apache-2.0 / BSD-3-Clause, según componente |
| ureq | 2.12.1 | MIT OR Apache-2.0 |
| clap | 4.6.7 | MIT OR Apache-2.0 |
| anyhow | 1.0.100 | MIT OR Apache-2.0 |
| serde_json | 1.0.154 | MIT OR Apache-2.0 |
| base64 | 0.22.1 | MIT OR Apache-2.0 |
| hex | 0.4.3 | MIT OR Apache-2.0 |
| ctrlc | 3.4.5 (SIGINT/SIGTERM/SIGHUP) | MIT OR Apache-2.0 |

APIs consultadas:

- https://docs.rs/bitcoin/0.32.102/bitcoin/blockdata/block/struct.Block.html
- https://docs.rs/rocksdb/0.24.0/rocksdb/struct.WriteBatchWithTransaction.html
- https://docs.rs/rocksdb/0.24.0/rocksdb/checkpoint/struct.Checkpoint.html
- https://docs.rs/ureq/2.12.1/ureq/
- https://docs.rs/ctrlc/3.4.5/ctrlc/ (API `set_handler`, feature `termination`)
- https://github.com/bitcoin/bips/blob/master/bip-0141.mediawiki
- https://github.com/bitcoin/bips/blob/master/bip-0144.mediawiki
- https://github.com/bitcoin/bitcoin/blob/v31.0/src/rpc/blockchain.cpp (`pruneblockchain`)
- https://github.com/bitcoin/bitcoin/blob/v31.0/src/kernel/chainparams.cpp (`nPruneAfterHeight`)

Core 31 exige altura al menos 1000 para podar Signet, Testnet4 y regtest normal;
el parámetro auxiliar `fastprune` reduce ese umbral en regtest, pero no se usa ni
se expone como bypass. La prueba física mina 601 bloques pequeños y 450 grandes
antes de la primera poda. El supervisor no intenta mantenimiento por debajo de
1000, incluso si el usuario pide un intervalo menor.

Se revisó también el código publicado de `Block::check_witness_commitment`: su
atajo para bloques sin ningún witness no basta para rechazar bloques completamente
despojados por un peer. La capa de red aplica una comprobación adicional cuando
hay commitment en coinbase y una prueba unitaria que rechaza ese caso.

Rust está fijado a 1.99.1. Core de la prueba local: 31.0.0. Tor no es una crate ni
un binario redistribuido aquí: proceso externo gestionado por la app, instalable
desde la distribución. Tor upstream tiene licencia BSD-3-Clause, pero el paquete
Debian **0.4.9.15-0+deb13u1** utilizado se compiló con `--enable-gpl` y su ejecutable
declara **GPL-3.0**. Se comprobó `tor --version` y el copyright del paquete;
no debe atribuirse BSD sin matices a ese binario. No se usa Arti ni libtor enlazada.
Los resultados de transporte/bootstrap están en `docs/verification.md`.

La licencia propia del proyecto no se modifica ni se supone por la de sus dependencias.
