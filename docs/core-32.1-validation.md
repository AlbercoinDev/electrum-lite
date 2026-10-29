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
no utilizaba este flujo en la validación inicial. El lanzador nuevo mantiene
los locks mediante exec.

## Publicación y bootstrap remoto

[CONFIRMADO] Se publicó `8bd972f2dee74c9db24bc828faa14a079705d545` en main.
Se ejecutó la línea pública de README con HOME dentro de un TemporaryDirectory
propio, no importando el instalador local. El bootstrap informó ese commit
exacto como origen. Resultado real:

```text
PASS remote one-line bootstrap installs under isolated HOME
PASS public installed launcher starts dedicated Signet Core (offline)
PASS public launcher confirms clean Core/Lite exit
PASS remote one-line update is idempotent, preserves data, removes staging
PASS isolated HOME, binaries and node data removed: True
```

Se ejecutó el comando instalado `electrum-lite --network signet --offline`;
se verificó `getblockchaininfo.chain=signet` por el bitcoin-cli instalado.
SIGTERM al lanzador confirmó salida cero y desaparición de cookie Core.
Una segunda ejecución de la línea pública preservó el manifiesto y un archivo
testigo dentro de data; tmp quedó vacío. No se descargó cadena pública ni se
afirma sincronización Signet completa. El tag original no se movió y el paquete
Lite sigue identificando el commit real `d258034`, no el tag original.

Pruebas Python en el árbol de trabajo: 27 tests superados (incluyen el test
pendiente de co-sync, cuyo cambio fuente NO se publicó con este bloque).
Shell `sh -n install.sh` y `git diff --check` sin errores. Este bloque modifica
Python/shell/documentación; no recompila ni atribuye cambios Rust al paquete.
