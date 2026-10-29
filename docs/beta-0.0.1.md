# Beta 0.0.1 — estado y validación

Versión Cargo: `0.0.1-beta.1`, mantenida por petición del usuario. Fecha: 2026-10-28. El tag inicial conserva A/B; main contiene el servidor privado funcional de laboratorio. Los apartados iniciales documentan entregas históricas, no las limitaciones actuales.

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

## Primera continuación SQLite (histórica), misma versión

- SQLite schema 1 para UTXO, historial, ocurrencias, referencias de bloques, contadores y undo. Caché sugerida de 8 MiB, journal DELETE, synchronous EXTRA, claves foráneas y hashes binarios.
- Commit por bloque: preparar SQL, sincronizar archivo, registrar ubicación y confirmar base. Recuperar registros completos no confirmados una sola vez y rechazar generaciones mezcladas, colas truncadas y referencias ausentes.
- Importar archivos ELBETA01 existentes sin reescribirlos. Desconocer un esquema causa fallo seguro; reconstrucción no destructiva mediante `rebuild-index --output NUEVO.sqlite`. Procedimiento de backup/rollback en README.
- Consultas SQL con límite explícito de resultados y recuperación de TX por ubicación verificada. Se sigue leyendo/verificando todo el archivo al arrancar: no prometer arranque constante ni latencias mainnet.
- Undo interno de 288 bloques, incluidos gastos intra-bloque; límite profundo falla sin modificar estado. No expuesto como recuperación operacional: el archivo sigue siendo lineal y la CLI rechaza reorgs.
- Microexperimento SQLite/redb y elección documentada en ADR-001. No representa una carga Bitcoin ni demuestra mínimos de hardware o ahorro total.

Pruebas nuevas: paridad contra índice de referencia, reapertura/importación en cuatro redes, referencias/tails mezclados o ausentes, esquema desconocido, límites sin escrituras parciales, undo/retención, error de append, SQLITE_FULL mediante límite de páginas y subprocesos que salen abruptamente con SQL preparado, archivo sincronizado, referencias preparadas y después del commit. El harness Core añade reconstrucción separada y restauración de copia cerrada en regtest.

Resultado local de la continuación: **25 tests aprobados**, fmt y clippy correctos, build release e integración Core 31.0 aprobados en las cuatro redes aisladas. No se ha realizado sincronización sostenida pública ni ensayo real de pérdida de energía.

## Beta funcional actual en main

- Electrum 1.4 TCP loopback, consultas históricas/Merkle, mempool padre/hijo y RBF, difusión, fees y suscripciones; worker único con límites y readiness.
- SQLite schema 2: cursor y rewind durable, reorgs hasta undo retenido y recuperación profunda a otra raíz conservando el origen. Migración explícita con backup, manifiestos, restore y auditoría semántica.
- Poda manual de prueba con auditoría, backup nuevo y Core offline; mainnet bloqueada sin override. Ejemplos systemd no iniciados aquí.
- Verificación Rust: **37 tests**, formato, clippy y release. Integración Core 31.0: TCP, RBF, reorg superficial/profunda, generación reconstruida y bytes SQLite originales preservados.
- Poda efectiva regtest: 1 600 bloques, 600 con OP_RETURN grandes; Core deja de servir el bloque histórico y el socket Electrum devuelve TX original con witness y Merkle válido. Datos deliberadamente compresibles, no benchmark.
- P2P público acotado: Signet y Testnet4, 100 bloques/101 TX cada una, auditados. Signet: 4 pares, 49 038 902 bytes recibidos; Testnet4: 1 par, 21 299 775 bytes. No sincronización completa.

## Aceptación externa pendiente

### Experimento de lotes de sincronización

En Linux x86-64 (Ryzen 9 7940HS, 8 CPU visibles, Linux 6.12), Core 31.0
regtest offline generó 1 000 bloques coinbase. Indexar 1 001 bloques en
`/tmp/opencode` (tmpfs) tomó 9,202 s con `8c66dd8` y 7,245 s con el commit
por lotes: 108,8 frente a 138,2 bloques/s. Orden fijo, una repetición, sin
IBD concurrente. **No es un benchmark de SSD ni prueba que alcance a Core**;
tmpfs no mide la latencia de sincronización de disco físico. Continúan
pendientes pipeline concurrente, mediciones por etapa y pruebas sostenidas.

- Sincronización sostenida real en Signet/Testnet4 y pruebas de parámetros/casos históricos representativos de mainnet. Génesis offline no valida toda una red.
- Prueba exhaustiva de undo histórico BIP30 real y recuperación a escala.
- Fault injection exhaustiva, fallos de disco lleno y recuperación validada de pérdida de energía. Las pruebas de truncado/corrupción no cubren todos esos escenarios.
- Ensayo real con Sparrow GUI; Java/Sparrow/entorno gráfico no disponibles aquí. TLS nativo no implementado: remoto requiere túnel/proxy seguro.
- Formato estable, migraciones entre futuros esquemas, pruebas de backup/restore a mayor escala y elección de licencia.
- Mediciones de CPU/RAM/latencia, disco total y pico temporal. No extrapolar ahorro de otros corpus o redes.

Si hay corrupción, conservar archivos y reconstruir aparte o restaurar una copia verificada. No se truncan ni migran datos silenciosamente. Usar carteras de laboratorio sin fondos reales; no podar nodos valiosos. Procedimientos actuales: README, docs/operations.md y docs/protocol.md.
