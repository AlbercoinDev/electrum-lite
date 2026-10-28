# Dependencias de la beta

Versiones concretas resueltas y checksums en `Cargo.lock`. No se ha realizado una auditoría de seguridad independiente.

| Componente directo | Función | Licencia declarada |
|---|---|---|
| bitcoin 0.32.102 | Tipos, serialización, hashes, génesis y comprobaciones Merkle/witness | CC0-1.0 |
| zstd 0.13.3 | Compresión sin pérdidas por bloque | MIT; biblioteca C Zstandard BSD-3-Clause OR GPL-2.0 |
| ureq 2.12.1 | Cliente RPC HTTP síncrono local, sin TLS/gzip | MIT OR Apache-2.0 |
| clap | CLI | MIT OR Apache-2.0 |
| serde / serde_json | JSON y serialización | MIT OR Apache-2.0 |
| anyhow | Errores | MIT OR Apache-2.0 |
| base64 | Autenticación Basic de cookie | MIT OR Apache-2.0 |
| hex | Codificación hexadecimal | MIT OR Apache-2.0 |
| fs2 | Bloqueo de archivo entre procesos | MIT OR Apache-2.0 |
| url | Validación de endpoint | MIT OR Apache-2.0 |
| tempfile (tests) | Directorios temporales aislados | MIT OR Apache-2.0 |

Las licencias completas, incluidas las transitivas y código nativo como secp256k1/Zstandard, están en los paquetes originales. Antes de distribuir binarios o elegir licencia propia, revisar y proporcionar avisos/licencias que correspondan. Esta tabla no reemplaza esos textos.

No se necesita Tokio en esta fase: no hay servidor ni procesamiento concurrente. Añadirlo al implementar red Electrum, sin bloquear su runtime con el archivador síncrono.

Referencias consultadas: documentación oficial de [rust-bitcoin](https://docs.rs/bitcoin/0.32.102/bitcoin/), [ureq](https://docs.rs/ureq/2.12.1/ureq/), [zstd](https://docs.rs/zstd/0.13.3/zstd/), metadatos de crates.io y RPC [getblock de Core 31.0](https://bitcoincore.org/en/doc/31.0.0/rpc/blockchain/getblock/). El transporte RPC entrega hex dentro de JSON. Se eligieron APIs concretas comprobadas al compilar, no versiones inferidas de pseudocódigo.
