# Beta 0.0.1 — estado y validación

Versión Cargo: `0.0.1-beta.1`, mantenida por petición del usuario. Fecha: 2026-10-28. El tag inicial conserva A/B; la continuación en main añade parte de C. No es una entrega de servidor Electrum.

## Entrega inicial (tag v0.0.1-beta.1)

- CLI `check-core`, `sync`, `inspect`, `history` y `transaction`.
- Mainnet, Signet, Testnet4 y regtest auxiliar, con puertos/génesis de rust-bitcoin y configuración RPC explícita. Core es autoridad de consenso.
- Cookie RPC, límite de respuesta HTTP, timeouts, loopback numérico y redirecciones deshabilitadas.
- Archivo `ELBETA01`: identidad de red, altura, hash, tamaños, checksum SHA256d del payload y frame Zstandard. Segmentos hasta 64 MiB, Zstandard nivel 3, límite de bloque descomprimido 4 MB y ventana de decodificación 8 MiB.
- Continuidad, Merkle y witness comprobados. `fsync` antes de informar éxito, bloqueo exclusivo y replay completo antes de append.
- Índice en memoria con cambios por bloque preparados antes de aplicar: UTXO, historial deduplicado, gastos intra-bloque, génesis no gastable y rama para excepciones BIP30. Recuperación offline de todas las ocurrencias TXID o por altura.

No hay commit coordinado de índices persistentes. Ninguna altura archivada autoriza poda. Los límites acumulados de transacciones y entradas UTXO/historial son guards de crecimiento, no límites precisos de bytes/RSS.

## Verificación de la entrega inicial

Entorno usado: Linux x86-64, Rust 1.99.1, Bitcoin Core 31.0. Compilación release genérica, sin flags `target-cpu=native` ni AVX2 obligatorio.

Resultado local: formato y clippy sin errores, **14 tests aprobados**, build release correcto e integración aislada aprobada en las cuatro redes. No se han ejecutado benchmarks ni sincronización pública sostenida.

- `cargo fmt --check`.
- `cargo clippy --locked --all-targets -- -D warnings`.
- `cargo test --locked`: pruebas de RPC/cookie/errores sin secretos, límite de respuesta, archivo/génesis de cuatro redes, corrupción/tail truncado, exclusión de escritor, tamaño/red incorrectos, indexación intra-bloque, deduplicación, doble gasto, fallo atómico, reorg rechazada, límite de transacciones, BIP30 sintético y CLI offline.
- `cargo build --release --locked`.
- `scripts/core_smoke.py`: nodos temporales de Core con red desactivada. Mainnet, Signet y Testnet4: RPC, identidad, génesis, replay y rechazo de red incorrecta. Regtest: 106 bloques, gasto con witness, recuperación byte a byte, replay/reanudación y rechazo de punta invalidada.

El tar oficial Core usado coincidió con SHA256 `d3e4c58a35b1d0a97a457462c94f55501ad167c660c245cb1ffa565641c65074`, obtenido del manifiesto HTTPS oficial. No se verificaron firmas de mantenedores en esta ejecución; el checksum no sustituye ese procedimiento de autenticidad.

## Continuación en main, misma versión

- SQLite schema 1 para UTXO, historial, ocurrencias, referencias de bloques, contadores y undo. Caché sugerida de 8 MiB, journal DELETE, synchronous EXTRA, claves foráneas y hashes binarios.
- Commit por bloque: preparar SQL, sincronizar archivo, registrar ubicación y confirmar base. Recuperar registros completos no confirmados una sola vez y rechazar generaciones mezcladas, colas truncadas y referencias ausentes.
- Importar archivos ELBETA01 existentes sin reescribirlos. Desconocer un esquema causa fallo seguro; reconstrucción no destructiva mediante `rebuild-index --output NUEVO.sqlite`. Procedimiento de backup/rollback en README.
- Consultas SQL con límite explícito de resultados y recuperación de TX por ubicación verificada. Se sigue leyendo/verificando todo el archivo al arrancar: no prometer arranque constante ni latencias mainnet.
- Undo interno de 288 bloques, incluidos gastos intra-bloque; límite profundo falla sin modificar estado. No expuesto como recuperación operacional: el archivo sigue siendo lineal y la CLI rechaza reorgs.
- Microexperimento SQLite/redb y elección documentada en ADR-001. No representa una carga Bitcoin ni demuestra mínimos de hardware o ahorro total.

Pruebas nuevas: paridad contra índice de referencia, reapertura/importación en cuatro redes, referencias/tails mezclados o ausentes, esquema desconocido, límites sin escrituras parciales, undo/retención, error de append, SQLITE_FULL mediante límite de páginas y subprocesos que salen abruptamente con SQL preparado, archivo sincronizado, referencias preparadas y después del commit. El harness Core añade reconstrucción separada y restauración de copia cerrada en regtest.

Resultado local de la continuación: **25 tests aprobados**, fmt y clippy correctos, build release e integración Core 31.0 aprobados en las cuatro redes aisladas. No se ha realizado sincronización sostenida pública ni ensayo real de pérdida de energía.

## Pendiente antes de una beta operativa de Electrum

- Sincronización sostenida real en Signet/Testnet4 y pruebas de parámetros/casos históricos representativos de mainnet. Génesis offline no valida toda una red.
- Recuperación operacional de reorg con archivo consciente de ramas, publicación de snapshots y prueba exhaustiva de undo histórico BIP30 real; la primitiva interna no basta.
- Fault injection exhaustiva, fallos de disco lleno y recuperación validada de pérdida de energía. Las pruebas de truncado/corrupción no cubren todos esos escenarios.
- Controlador de poda y consulta después de poda efectiva. Esta beta nunca ejecuta `pruneblockchain`.
- Servidor Electrum, pruebas Merkle expuestas, mempool, suscripciones, TLS y Sparrow.
- Formato estable, migraciones entre futuros esquemas, pruebas de backup/restore a mayor escala y elección de licencia.
- Mediciones de CPU/RAM/latencia, disco total y pico temporal. No extrapolar ahorro de otros corpus o redes.

Si hay corrupción o reorg, conservar archivos y reconstruir aparte o restaurar una copia verificada. La CLI no elimina, trunca ni migra datos silenciosamente. No conectar una cartera esperando un servicio Electrum ni usar esta beta para decidir la poda de un nodo valioso.
