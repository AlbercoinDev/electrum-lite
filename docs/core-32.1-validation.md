# Core 32.1: validación preliminar, 2026-10-29

[CONFIRMADO] La API oficial `bitcoin/bitcoin/releases/latest` devolvió `v32.1`,
estable y no borrador. Descarga desde `bitcoincore.org/bin/bitcoin-core-32.1/`.
Los SHA256SUMS se autenticaron mediante la clave primaria de fanquake fijada
en `scripts/core_verify.py`. El tarball Linux x86-64 pasó su checksum autenticado.

Prueba real aislada, sin Lite ni descarga de cadena:

```text
Bitcoin Core daemon version v32.1.0 bitcoind
PASS isolated cookie RPC, regtest identity, manual pruning, P2P offline
PASS clean Core shutdown; binary and node data removed on context exit
PASS temporary root removed: True
```

Se arrancó `daemon=0`, `server=1`, `prune=1`, `txindex=0`, `disablewallet=1`,
con P2P deshabilitado y puerto RPC efímero. Se consultaron getblockchaininfo y
getnetworkinfo; se confirmó salida cero tras SIGTERM. Todo residió en un
TemporaryDirectory propio en `/tmp/opencode`; binarios y datos se eliminaron.

[CONFIRMADO] Posteriormente se ejecutó la suite completa con el artefacto
Lite `d258034`, SHA256
`452439e6fe62cd8bb5e43cfb1b0cdb08def3c24b1217fd94a77bf163febeb3e6`:

```sh
python3 scripts/validate_core_bundle.py \
  --package dist/electrum-lite-0.0.1-beta.1-linux-x86_64-d258034.tar.gz --prune
```

Resultado del 2026-10-29:

```text
PASS launcher_smoke.py
PASS core_smoke.py
PASS managed_smoke.py
Generated 600/600 large regtest blocks
PASS AUTOMATIC PRUNING: owned supervisor, verified backup, real Core deletion, local witness TX/Merkle, restart cursor
PASS prune_smoke.py
PASS validation binaries and all node data removed
```

Incluye identidad en mainnet/Signet/Testnet4/regtest sin pares públicos,
Electrum TCP, mempool/RBF, suscripciones, reorg, recuperación profunda preservando
origen y fallos del supervisor. El lanzador mantuvo exclusión de actualización
durante exec y confirmó cierre de los hijos con salida cero.

Se autoriza únicamente esta pareja exacta en `VALIDATED_PAIRS`, no todos los
binarios que compartan la versión beta. No hay fallback silencioso a Core 31.
[PENDIENTE] Sincronización pública completa, capacidad sostenida, nuevo backend
LSM y despliegue de producción no están demostrados por estas pruebas.

[CONFIRMADO] Los tests offline de bundle cubren activación por manifiesto,
actualización, idempotencia, preservación de datos y rechazo de enlaces del tar.
Los tests de política comprueban exclusión mutua actualización/ejecución y
rechazo de metadatos de versiones no estables. El bootstrap público todavía
no utiliza este flujo. El lanzador nuevo mantiene los locks mediante exec;
su distribución conjunta en el bootstrap sigue pendiente.
