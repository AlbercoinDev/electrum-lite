# Prototipo LSM aislado: primera puerta de evaluación

Fecha: 2026-10-29. Estado: **experimental, no integrado**.

## Alcance y decisión

[CONFIRMADO] `examples/support/lsm.rs` implementa una capa KV usando la
biblioteca genérica rocksdb 0.24.0, librocksdb-sys 0.17.3+10.4.2 fijadas en
Cargo.lock. Feature opt-in `experimental-lsm`; el ejecutable normal y el
instalador siguen utilizando SQLite. No importa ni migra instalaciones antiguas.
No se ha copiado código, esquema o arquitectura de electrs/Fulcrum/bindex.

[DECIDIDO] ADR-001 sigue vigente para runtime. Este experimento no autoriza
pruning, no tiene acceso a Core y no interpreta sus claves como un índice Bitcoin.

Características verificadas:

- Lote atómico con generación esperada y siguiente generación dentro del mismo
  WriteBatch. WAL explícitamente habilitado, `sync=true`, `use_fsync=true`.
- Rechazo de claves reservadas, duplicados, generaciones obsoletas y deltas
  mayores de 16 MiB/65536 mutaciones; error de escritura exige reabrir.
- Compacciones automáticas habilitadas, flush+compactación final contabilizados.
- Snapshot consistente de datos y generación de la biblioteca genérica.
- Checkpoint mediante API oficial (no copia arbitraria de DB abierta), inventario
  SHA256 sincronizado y manifiesto de generación. Restore a destino nuevo,
  verificación de archivos antes/después de copiar, identidad y generación.
- No sobrescribe DB/backup/destino existentes; rechaza enlaces/subdirectorios
  de checkpoint y backup dentro de DB/restore dentro de backup.
- Recuperación WAL tras salida de subproceso después del commit y sin ejecutar
  destructores. **No es una prueba física de apagón**, ni inyección de EIO/ENOSPC.

## Ejecución reproducible

Requiere compilador C++, libclang con sus resource headers y Rust fijado.
Se verificaron versión, API y licencia Apache-2.0 en fuentes oficiales:

- https://github.com/rust-rocksdb/rust-rocksdb/blob/v0.24.0/Cargo.toml
- https://github.com/rust-rocksdb/rust-rocksdb/blob/v0.24.0/LICENSE
- https://github.com/rust-rocksdb/rust-rocksdb/blob/v0.24.0/src/db_options.rs
- RocksDB 10.4.2 incluido por el crate: `rocksdb/LICENSE.Apache`.

No se instalaron paquetes del sistema. libclang1-19, libllvm19 y
libclang-common-19-dev de Debian se extrajeron en una carpeta propia temporal.
Flags C++ `-g0 -march=x86-64`, sin AVX2 obligatorio ni `target-cpu=native`.
Compilación acotada a dos trabajos; no se midió pico RSS del compilador.

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets --features experimental-lsm -- -D warnings
cargo test --locked
cargo test --locked --all-targets --features experimental-lsm
cargo build --release --locked
cargo run --release --locked --features experimental-lsm --example lsm_compare
```

[CONFIRMADO] Todos pasaron: 52 tests runtime y 4 tests adicionales del ejemplo.
Estos comandos usaron el árbol de trabajo que contiene el cambio pendiente
de ventana catch-up; ese cambio no forma parte del prototipo ni de su publicación.
No se generó ni sustituyó el paquete público Lite `d258034` con ese árbol.

## Resultado pequeño, no banco Bitcoin

100 generaciones de 512 inserciones, claves SHA256 completas con prefijo de
namespace y valores de 256 bytes; 51200 filas. SQLite DELETE/EXTRA, mmap OFF,
caché 8 MiB. RocksDB caché de bloques 8 MiB y dos memtables de 4 MiB cada una:
**el presupuesto de memoria total no es igual**. Sin compresión RocksDB; no se
afirma un ahorro de espacio. La identidad/generación también se persiste.

Una ejecución release en el filesystem del repositorio, no tmpfs:

```text
synthetic only: SQLite DELETE/EXTRA 13227 ms; RocksDB WAL/sync 400 ms;
additional flush+compaction 191 ms; 51200 restored rows verified
PASS temporary databases and backups removed
```

Se compararon byte a byte las 51200 filas de SQLite con las del backup RocksDB
restaurado en otra DB. Las DB y backup residen en un TemporaryDirectory del
workspace que se elimina incluso si retorna un error.

[HIPÓTESIS] El patrón LSM podría reducir el coste del escritor observado en
Signet. Estos tiempos únicamente justifican continuar el experimento.
**No demuestran indexación Bitcoin rápida, co-sincronización, durabilidad
equivalente del sistema completo ni una ventaja sostenida sobre Core.**

## Pendiente antes de integrar

1. Deltas reales equivalentes con UTXO, historial, undo, BIP30, bajas y consultas;
   tamaño mayor que la memoria total, no solo la caché de bloques.
2. Coordinación archivo fsync → commit índices/undo/cursor → publicación;
   replay idempotente, reorgs y fallos en cada frontera. Ninguna API de este
   prototipo representa todavía una autorización de poda.
3. Backup de generación completa (archivo + DB), no solo checkpoint KV.
4. Medir RSS, bytes físicos escritos, colas, latencias, picos temporales y disco
   total; varios ciclos incluyendo compactación y deletes, órdenes alternados.
5. Tras superar esas puertas: integración nueva desde cero, suites Core completas
   y co-sync pública acotada. Mainnet pruning sigue bloqueada sin override.

El prototipo no cambia los comandos Signet publicados ni los datos existentes.
