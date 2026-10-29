# Electrum Lite — beta 0.0.1

**Versión: `0.0.1-beta.1`, sin cambios.** Servidor Electrum privado en Rust para **mainnet, Signet y Testnet4**, con Bitcoin Core como validador, archivo histórico de bloques completos Zstandard e índices SQLite.

La rama **`main` contiene la beta funcional actual**. La etiqueta `v0.0.1-beta.1` conserva la entrega inicial; no se ha movido ni sustituido. Al mantener la versión, registra también el commit instalado (`git rev-parse HEAD`).

**Experimental, no certificado para fondos reales.** Hay pruebas TCP extremo a extremo en regtest y validación de un prefijo real de Signet/Testnet4; todavía no hay sincronización completa sostenida, prueba GUI con Sparrow ni mediciones en hardware antiguo. **La poda en mainnet está bloqueada**, sin opción insegura para saltarse la protección.

**Rendimiento real:** la co-sincronización Signet del binario `f8fcfb6` mostró retraso creciente (aprox. 5593 → 70 668 bloques), sin pausas de mantenimiento. La meta de seguir/superar a Core **no está conseguida**. [Evidencia y reproducción](docs/benchmarks/signet-cosync-f8fcfb6.md).

## Qué funciona

- Servicio Electrum TCP privado, negociación 1.4, cabeceras y checkpoints, historial, saldo, UTXO, transacciones con witness, pruebas Merkle, fees y difusión.
- Mempool reconciliada por RPC: padres/hijos, alturas 0/-1, RBF, gastos pendientes, suscripciones y notificaciones.
- Sincronización continua, un escritor lógico y respuestas de una generación consistente. No devuelve balances/historiales vacíos cuando el índice no está listo.
- Reorgs dentro del undo retenido (hasta 288 bloques): marcas de intención durables, reversión y nueva rama, sin sobrescribir bloques anteriores.
- SQLite schema 2, commit coordinado, recuperación tras reinicios, auditoría semántica y reconstrucción separada.
- Backup con manifiesto SHA-256, verificación, restauración a directorio nuevo y migración explícita con copia previa.
- Poda manual auditada en redes de prueba, con backup obligatorio y demostración de recuperación histórica después de poda efectiva.

No hay GUI, servidor público, TLS nativo, telemetría ni dependencia de servidores Electrum externos. El TCP solo permite loopback; usa un túnel privado para acceso remoto. **Conservar todo el histórico exige disco: no hay un ahorro total ni un mínimo de RAM demostrado.**

## Instalar

Instalación o actualización del **ejecutable**, sin sudo (Linux x86-64, Python 3.11+ y curl):

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/AlbercoinDev/electrum-lite/main/install.sh | sh
```

La misma línea instala o actualiza **Lite y la última versión estable oficial de Bitcoin Core** dentro de `~/electrum-lite/`. Requiere Python 3.11+, curl, GnuPG y Linux x86-64. Core se autentica con firma de un firmante fijado; Lite confía en GitHub HTTPS y en el hash exacto de la pareja validada. Si la última versión no está validada, se rechaza sin elegir una anterior. Cierra Lite antes de actualizar: el instalador no mata procesos, no hace downgrade y conserva datos y versiones. No toca `~/.bitcoin`. [Detalles](docs/install-linux.md).

```sh
"$HOME/electrum-lite/electrum-lite" --network signet
```

Este único comando inicia el supervisor, su Core dedicado y Lite, con poda coordinada automática. Ctrl+C cierra ambos. Datos bajo `~/electrum-lite/data/signet/{core,lite}/`; Electrum en `127.0.0.1:60601`. El motor sigue siendo SQLite; el rediseño LSM y el rendimiento superior a Core siguen pendientes. Los ejemplos manuales posteriores son para desarrollo, no pasos requeridos por este lanzador.

Para el paquete Linux x86-64 experimental (ejecutable e instalador sin sudo),
consulta [instalación, conexión y uso](docs/install-linux.md). El paquete se
adjunta a la [release existente](https://github.com/AlbercoinDev/electrum-lite/releases/tag/v0.0.1-beta.1),
pero cada paquete identifica su commit de main, **no el código del tag original**.
El tag no se modifica. Para regenerarlo desde una
compilación release verificada: `cargo fetch --locked` y
`python3 scripts/package_linux.py` (Python 3.11+, salida en `dist/`, sin subir archivos).

Entorno probado: **Linux x86-64, Rust 1.99.1 y Bitcoin Core 31.0/32.1**. La pareja publicada Lite `d258034`/Core 32.1 pasó suites aisladas de protocolo, reorg, supervisor y poda real ([evidencia](docs/core-32.1-validation.md)). No se exige AVX2. Otras plataformas/versiones no se han validado.

En Debian/Ubuntu:

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config git ca-certificates curl
```

Instala Rust mediante [rustup oficial](https://rustup.rs/). Después:

```sh
git clone https://github.com/AlbercoinDev/electrum-lite.git
cd electrum-lite
cargo build --release --locked
./target/release/electrum-lite --version
./target/release/electrum-lite --help
```

Toolchain y dependencias quedan fijados en `rust-toolchain.toml` y `Cargo.lock`. Opcionalmente: `cargo install --path . --locked`. Para reemplazar una instalación existente se necesita `--force`: **conserva antes el binario previo y una copia de datos**, especialmente si migras esquema. El paquete Linux publicado es experimental y no está firmado.

## Preparar Core y los datos

Usa un nodo dedicado de laboratorio, RPC por cookie y puertos solo en localhost. Configuración conceptual del Core dedicado:

```ini
server=1
prune=1
txindex=0
```

`prune=1` significa **poda manual**, no automática. La app no cambia la configuración de Core. No apliques estos ajustes a un nodo valioso sin revisar sus backups y consumidores. No expongas RPC a Internet.

Una instalación nueva debe disponer de bloques desde génesis. Si Core ya eliminó histórico y no hay archivo/backup confiable, **no se puede reconstruir ese histórico mágicamente**. Core puede descargar más rápido de lo que indexamos: durante IBD no hay un máximo de disco garantizado.

| Red | `--network` | RPC habitual | Cookie Linux habitual |
|---|---|---|---|
| Mainnet | `mainnet` | 8332 | `~/.bitcoin/.cookie` |
| Signet estándar | `signet` | 38332 | `~/.bitcoin/signet/.cookie` |
| Testnet4 | `testnet4` | 48332 | `~/.bitcoin/testnet4/.cookie` |
| Regtest auxiliar | `regtest` | 18443 | `~/.bitcoin/regtest/.cookie` |

Otro datadir/puerto: pasa `--cookie-file` y `--rpc-url http://127.0.0.1:PUERTO/`. Solo se aceptan IPs loopback, sin credenciales en URL ni redirecciones. Signets personalizadas no están validadas.

Los datos se separan automáticamente en `DATA_ROOT/mainnet`, `DATA_ROOT/signet`, etc. **`--network` debe ir antes del subcomando.**

### Actualizar desde SQLite schema 1

Detén todas las instancias, conserva el binario anterior y ejecuta con el nuevo binario:

```sh
./target/release/electrum-lite --network signet --data-dir ./data \
  migrate-index --backup-dir ./before-schema2
./target/release/electrum-lite --network signet --data-dir ./data inspect
```

El destino del backup debe ser nuevo. Se conserva la copia completa anterior y se migra SQLite en una transacción. No se reescriben bloques `ELBETA01`; schema 2 añade cabeceras, cursor del log de ramas y auditoría de poda. Los archivos sin SQLite se importan directamente al esquema nuevo. Esquemas desconocidos se rechazan. [Actualización, rollback y recuperación](docs/operations.md).

## Arrancar

Verifica primero el nodo:

```sh
./target/release/electrum-lite --network signet check-core \
  --cookie-file "$HOME/.bitcoin/signet/.cookie"
```

Servicio Signet:

```sh
./target/release/electrum-lite --network signet --data-dir ./data serve \
  --cookie-file "$HOME/.bitcoin/signet/.cookie" --listen 127.0.0.1:60601
```

Servicio Testnet4:

```sh
./target/release/electrum-lite --network testnet4 --data-dir ./data serve \
  --cookie-file "$HOME/.bitcoin/testnet4/.cookie" --listen 127.0.0.1:61001
```

Servicio mainnet, **solo para validación experimental y sin poda habilitada**:

```sh
./target/release/electrum-lite --network mainnet --data-dir ./data serve \
  --cookie-file "$HOME/.bitcoin/.cookie" --listen 127.0.0.1:50001
```

El arranque verifica íntegramente archivo, referencias e índices; puede tardar según el histórico. Después sincroniza lotes y reconcilia mempool. Las consultas de cartera solo se habilitan al alcanzar el tip observado de Core, finalizar IBD y obtener una mempool consistente dentro de los presupuestos.

La misma terminal de `serve` muestra cada aproximadamente cinco segundos las alturas observadas de Core e índice, retraso, velocidad entre muestras, altura de poda, antigüedad de la observación y espacio libre. También informa de errores y alerta de poda automática o histórico ausente. Si el worker está ocupado o Core no responde, la actualización puede tardar más; `?` significa que no hay dato disponible. No registra cookies ni consultas de cartera.

`serve` normal **no supervisa ni detiene Core**. En Linux existe `managed-serve`, experimental y solo para un Core dedicado nuevo de Signet/Testnet4/regtest: verifica el índice antes de arrancarlo y solicita la parada de su propio proceso si Electrum Lite falla. Exige `--confirm-dedicated` y **usa internamente `prune=1`**, no `prune=550`. Con `--auto-prune --prune-backup-root DIRECTORIO_EXISTENTE` el supervisor automatiza ventanas de auditoría, backup verificable y poda del prefijo durable; sin esas opciones no poda. La poda independiente de Core no está validada para preservar el histórico. Ver [supervisión y límites](docs/operations.md#supervisor-linux-experimental).

En otra terminal:

```sh
./target/release/electrum-lite --network signet health --address 127.0.0.1:60601
```

Comprueba `ready`, tip local/Core, IBD, conexiones/red activa, espacio, tamaños de archivo/base y presupuesto de mempool. `ready` significa coherencia con el **Core observado**, no conexión garantizada al resto de Bitcoin. Core sin pares o con red desactivada puede estar atrasado. Ctrl+C/SIGTERM detiene el servicio; no ejecuta poda automática.

Para observar progreso continuamente: `./target/release/electrum-lite --network signet monitor --address 127.0.0.1:60601`. Emite JSON cada cinco segundos con velocidad entre muestras, retraso frente al Core observado, espacio y alertas de poda; `--interval` cambia el periodo. Las observaciones pueden quedar desactualizadas y no sustituyen una ventana de mantenimiento segura. No hay garantía medida de alcanzar a Core durante IBD.

La sincronización agrupa hasta 64 bloques y 16 MB serializados por commit durable (el daemon procesa hasta 256 por ciclo). El archivo se sincroniza antes de confirmar SQLite. Una cola incompleta sigue fallando de forma segura; no se trunca automáticamente. Este cambio no demuestra que la indexación pueda seguir a Core: se necesitan pruebas sostenidas y mediciones representativas.

Un pool fijo de 2–8 workers descarga grupos de ocho bloques mediante dos peticiones RPC por lote (hashes y bloques), con una cola de un grupo por worker, y se solapan descargas con commits mediante el único escritor; las sentencias SQL se reutilizan durante el lote. Un overlay temporal por bloque resuelve gastos en orden y aplica el delta UTXO final, conservando undo e historial; BIP30 mantiene su ruta específica. Además del lote serializado acotado, hay memoria transitoria de los bloques y sus respuestas JSON/hex; no es un límite total de RSS. Un error RPC interrumpe el grupo, no se interpreta como una reorganización. `health` incluye `sync_metrics`: total, espera, commit, comprobación de rama (`anchor_ms`) y tiempos RPC sumados entre workers (`rpc_*_worker_us`, no tiempo de pared). [Perfil real en disco y límites pendientes](docs/benchmarks/signet-rpc-writer-profile.md).

Para Sparrow, selecciona la misma red y **Private Electrum server**, host `127.0.0.1` y el puerto elegido. TCP sin SSL solo en loopback/túnel privado; nunca transportes abierto por Internet. La interfaz con Sparrow real sigue pendiente de validación aquí: no se ha instalado ni probado su GUI. Ver [perfil de protocolo y pruebas](docs/protocol.md).

## Herramientas y límites

```sh
# Lote manual; continuar repitiendo el comando. No omite el prefijo desde génesis.
./target/release/electrum-lite --network signet --data-dir ./data sync \
  --cookie-file "$HOME/.bitcoin/signet/.cookie" --max-blocks 100
./target/release/electrum-lite --network signet --data-dir ./data inspect
./target/release/electrum-lite --network signet --data-dir ./data audit
```

También hay `history SCRIPTHASH`, `transaction TXID [--height ALTURA]`, `rebuild-index --output NUEVO.sqlite`, `backup`, `verify-backup`, `restore`, `recover-chain` y `prune`. Las herramientas de archivos requieren el servicio detenido por su bloqueo exclusivo. Las respuestas offline representan el histórico local, que puede estar incompleto o pertenecer a una rama que ya cambió en Core.

- SQLite usa una caché de páginas sugerida de 64 MiB (no preasignación ni límite duro); no es un límite total de RAM.
- Mempool: 50 000 transacciones, 64 MiB serializados y 500 000 entradas input/output por defecto; `--max-mempool-transactions`, `--max-mempool-mib`, `--max-mempool-entries`. Se calienta la caché en lotes de hasta 500 TX/ciclo. Si no cabe, no se sirve un historial parcial: queda no listo.
- Transporte: 16 conexiones y 1 000 suscripciones por conexión por defecto, configurables con `--max-connections`/`--max-subscriptions`. Lotes JSON-RPC de hasta 64, 200 peticiones por 10 s/conexión, mensaje hasta 8,1 MB, respuesta hasta 16,2 MB, idle 120 s y escritura 15 s.
- `--max-query-results` limita a 10 000 entradas por script y produce error, nunca truncado silencioso. Checkpoints de cabeceras hasta altura 2 000 000; chunks de 2 016.
- `--min-free-mib` de `serve` y mantenimiento: 512 MiB por defecto. No garantiza espacio suficiente para un backup/auditoría grande ni supervisa otro filesystem de Core.
- `--max-index-transactions` y `--max-index-entries` son topes acumulados de seguridad en disco durante importación/nueva indexación, no de RAM; sus defaults permiten indexación extensa.

## Backup y poda

### Poda automática coordinada (experimental, Linux)

```sh
mkdir -p "$HOME/electrum-prune-backups"
~/.local/bin/electrum-lite --network signet --data-dir "$HOME/electrum-data" managed-serve \
  --bitcoind /ruta/real/a/bitcoind --core-dir "$HOME/electrum-core-dedicado" \
  --rpc-port 38342 --listen 127.0.0.1:60601 --confirm-dedicated \
  --auto-prune --prune-backup-root "$HOME/electrum-prune-backups"
```

Edita la ruta de `bitcoind`. La raíz de Core debe ser **nueva** o creada antes por este supervisor; no adopta tu nodo existente. No ejecutes otros escritores de cadena contra el Core dedicado. Mainnet está bloqueada.

Cada 2016 nuevas alturas elegibles (configurable con `--auto-prune-interval-blocks`, mínimo 288), el supervisor detiene su daemon, desactiva temporalmente P2P, verifica el prefijo durable, audita, crea/verifica un backup completo nuevo y solicita poda dejando 288 bloques de margen. Reinicia el daemon y restaura P2P solo tras éxito; ante error detiene el Core propio. Los backups nunca se borran automáticamente: reserva espacio para las copias completas y revisa capacidad. No es un objetivo de tamaño de disco ni `prune=550`.

El cursor de poda persiste en SQLite y evita repetir el mismo prefijo tras reinicios. Una petición incompleta exige revisión del operador; nunca se borra para continuar. La ventana interrumpe consultas/conexiones y pausa la descarga de Core; **no cuenta como evidencia de que Lite indexe más rápido**. `health` del daemon conserva `automatic_pruning:false` porque la automatización pertenece al supervisor; sus logs muestran petición, resultado real y backup. Véase [operación](docs/operations.md#poda-automática-coordinada).


```sh
# Con el servicio detenido. Los destinos deben ser nuevos y estar fuera del origen.
./target/release/electrum-lite --network signet --data-dir ./data backup --output ./backup-signet
./target/release/electrum-lite --network signet verify-backup --directory ./backup-signet
./target/release/electrum-lite --network signet restore \
  --backup-dir ./backup-signet --output-root ./restored-data
./target/release/electrum-lite --network signet --data-dir ./restored-data inspect
```

La poda manual es **mantenimiento explícito en redes de prueba**. Existe además automatización opt-in en el supervisor dedicado, descrita debajo. Requiere Core con poda manual, red de Core desactivada explícitamente por el operador, ningún otro escritor de cadena, prefijo indexado coincidente, auditoría semántica, margen de 288 bloques y backup nuevo. Puede podar un prefijo durable durante bootstrap aunque Core vaya por delante.

```sh
# Solo sobre el nodo de pruebas dedicado. Detén primero Electrum Lite.
bitcoin-cli -signet setnetworkactive false
./target/release/electrum-lite --network signet --data-dir ./data prune \
  --cookie-file "$HOME/.bitcoin/signet/.cookie" \
  --confirm --backup-dir ./before-prune-signet
# Revisa el resultado y vuelve a activar la red de Core cuando sea seguro.
bitcoin-cli -signet setnetworkactive true
```

Registra altura solicitada/real, verifica indisponibilidad del bloque en Core y conserva auditoría. Ante fallo tras solicitar poda, se bloquean nuevas peticiones hasta revisión del operador. No borres auditorías para forzarlas. Nunca ejecutes pruebas destructivas contra nodos compartidos/valiosos. [Procedimientos completos](docs/operations.md).

## Pruebas y estado del plan

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

Con Bitcoin Core 31.0 instalado y verificado:

```sh
BITCOIND=/ruta/a/bitcoind BITCOINCLI=/ruta/a/bitcoin-cli \
  python3 scripts/core_smoke.py
BITCOIND=/ruta/a/bitcoind python3 scripts/prune_smoke.py
BITCOIND=/ruta/a/bitcoind BITCOINCLI=/ruta/a/bitcoin-cli \
ELECTRUM_LITE=./target/release/electrum-lite \
  python3 scripts/bench_sync.py --blocks 120 --txs-per-block 200 --rounds 2
```

Crean únicamente nodos temporales aislados. `bench_sync.py` genera una cadena regtest densa, mide la ingesta completa de Electrum Lite por RPC y, con los mismos bloques, un `-reindex-chainstate` de Core: es una comparación relativa en una sola máquina con caché caliente, no una promesa de mainnet. La segunda prueba genera unos 300 MiB de datos muy compresibles, poda realmente Core y verifica TX/Merkle por Electrum sin ese bloque en Core: **no es un benchmark de compresión**. Las pruebas públicas acotadas son opcionales: `scripts/live_smoke.py --network signet` o `--network testnet4`; implican conexiones P2P y descarga limitada.

Ver [evidencias y pendientes](docs/beta-0.0.1.md), [operación/systemd](docs/operations.md), [protocolo](docs/protocol.md), [decisión SQLite](docs/decisions/0001-sqlite.md) y [especificación](ELECTRUM_LITE_ENGINE_v1.0.md). GitHub Actions ejecuta tests, integración TCP y poda real regtest. No sustituye sincronización sostenida, ensayo con Sparrow ni pruebas de pérdida real de energía.

## Licencia

La licencia propia sigue pendiente de decisión. Un repositorio público no concede por sí solo una licencia general de reutilización. Las dependencias conservan sus licencias: [notas](docs/dependencies.md).
