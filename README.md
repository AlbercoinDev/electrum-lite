# Electrum Lite — beta 0.0.1

**Versión técnica: `0.0.1-beta.1` (sin incremento de versión).** CLI experimental en Rust para archivar bloques Bitcoin completos con Zstandard y mantener historial/UTXO en SQLite. Esta continuación está en la rama `main`; la etiqueta publicada originalmente conserva su contenido y no se mueve.

**Todavía no es un servidor Electrum:** no acepta conexiones de Sparrow ni sigue mempool. Ya hay base transaccional y una primitiva interna de undo, pero no recuperación automática de reorgs ni controlador de poda. No usar como servicio de cartera ni para autorizar la eliminación de bloques de Core.

## Funciones disponibles

- Selección explícita de **mainnet, Signet, Testnet4** y regtest auxiliar, con directorios separados y comprobación de red/génesis contra Core.
- RPC local autenticado por cookie; el secreto no se guarda en el archivo ni se incluye en mensajes RPC de error.
- Descarga secuencial desde génesis, comprobación de continuidad, raíz Merkle y compromiso witness.
- Archivo append-only de segmentos de hasta 64 MiB, frames Zstandard por bloque, checksum, `fsync` y bloqueo exclusivo.
- Índices persistentes en **una base SQLite**, esquema 1, journal DELETE y sincronización EXTRA. Caché de páginas sugerida de 8 MiB; no es un límite total de RSS.
- Commit coordinado: preparar cambios SQL, sincronizar el bloque archivado y confirmar referencias/UTXO/historial/undo en una transacción.
- Reanudación con verificación completa del archivo, sin reconstruir los índices ya confirmados en RAM; recuperación idempotente de registros completos archivados aún no indexados. Colas incompletas, corrupción y generaciones mezcladas causan error.
- Consultas SQL de historial/UTXO por scripthash Electrum y recuperación exacta de transacciones por ubicación en archivo, incluido witness.
- Undo interno de hasta 288 bloques y reconstrucción no destructiva a una base separada. La CLI aún rechaza reorgs en vez de revertirlas automáticamente.

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

Los datos quedan en `./data/signet/`, `./data/testnet4/`, etc., junto a `index.sqlite`. **El archivo es un prefijo histórico que puede estar incompleto o atrasado, no una prueba de sincronización con la punta actual.** Cada invocación comprueba integridad SQLite, claves foráneas, contadores y todos los bloques/referencias (E/S proporcional al histórico), pero no vuelve a aplicar los ya indexados ni carga todo el UTXO/historial en memoria. Esto no sustituye una auditoría semántica completa de saldos reconstruidos desde génesis.

Los topes de seguridad durante nueva indexación/importación siguen siendo 100 000 transacciones (`--max-index-transactions`) y 500 000 entradas UTXO + historial (`--max-index-entries`). Se comprueban antes de archivar nuevos bloques; una cola completa ya archivada requiere límites suficientes para importarla. Son topes de crecimiento, no un presupuesto de RAM ni una validación de sincronización completa de mainnet.

`--max-query-results` limita a 10 000 las entradas de historial o UTXO devueltas por script; superar el límite produce error sin devolver un historial parcial. Las consultas sobre índices ya existentes no vuelven a aplicar los topes de importación.

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

El scripthash es SHA-256 de scriptPubKey con bytes invertidos, como en Electrum; la base conserva el hash binario canónico. Las respuestas incluyen solo los bloques archivados: no mempool, ni saldo gastable ajustado por madurez de coinbase. `--height` desambigua TXID históricos duplicados. No hay todavía comando público de pruebas Merkle ni servidor JSON-RPC Electrum.

`indexed_tip` y `durable_tip` describen la generación local confirmada. **No significan que el índice esté actualizado contra Core ni que la poda esté autorizada.**

### Actualizar desde la beta inicial

Con todos los procesos detenidos, haz una copia completa del directorio de red. Conserva el binario anterior y esa copia para rollback. El formato de bloques `ELBETA01` no cambia: al ejecutar esta continuación sobre un archivo sin `index.sqlite`, se crea el esquema 1 y se importa el histórico desde génesis, sin eliminar ni reescribir bloques. Es necesario espacio adicional para base, journal y backup.

La creación/importación es reanudable por bloque. Una base con esquema desconocido o dañada se rechaza; no se migra ni sobrescribe silenciosamente. El binario anterior ignora SQLite y conserva sus límites de RAM; para volver a él usa la copia previa en otro directorio, no mezcles archivos de distintos momentos.

## Seguridad, backups y recuperación

- No hay rollback automático: una reorg detectada al sincronizar bloquea la continuación. Conserva el archivo y reconstruye en **otro directorio**, usando un Core que aún disponga de todo el histórico necesario.
- Una cola incompleta o checksum incorrecto causa error. No se trunca ni repara automáticamente; no borres archivos para intentar continuar. Restaura una copia verificada o reconstruye aparte.
- Para backup, termina todas las CLI y copia **todo** el subdirectorio de red: archivo, SQLite y cualquier journal presente después de un fallo. No copies solo la base mientras hay un escritor ni mezcles generaciones. Verifica la copia con `inspect` desde otro directorio raíz. Se prueba restauración de una copia cerrada en regtest; no equivale a una certificación de recuperación ni autoriza poda.
- Se prueban salidas abruptas de subprocesos antes del append, después de sincronizar archivo, con referencias SQL preparadas y después del commit; también SQLITE_FULL simulado con límite de páginas y un error de append. No equivalen a pérdida real de energía, disco físico lleno ni una campaña exhaustiva de fallos.
- No ejecutes varias instancias sobre el mismo archivo. No mezcles redes ni compartas directorios con Core. No uses root.
- No hay garantía de límite de disco durante IBD; Core puede descargar más rápido de lo que esta CLI archiva.
- El formato `ELBETA01` y el esquema SQLite 1 son experimentales. Cambios futuros exigirán migración explícita o reconstrucción, nunca actualización silenciosa.

### Reconstruir sin sobrescribir datos

`rebuild-index --output NUEVO.sqlite` verifica el archivo y genera una base nueva; exige extensión `.sqlite` y rechaza destinos existentes de forma atómica. Un fallo puede dejar una base parcial en ese destino, que debe conservarse para diagnóstico; elige otro nombre para otro intento. El comando no sustituye automáticamente la base activa ni repara una rama reorganizada.

Para trabajar con una reconstrucción, usa un directorio de recuperación nuevo. Ejemplo Signet, **con todas las CLI detenidas y espacio para otra copia del archivo**:

```sh
recovery="$(mktemp -d ./recovery-XXXXXX)"
mkdir "$recovery/signet"
cp -p ./data/signet/blocks-*.elb "$recovery/signet/"
./target/release/electrum-lite --network signet --data-dir ./data \
  rebuild-index --output "$recovery/signet/index.sqlite"
./target/release/electrum-lite --network signet --data-dir "$recovery" inspect
```

Si verificas la copia, puedes usar ese nuevo `--data-dir`. El original queda intacto para rollback. No sustituyas un SQLite dejando journals antiguos a su lado ni elimines una cola corrupta para forzar el arranque.

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

El script crea nodos temporales sin pares públicos y los cierra al terminar. Verifica génesis/red en mainnet, Signet y Testnet4; en regtest genera 106 bloques, realiza un gasto SegWit, verifica transacción, índice persistente, reanudación, reconstrucción separada, restauración de copia cerrada y rechazo de reorg. **No equivale a sincronización real de Signet/Testnet4 ni a validación completa de mainnet.** Estas pruebas sostenidas siguen pendientes.

La elección de SQLite y el microexperimento reproducible frente a redb están en [ADR-001](docs/decisions/0001-sqlite.md). redb es dependencia de desarrollo del experimento, no un segundo backend de producción.

Ver [resultados y límites de la beta](docs/beta-0.0.1.md) y [especificación](ELECTRUM_LITE_ENGINE_v1.0.md). GitHub Actions ejecuta comprobaciones Rust y la integración aislada.

## Licencia

La licencia del proyecto sigue pendiente de elección. Publicar el repositorio no concede por sí solo una licencia general de reutilización. Las dependencias mantienen sus propias licencias; ver [notas de dependencias](docs/dependencies.md).
