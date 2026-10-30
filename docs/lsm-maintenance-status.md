# Mantenimiento LSM: supervisor y poda experimental

[CONFIRMADO] Se añadió `prune_offline` al coordinador LSM: bloqueo mainnet,
confirmación explícita, Core offline y poda manual, coincidencia exacta de punta,
backup completo y restauración semántica en destino nuevo antes de pedir poda.
Intención append-only con fsync previo al RPC. Un resultado incierto deja la
intención inconclusa y bloquea nuevas podas. El journal se incluye en backups.
No hay borrado automático del journal ni conversión de generaciones anteriores.

La operación comprueba pruneheight y error Core de bloque eliminado, y recupera
transacciones originales y pruebas Merkle localmente antes de marcar finalización.
Esto describe controles implementados, **no una prueba de poda real ejecutada**.

La primera prueba detectó que incrementar la generación KV solo por mantenimiento
rompía el replay de bloques. Se sustituyó por journal de mantenimiento separado,
sin alterar generación, cursor ni undo confirmados.

[CONFIRMADO] `managed-serve --lsm` selecciona el servidor y mantenimiento LSM
(requiere feature). El supervisor conserva sus controles de raíz dedicada,
propiedad de procesos y cierre. Espera generación verificada antes de iniciar
Core; con `--auto-prune` detiene el escritor, deshabilita P2P, restaura/verifica
backup, ejecuta poda y reanuda solo tras éxito. Fallos detienen Core. Antes de
arrancar comprueba intención inconclusa; no crea índices SQLite en la ruta LSM.
Mainnet continúa bloqueada. El instalador todavía no selecciona este modo.

## Evidencia reproducible

```sh
cargo fmt --check
cargo clippy --release --locked --all-targets --features experimental-lsm -- -D warnings
cargo test --release --locked --all-targets --features experimental-lsm
cargo build --release --locked --features experimental-lsm
LSM_PRUNE=1 AUTO_PRUNE=1 BITCOIND=/tmp/opencode/bitcoin-31.0/bin/bitcoind ELECTRUM_LITE=target/release/electrum-lite python3 scripts/prune_smoke.py
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

[CONFIRMADO] 88 tests con feature y 53 normales, Clippy y builds correctos.
Ensayo con Core 31.0 offline: 1000 bloques iniciales y 600 bloques con payload
OP_RETURN de 512 KiB. Datos deliberadamente compresibles: NO benchmark de ahorro
ni capacidad de bloques públicos. El objetivo histórico es una transacción
firmada witness del bloque 1001, no una coinbase sin witness. Se verifica error
Core de bloque eliminado, pruneheight posterior y recuperación byte-exact de TX
y prueba Merkle por TCP LSM. Backup completo restaurado y auditado antes de poda.

```text
PASS AUTOMATIC PRUNING: owned supervisor, verified backup, real Core deletion, local witness TX/Merkle, restart cursor
PASS LSM unfinished prune intent blocks supervisor before Core startup; journal and backup preserved
```

El reinicio no repite backup/poda. Inyección de fallo en la raíz desechable:
se conserva solicitud durable omitiendo finalización; el supervisor falla antes
de iniciar Core, conserva journal y backup. Test unitario también cubre backup/
restore de intención inconclusa y cola torn rechazada. No se han probado todos
los fallos de disco ni corte físico de energía. Procesos y datos de ensayo se
limpian; solo evidencia pequeña y código se conservan.

[PENDIENTE] Co-sync sostenida Signet/Testnet4, capacidad, operaciones CLI completas,
ensayo de poda con Core 32.1 y publicación del artefacto/instalador LSM. No se
afirma preparación de producción ni poda mainnet segura.
