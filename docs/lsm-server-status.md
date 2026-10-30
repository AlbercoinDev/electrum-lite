# Servidor Electrum LSM — 2026-10-30

[CONFIRMADO] `lsm-serve`, disponible en Linux con la feature `experimental-lsm`,
integra sincronizador, snapshots y mempool del nuevo motor. No es el motor
entregado por el instalador público y no administra todavía Bitcoin Core.

## Implementado

- TCP Tokio en loopback; un worker nativo posee Core, LSM, archivo y mempool.
  Cola de 64 solicitudes, conexiones y suscripciones acotadas. Trabajo bloqueante
  fuera de tareas async. Diagnóstico y ping independientes del worker.
- Electrum **1.4 exclusivamente**: negociación, features, cabeceras/chunks y
  checkpoints, saldo, historial, UTXO, mempool, suscripciones, transacciones
  raw/verbose, Merkle/id_from_pos, broadcast, fees e histograma.
- Readiness exige alcanzar Core observado y completar una mempool fresca ligada
  a la generación. Core inaccesible bloquea carteras incluso con índice local
  completo. `ready` no certifica sincronización pública completa ni capacidad.
- Frames de hasta 8.100.000 bytes, respuestas hasta 16.200.000 bytes y batches
  de 1..32. Límites devuelven error, no resultados truncados. Lecturas fragmentadas
  sobreviven a la cancelación por el temporizador de suscripciones.
- Suscripciones conservan la punta del snapshot inicial: un reorg anterior al
  primer poll notifica incluso cuando el status del script no cambia.
- Contadores durables de transacciones, UTXO e historial, aplicados antes del
  append y durante recuperación/importación; participan en undo y auditoría.
  Identidad experimental **lite-bitcoin-lsm-v3**. Raíces anteriores se rechazan,
  sin adaptación automática. Estas cotas no son un límite RSS medido.

## Evidencia

[CONFIRMADO] Verificación ejecutada antes de publicar este bloque:

```sh
cargo fmt --check
cargo clippy --release --locked --all-targets --features experimental-lsm -- -D warnings
cargo test --release --locked --all-targets --features experimental-lsm
cargo build --release --locked --features experimental-lsm
python3 scripts/lsm_electrum_smoke.py --bitcoind /tmp/opencode/bitcoin-31.0/bin/bitcoind --bitcoin-cli /tmp/opencode/bitcoin-31.0/bin/bitcoin-cli --electrum-lite target/release/electrum-lite
python3 scripts/lsm_official_core_smoke.py --electrum-lite target/release/electrum-lite
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

**87 tests con feature y 53 sin feature superados**; formato, Clippy y builds
correctos. El wrapper autenticó firma/checksum de Core 32.1, última estable
observada; rechaza una versión nueva sin validación explícita, sin fallback.
Ambas ejecuciones TCP completas terminaron con:

```text
PASS LSM TCP: negotiation, baseline header chunks/checkpoints, genesis, batches, named params, fragmented frames and limits
PASS LSM TCP: local witness TX/proofs, mempool chains/RBF, confirmation and reorg notifications including unchanged status
PASS LSM TCP: nonblocking diagnostics during Core stall, graceful shutdown/reopen and strict response limits without truncation
PASS LSM TCP: unavailable Core blocks wallet queries even with a complete local index
PASS all owned processes stopped; Core wallet/cookies, LSM generations and temporaries removed
```

Pruebas aisladas offline regtest, no co-sync pública ni interoperabilidad GUI.
Los scripts poseen y limpian ambos procesos; el servicio por sí solo no supervisa
Core. Se conservaron las herramientas Core 31.0 preexistentes y las fuentes del
experimento comparativo aparcado. Builds y herramientas locales de compilación
de este bloque se eliminan al cerrarlo.

## Pendiente antes de activar el instalador LSM

[PENDIENTE] Operaciones CLI, supervisor dedicado y poda LSM con intención durable,
backup verificado y recuperación witness/Merkle tras eliminación real de bloques;
capacidad/bloques grandes y co-sincronización sostenida Signet/Testnet4 que reduzca
retraso mientras Core avanza. Revisar límites KV, representación JSON, auditoría
por replay y coste de anclas RPC por bloque. Después, artefacto desde commit
limpio, política de pareja por hash y bootstrap remoto actualizado.

No se cambia `0.0.1-beta.1` ni el tag original. No hay permiso de poda mainnet,
afirmación de producción ni prueba de rendimiento superior a Core. Las instrucciones
Signet del instalador actual no deben presentarse como ejecución de este LSM.
