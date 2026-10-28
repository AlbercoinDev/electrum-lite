# Electrum Lite — beta 0.0.1

**Versión técnica: `0.0.1-beta.1`.** CLI experimental en Rust para archivar bloques Bitcoin completos con Zstandard y reconstruir un índice de historial/UTXO en memoria.

**Todavía no es un servidor Electrum:** no acepta conexiones de Sparrow, no sigue mempool, no tiene base transaccional persistente, undo ni controlador de poda. No usar como servicio de cartera ni para autorizar la eliminación de bloques de Core.

## Funciones disponibles

- Selección explícita de **mainnet, Signet, Testnet4** y regtest auxiliar, con directorios separados y comprobación de red/génesis contra Core.
- RPC local autenticado por cookie; el secreto no se guarda en el archivo ni se incluye en mensajes RPC de error.
- Descarga secuencial desde génesis, comprobación de continuidad, raíz Merkle y compromiso witness.
- Archivo append-only de segmentos de hasta 64 MiB, frames Zstandard por bloque, checksum, `fsync` y bloqueo exclusivo.
- Reanudación mediante verificación y replay completo del archivo; rechazo de corrupción, colas incompletas y discrepancias con la cadena activa de Core.
- Consultas offline de historial/UTXO por scripthash Electrum y recuperación exacta de transacciones, incluido witness.

Bitcoin Core conserva la responsabilidad de validar consenso. Las comprobaciones del archivo no convierten esta app en un validador independiente. No hay ahorro de disco ni consumo mínimo de RAM demostrados.

## Instalación desde código

Entorno probado: Linux x86-64, Rust **1.99.1**, Bitcoin Core **31.0**. Otros sistemas y versiones no se han validado. No se exige AVX2. Se necesita un compilador C para Zstandard/secp256k1.

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

`rust-toolchain.toml` fija el toolchain y `Cargo.lock` fija las dependencias. Opcionalmente instala la CLI en `~/.cargo/bin` con `cargo install --path . --locked`. No hay instalador ni binarios precompilados oficiales en esta beta.

## Preparar Bitcoin Core

Usa un Core local con `server=1`, autenticación por cookie y RPC solo en loopback. La app acepta únicamente URLs HTTP con IP de loopback y sin credenciales en URL; no sigue redirecciones. Para Core remoto, usa un túnel seguro con extremo local.

La app **no cambia la configuración de Core ni ejecuta poda**. Para una instalación nueva destinada a poda manual, el diseño requiere `prune=1` y `txindex=0`, pero **esta beta no debe usarse para decidir cuándo podar**. La poda automática puede eliminar datos antes de archivarlos. Un Core ya podado no permite completar desde cero el histórico faltante.

No modifiques la configuración ni los datos de tu nodo habitual para experimentar. Usa nodos de laboratorio independientes; nunca expongas RPC a Internet.

| Red | `--network` | Puerto RPC habitual | Cookie habitual en Linux |
|---|---|---|---|
| Mainnet | `mainnet` | 8332 | `~/.bitcoin/.cookie` |
| Signet | `signet` | 38332 | `~/.bitcoin/signet/.cookie` |
| Testnet4 | `testnet4` | 48332 | `~/.bitcoin/testnet4/.cookie` |
| Regtest auxiliar | `regtest` | 18443 | `~/.bitcoin/regtest/.cookie` |

Si Core usa otro datadir/puerto, especifica su cookie y `--rpc-url http://127.0.0.1:PUERTO/`. Los ejemplos Signet se refieren a la red estándar; signets personalizadas no están validadas.

## Uso

`--network` es obligatorio y debe ir **antes del subcomando**. No hay una red predeterminada.

### Verificar Core sin descargar bloques

```sh
./target/release/electrum-lite --network signet check-core \
  --cookie-file "$HOME/.bitcoin/signet/.cookie"

./target/release/electrum-lite --network testnet4 check-core \
  --cookie-file "$HOME/.bitcoin/testnet4/.cookie"

./target/release/electrum-lite --network mainnet check-core \
  --cookie-file "$HOME/.bitcoin/.cookie"
```

### Archivar un lote limitado

```sh
./target/release/electrum-lite --network signet --data-dir ./data sync \
  --cookie-file "$HOME/.bitcoin/signet/.cookie" --max-blocks 100
```

Repetir el comando verifica lo existente y continúa desde el siguiente bloque. `--to-height N` establece una altura máxima adicional; no omite el procesamiento desde génesis. `--max-blocks` limita el lote (100 por defecto), no el tamaño del archivo total. Para Testnet4 cambia red y cookie; mainnet usa la misma interfaz, pero no está validada una sincronización completa.

Los datos quedan en `./data/signet/`, `./data/testnet4/`, etc. **El archivo es un prefijo histórico que puede estar incompleto o atrasado, no una prueba de sincronización con la punta actual.** Cada invocación reconstruye los índices en memoria. El límite inicial es 100 000 transacciones archivadas acumuladas (`--max-index-transactions`); al alcanzarlo se detiene. Aumentarlo consume más RAM y no habilita una modalidad de bajo consumo para toda mainnet.

Además, `--max-index-entries` limita la suma de UTXO y ocurrencias de historial a 500 000 por defecto. Ambos límites se comprueban antes de aplicar y archivar un bloque, también durante replay. Son límites de elementos, no una garantía exacta de RAM en bytes.

### Verificar y consultar offline

```sh
./target/release/electrum-lite --network signet inspect
```

```sh
# Sustituye los argumentos por hashes hexadecimales reales.
./target/release/electrum-lite --network signet history SCRIPT_HASH
./target/release/electrum-lite --network signet transaction TXID
./target/release/electrum-lite --network mainnet transaction TXID --height ALTURA
```

El scripthash es SHA-256 de scriptPubKey con bytes invertidos, como en Electrum. Las respuestas incluyen solo los bloques archivados: no mempool, ni saldo gastable ajustado por madurez de coinbase. `--height` desambigua TXID históricos duplicados. No hay todavía comando público de pruebas Merkle ni servidor JSON-RPC Electrum.

## Seguridad, backups y recuperación

- No hay rollback automático: una reorg detectada al sincronizar bloquea la continuación. Conserva el archivo y reconstruye en **otro directorio**, usando un Core que aún disponga de todo el histórico necesario.
- Una cola incompleta o checksum incorrecto causa error. No se trunca ni repara automáticamente; no borres archivos para intentar continuar. Restaura una copia verificada o reconstruye aparte.
- Para backup, termina la CLI y copia todo el subdirectorio de la red. Verifica la copia con `inspect` desde otro directorio raíz. Esto no es una suite de recuperación certificada ni autoriza poda.
- No se prueba ni se promete resistencia completa ante pérdida de energía: las pruebas de cola truncada/corrupción son parciales; la fase de fault injection completa sigue pendiente.
- No ejecutes varias instancias sobre el mismo archivo. No mezcles redes ni compartas directorios con Core. No uses root.
- No hay garantía de límite de disco durante IBD; Core puede descargar más rápido de lo que esta CLI archiva.
- El formato `ELBETA01` es experimental. Cambios futuros exigirán migración explícita o reconstrucción, nunca actualización silenciosa.

## Pruebas

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

Para la integración aislada, instala Bitcoin Core 31.0 desde su [distribución oficial](https://bitcoincore.org/bin/bitcoin-core-31.0/) y verifica la descarga siguiendo sus instrucciones. Después:

```sh
BITCOIND=/ruta/a/bitcoind \
BITCOINCLI=/ruta/a/bitcoin-cli \
ELECTRUM_LITE=./target/release/electrum-lite \
python3 scripts/core_smoke.py
```

El script crea nodos temporales sin pares públicos y los cierra al terminar. Verifica génesis/red en mainnet, Signet y Testnet4; en regtest genera 106 bloques, realiza un gasto SegWit, verifica recuperación de la transacción, reanudación y rechazo de reorg. **No equivale a sincronización real de Signet/Testnet4 ni a validación completa de mainnet.** Estas pruebas sostenidas siguen pendientes.

Ver [resultados y límites de la beta](docs/beta-0.0.1.md) y [especificación](ELECTRUM_LITE_ENGINE_v1.0.md). GitHub Actions ejecuta comprobaciones Rust y la integración aislada.

## Licencia

La licencia del proyecto sigue pendiente de elección. Publicar el repositorio no concede por sí solo una licencia general de reutilización. Las dependencias mantienen sus propias licencias; ver [notas de dependencias](docs/dependencies.md).
