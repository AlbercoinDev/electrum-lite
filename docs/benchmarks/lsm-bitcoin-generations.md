# Generaciones Bitcoin LSM experimentales

Fecha de ejecución: 2026-10-29. No conectado al CLI, al supervisor ni a la poda.
Complementa el [experimento KV](lsm-prototype.md), no sustituye ADR-001.

## Implementación

[CONFIRMADO] `src/experimental_lsm.rs` es un coordinador original, feature
`experimental-lsm`, sobre `src/experimental_lsm/kv.rs` (la primitiva antes
ubicada en examples/support). El ejecutable normal conserva SQLite; no cambia
la versión beta, el tag original ni el artefacto Linux publicado.

Una raíz NUEVA contiene archivo ELBETA01 por red y un directorio `lsm`:

1. Validar bloque, continuidad y prevouts; preparar overlay en orden, índices
   de UTXO/script, historial deduplicado y ocurrencias por TXID+bloque+posición.
2. Preparar undo con los valores ANTERIORES de cada clave afectada, incluyendo
   sobrescrituras BIP30, y prevalidar tamaño/cantidad del lote antes del append.
3. Append del archivo mediante Archive::append, que verifica Merkle/witness y
   sincroniza bytes y directorios. No se publican todavía referencias.
4. Un WriteBatch WAL habilitado/sync/fsync confirma datos, undo, bloque,
   generación y cursor físico del archivo. Solo después se admiten consultas.

Los hashes de script internos son bytes SHA256 completos, sin invertirlos para
Electrum. El historial consultable excluye génesis, pero archivo y ocurrencias
lo conservan. Se distingue cada ocurrencia por bloque y posición; no se impone
unicidad global de TXID. Los límites de respuesta producen error, no truncado.

Undo retiene 288 bloques. Rewind valida disponibilidad y escribe primero una
repetición durable del ancestro, reutilizando el log de ramas de Archive.
Después aplica undo en orden inverso y actualiza cursor/punta atómicamente.
Nunca se usa un rollback solo de DB como operación de recuperación.

## Recuperación y backup

[CONFIRMADO] Open verifica identidad/red/génesis, todas las referencias
publicadas y todo el archivo, busca el cursor físico durable e importa la cola
completa posterior. Repite también intenciones de rewind pendientes. Cursor
ausente, referencia mezclada/faltante o cola rota fallan sin truncar.

Una auditoría semántica reproduce el log completo en otro LSM temporal y compara
ambas direcciones de todos los namespaces de datos/undo/estado. Se detectó por
test una moneda borrada solo de los índices. Este replay completo es
**deliberadamente caro y NO una solución de arranque escalable validada**.

Backup exige propiedad exclusiva y auditoría; copia los segmentos completos y
crea checkpoint de DB con manifiesto, después inventario SHA256 de toda la
generación más red, punta y cursor. Sincroniza archivos/manifiestos/directorios.
Restore exige destino nuevo fuera del backup, verifica inventario y red antes
de copiar, restaura DB y archivo y vuelve a recuperar/auditar la generación.
Se preservan tanto origen como backup. Las copias parciales fallidas no se
activan, no se mezclan y no se borran automáticamente.

## Evidencia ejecutada

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets --features experimental-lsm -- -D warnings
cargo test --locked
cargo test --locked --all-targets --features experimental-lsm
cargo build --release --locked
cargo build --locked --features experimental-lsm --example lsm_bitcoin_check
```

[CONFIRMADO] Todos pasaron: **52 tests normales**, **66 con la feature**
(incluyen helpers de subproceso). Cobertura adicional:

- Bitcoin UTXO/historial contra Store SQLite, archivos byte-exact incluyendo
  witness y límites de respuesta.
- Gastos en orden dentro del mismo bloque, fallo por orden incorrecto antes de
  append, restauración exacta del estado anterior mediante undo.
- Caso BIP30 sintético de sobrescritura, conservando moneda/script anterior.
- Subprocesos terminados sin destructores antes de append, después de fsync
  y después de commit. Reapertura e importación idempotente.
- Intención ancestral durable sin rollback DB, nueva rama y reapertura.
- 289 bloques: rechazo de rewind más profundo que 288 sin append; rewind de
  exactamente 288 y recuperación posterior.
- Backup/restauración de archivo + DB tras reorg, sin modificar origen; rechazo
  de red incorrecta y alteraciones del backup antes de crear destino.
- Cola rota, referencia truncada y corrupción lógica solo del índice.

Prueba adicional con **Core 31.0 real**, regtest propio con P2P deshabilitado:

```sh
python3 scripts/lsm_bitcoin_smoke.py \
  --bitcoind /tmp/opencode/bitcoin-31.0/bin/bitcoind \
  --bitcoin-cli /tmp/opencode/bitcoin-31.0/bin/bitcoin-cli \
  --example target/debug/examples/lsm_bitcoin_check
```

Generó 101 bloques, gastó una coinbase madura a dirección bech32 y minó el gasto
witness firmado: **103 bloques contando génesis**. Verificó cada UTXO producido
contra SQLite tras cada bloque, todo historial por script, bytes originales de
cada bloque y todos los bloques del backup completo restaurado, más reopen con
auditoría. Salida:

```text
PASS 103 Core-generated blocks: SQLite/LSM UTXO+history, byte-exact witness archive, semantic replay, full backup+restore
PASS experimental indexes, archives, audit scratch and backups removed
PASS owned offline Core stopped; generated blocks, wallet and temporary files removed
```

Se usó perfil dev para la comprobación funcional: no se publican tiempos de
rendimiento. El build release comprobado es el normal, sin feature LSM.
Las herramientas y compilaciones temporales de este ensayo se eliminan al
final; no se conserva cadena, wallet ni índice de laboratorio.

Las pruebas se ejecutaron con el cambio de catch-up pendiente presente en el
árbol, que no pertenece a este bloque. No se empaquetó ese binario ni se cambió
la pareja pública validada Lite d258034/Core 32.1.

## Puertas pendientes

[PENDIENTE] Todavía NO es backend de servicio ni arquitectura final:

- Consultas completas Electrum/mempool, estadísticas/caps acumulados y API de
  snapshots de generación del coordinador; el prototipo conserva JSON en valores.
- Inyección EIO/ENOSPC y pruebas físicas de energía; subprocesos no prueban apagón.
- Comparación representativa por lotes equivalentes con índices mayores que la
  memoria total, compactaciones, RSS, bytes físicos y picos de disco.
- Sustituir/revisar el replay completo de auditoría solo tras medir capacidad,
  sin sacrificar validación de referencias y recuperación.
- Integración de sincronizador y servidor, suites Core 32.1 completas con ese
  backend y co-sincronización pública acotada. No se ha demostrado todavía
  retraso decreciente mientras Core avanza sin pausas de mantenimiento.

No hay autorización de poda LSM ni mainnet pruning; el servicio publicado sigue
usando SQLite y los comandos Signet publicados no cambian.
