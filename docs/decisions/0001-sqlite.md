# ADR-001 — SQLite para los índices persistentes

Estado: decidido para la continuación de `0.0.1-beta.1`. No implica formato estable ni autorización de poda.

Experimento opt-in posterior: [prototipo LSM aislado](../benchmarks/lsm-prototype.md).
No sustituye esta decisión ni habilita RocksDB como almacenamiento runtime.

Nota histórica: el esquema 1 y las limitaciones originales corresponden a la primera entrega de persistencia. La implementación actual usa schema 2 y el log de ramas de [ADR-002](0002-branch-log.md).

## Decisión

Una base SQLite mediante rusqlite 0.41.1 y SQLite empaquetado (`bundled`). Esquema 1 en `src/schema.sql`; bloques originales fuera de la base en ELBETA01. Usar journal DELETE, synchronous EXTRA, claves foráneas activas, mmap desactivado y caché de páginas sugerida de 8 MiB (ampliada a 64 MiB en runtime tras el banco de ingesta; el microbenchmark original de esta ADR se midió con 8 MiB). No usar WAL inicialmente para simplificar copias cerradas y recuperación.

Historial, UTXO y ocurrencias encajan en tablas e índices estándar. Elegimos SQLite por simplicidad transaccional, restricciones verificables y herramientas de integridad/reconstrucción, **no por afirmar que sea el más rápido**. redb solo queda como dependencia de desarrollo para reproducir la comparación.

## Microexperimento ejecutado

```sh
cargo build --release --locked
TMPDIR="$PWD/target" cargo run --release --locked --example backend_compare
```

100 transacciones durables; 256 inserciones de 96 bytes por transacción y 64 borrados del lote previo desde la segunda; 19 264 filas finales y 1 000 lecturas pseudoaleatorias warm. SQLite DELETE/EXTRA frente a redb 4.4.0 Immediate, cachés configuradas a 8 MiB. Ambos solicitan commits durables, aunque sus algoritmos y llamadas de sincronización difieren.

Una ejecución sobre el filesystem del workspace (`stat -f` informa ext2/ext3; almacenamiento físico no caracterizado), Linux 6.12.113+deb13-amd64, x86-64 AMD Ryzen 9 7940HS, unos 8 GB de RAM, Rust 1.99.1, perfil release genérico:

| Backend | Escritura de lotes | 1 000 lecturas | Archivo final |
|---|---:|---:|---:|
| SQLite | 515 ms | 11 409 µs | 2 375 680 B |
| redb | 225 ms | 956 µs | 4 214 784 B |

Es una comparación sintética pequeña, no la carga completa del indexador. No mide RSS, cold cache, p95/p99, amplificación de escritura, desgaste, latencia bajo carga ni fallos de energía. No demuestra rendimiento en SSD SATA o hardware antiguo ni ratios mainnet. Una ejecución previa en tmpfs solo sirvió para comprobar el harness; no se usa como evidencia de rendimiento de disco.

## Consistencia y recuperación

- Una transacción SQL prepara cambios y undo sin publicarlos; el archivo se sincroniza antes de insertar/publicar referencias y confirmar la base.
- Recuperación verifica archivo y referencias ya confirmadas; los registros completos posteriores se importan una sola vez. No se truncan colas corruptas ni se sirve una consulta si falla esa verificación.
- La primera importación de un archivo antiguo añade index.sqlite sin modificar ELBETA01. Copia previa completa y reconstrucción separada para migraciones/rollback; los esquemas desconocidos se rechazan.
- Undo de las últimas 288 alturas: primitiva interna probada, no recuperación automática de reorg. El archivo sigue siendo lineal; no retroceder solo la base desde la CLI ni podar.
- La verificación inicial completa es costosa en E/S, aunque ya no reconstruye UTXO/historial en RAM. La optimización de arranque y auditoría semántica exhaustiva quedan pendientes.

Fuentes: [rusqlite](https://docs.rs/rusqlite/0.41.1/rusqlite/), [SQLite synchronous](https://www.sqlite.org/pragma.html#pragma_synchronous), [atomic commit](https://www.sqlite.org/atomiccommit.html), [redb](https://docs.rs/redb/4.4.0/redb/). Las garantías reales dependen de que SO, filesystem y dispositivo respeten las sincronizaciones.
