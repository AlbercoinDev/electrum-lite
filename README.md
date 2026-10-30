# Electrum Lite — reescritura thin experimental

**Versión `0.0.1-beta.1`.** Servidor Electrum en Rust construido desde cero:
Core valida, RocksDB conserva índices y los bloques históricos ausentes se obtienen
completos por P2P. Tor opcional. Sin APIs públicas ni servidores Electrum externos.

**Solo pruebas iniciales, sin fondos reales.** No está demostrada la velocidad de
IBD ni la capacidad mainnet. El supervisor de Core y la poda coordinada son experimentales.
No usar `prune=550` automático: usar un **Core nuevo dedicado con `prune=1`**.
No se ha probado una GUI de cartera; el smoke comprueba el protocolo TCP.
Diseño, estado y límites: [docs/rewrite.md](docs/rewrite.md).

## 1. Instalar desde cero (Debian/Ubuntu, Linux x86-64)

### Binario listo para instalar o actualizar

Instala `curl`, `coreutils` y `libstdc++` con el gestor de paquetes de tu
distribución. En Debian/Ubuntu:

```sh
sudo apt-get update && sudo apt-get install -y curl coreutils libstdc++6
```

Después, esta línea instala o actualiza el binario en `~/.local/bin`:

```sh
curl -fsSL https://raw.githubusercontent.com/AlbercoinDev/electrum-lite/main/scripts/install-release.sh -o /tmp/electrum-lite-installer.sh && sh /tmp/electrum-lite-installer.sh
```

El instalador comprueba SHA256 contra el valor fijado en el script, comprueba que
el binario arranca y conserva el ejecutable anterior con un nombre único. No toca
datos, servicios ni Bitcoin Core. El checksum detecta errores o sustituciones del
archivo, pero **no es una firma**: el script y el checksum se distribuyen mediante
GitHub, por lo que dependen de la confianza en esa cuenta y en HTTPS. Puedes leer
el script descargado antes de ejecutarlo. Esta compilación Linux x86-64 depende
de glibc/libstdc++ compatibles con la máquina de compilación; no se garantiza en
distribuciones antiguas. Bitcoin Core se instala por separado, como se explica abajo.

```sh
export PATH="$HOME/.local/bin:$PATH"
electrum-lite --version
```

### Alternativa: compilar desde fuentes

```sh
sudo apt-get update
sudo apt-get install -y build-essential clang libclang-dev pkg-config git curl python3
```

Instala Rust con el instalador oficial de https://rustup.rs. Para descargarlo
y revisarlo antes de ejecutar:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o rustup-init.sh
sh rustup-init.sh --profile minimal
. "$HOME/.cargo/env"
```

El repositorio fija Rust en `rust-toolchain.toml`; rustup instala esa versión al
compilar. RocksDB se compila desde fuentes y necesita C++, libclang, espacio y
varios minutos. Con poca RAM usa `CARGO_BUILD_JOBS=2`.

Si aún no tienes el repositorio:

```sh
git clone https://github.com/AlbercoinDev/electrum-lite.git
cd electrum-lite
```

En la carpeta del repositorio ejecuta:

```sh
CARGO_BUILD_JOBS=2 sh install.sh
export PATH="$HOME/.local/bin:$PATH"
electrum-lite --version
```

No instala servicios, no inicia Core y no modifica nodos existentes. El binario
previo se guarda como `electrum-lite.previous` al reinstalar.

### Instalar esta versión local sin recompilar

`python3 scripts/package.py` genera `dist/electrum-lite-thin-managed-experimental-linux-x86_64.tar.gz` con el binario Linux
x86-64, los fuentes y las instrucciones, sin datos de nodos ni `.git`.
Para instalarlo en una máquina compatible (glibc/libstdc++):

```sh
sha256sum -c electrum-lite-thin-managed-experimental-linux-x86_64.tar.gz.sha256
mkdir electrum-lite-instalacion-nueva
tar -xzf electrum-lite-thin-managed-experimental-linux-x86_64.tar.gz -C electrum-lite-instalacion-nueva
cd electrum-lite-instalacion-nueva/electrum-lite-thin-experimental
mkdir -p "$HOME/.local/bin"
install -m 755 bin/electrum-lite "$HOME/.local/bin/electrum-lite"
"$HOME/.local/bin/electrum-lite" --version
```

El checksum detecta cambios respecto al paquete local; no es una firma de autoría
ni una release oficial. Guarda una copia del binario anterior antes de sustituirlo.

## 2. Instalar Bitcoin Core para las pruebas

Descarga Bitcoin Core desde https://bitcoincore.org/en/download/ y verifica sus
SHA256SUMS **y firmas** con las instrucciones oficiales. Extrae el paquete y añade
su `bin` al PATH. El smoke se ejecuta con Core 31.0 en este entorno; no se descarga
ni se confía automáticamente en otro ejecutable.

```sh
bitcoind --version
bitcoin-cli --version
```

## 3. Comprobar automáticamente que funciona

```sh
python3 scripts/smoke_regtest.py \
  --bitcoind /ruta/bitcoin/bin/bitcoind \
  --bitcoin-cli /ruta/bitcoin/bin/bitcoin-cli \
  --electrum-lite target/release/electrum-lite
```

Crea **su propio regtest offline temporal**, nunca usa tu nodo, no hace poda física
y lo apaga al terminar. Comprueba sincronización, mempool, saldo, UTXO, transacción,
Merkle, reorg, reinicio y backup. Simula un bloque ausente en RPC y verifica su
recuperación desde un peer P2P local y la caché. Eso **no** es una prueba de poda
real ni de disponibilidad de peers públicos.

## 4. Probar tú con una cartera

Crea un entorno nuevo (la ruta NO debe existir):

```sh
python3 scripts/devnet.py --root "$HOME/electrum-lite-prueba-1"
```

El script crea Core regtest offline con 101 bloques y muestra los comandos exactos
para arrancar Lite, enviar monedas ficticias, minar y apagar Core. Usa puertos
19443/19444; puedes elegir otros con `--rpcport` y `--port`.

Ejecuta el comando `electrum-lite ... serve` que imprime. En otra terminal inicia
Electrum en **regtest**, con cartera nueva sin fondos:

```sh
electrum --regtest --oneserver --server 127.0.0.1:50001:t
```

También puedes comprobar el servidor sin instalar una cartera:

```sh
python3 scripts/query.py server.health
python3 scripts/query.py blockchain.headers.subscribe
```

Copia una dirección de recepción y úsala en el comando `sendtoaddress` impreso.
Verás saldo sin confirmar; mina un bloque para confirmarlo. Detén Lite con Ctrl+C
y vuelve a arrancarlo para comprobar persistencia. El script no borra tu entorno.
La compatibilidad GUI deberá confirmarla esta prueba manual; no está certificada.

## 5. Core propio desde génesis (experimental)

### Arranque gestionado (Core nuevo y dedicado)

La app puede iniciar, supervisar y detener su propio Core. No adopta procesos
existentes ni directorios ajenos. Las dos rutas deben estar separadas, sin anidarse.
Ejemplo **Signet**, con puertos aislados y rutas inicialmente nuevas:

```sh
electrum-lite --network signet \
  --data-dir "$HOME/electrum-lite-managed/lite" \
  --core-dir "$HOME/electrum-lite-managed/core" \
  --bitcoind /ruta/bitcoin/bin/bitcoind \
  --core-rpc http://127.0.0.1:48332 --core-p2p-port 48333 \
  --confirm-dedicated --auto-prune managed-serve
```

No pases `--core-cookie`: el supervisor deriva la cookie privada de su Core.
Al reanudar, conserva red, rutas y puertos: se verifica el marcador de propiedad,
la configuración y el PID real del hijo. El listener gestionado es solo loopback.
Para una prueba sin red pública cambia a `--network regtest`; los nodos regtest
gestionados no buscan peers automáticamente. Puedes usar `bitcoin-cli` con esa
ruta y puerto para crear una cartera minera y generar monedas de prueba.

**Coordinación:** si el índice se retrasa 200 bloques, pausa P2P de su propio Core;
lo reanuda al bajar a 20 (`--pause-lag` / `--resume-lag`). Core siempre usa
`prune=1`, nunca poda por su cuenta. Con `--auto-prune`, cada 2016 bloques
(`--prune-interval`) y estando al día, detiene Electrum, desactiva P2P, ejecuta la
misma auditoría/backup/poda que el comando manual, y solo reanuda tras éxito.
Con los parámetros normales de Core en las redes de prueba, no intenta podar
antes de altura **1000**; no usa `-fastprune` ni elude esa protección.
SIGINT/SIGTERM cierran Electrum, sincronizan su WAL y detienen el Core propio.
Un fallo de mantenimiento detiene Core y no reanuda automáticamente.
Una terminación no interceptable del gestor (`SIGKILL`/fallo del sistema) no puede
ejecutar su limpieza: comprueba y detén manualmente su Core dedicado si continúa
activo. Nunca adopta ese proceso automáticamente; `prune=1` impide que Core pode
por su cuenta sin el gestor.

**No es un límite de 550 MiB.** Core conserva al menos 288 bloques y archivos
completos; backups y auditorías también consumen espacio. Cada mantenimiento
conserva una generación nueva de auditoría y backup en `RED/supervisor/`;
no se borran automáticamente. Vigila disco y conserva copias externas.
La auditoría actual es completa y costosa; no es aún el mantenimiento incremental
de mínimo espacio. Para posteriores auditorías pueden necesitarse fuentes
archivadoras: el supervisor conserva candidatos conocidos antes de apagar P2P,
o puedes indicarlos explícitamente con `--block-peer host:puerto`.
La disponibilidad de esas fuentes no está garantizada.
Los bloques recuperados solo para auditar se verifican y descartan en memoria:
no llenan la caché persistente de consultas de cartera.

Sin `--auto-prune` solo supervisa y controla el retraso: Core sigue acumulando
bloques hasta que realices mantenimiento. La poda automática **mainnet está
bloqueada**, sin excepción insegura. `prune-pending` bloquea su arranque automático.
Los eventos están en `RED/supervisor/events.jsonl`, sin consultas de cartera.

### Con un Core arrancado por ti

Usa **otra configuración y otro directorio nuevos**, no tu nodo habitual.
Configura la red elegida, `server=1`, `prune=1`, `txindex=0` y RPC loopback.
Arranca Lite a la vez. Ejemplo Signet, ajustando rutas:

```sh
electrum-lite --network signet \
  --data-dir "$HOME/.local/share/electrum-lite" \
  --core-cookie /ruta/core/signet/.cookie \
  --core-rpc http://127.0.0.1:38332 serve
```

Redes: `mainnet`, `signet`, `testnet4`, `regtest`. `--network` y otras opciones
preceden al comando. Hay `serve`, `managed-serve`, `sync`, `health`, `backup`, `audit` y `prune`.
El puerto RPC **debe indicarse**: por defecto es el de regtest 18443.
Los datos quedan en `--data-dir/RED/index`, caché en `RED/cache`.
No se reconstruye un índice vacío a partir de Core ya podado: falla claramente.

## 6. Bloques históricos y Tor opcional

Por defecto, conexiones P2P directas a las direcciones de peers conocidas por Core.
También puedes especificar fuentes archivadoras con `--block-peer host:puerto`
(repetible). El peer debe servir bloques witness y conservar el histórico solicitado.
Una fuente inaccesible produce error, nunca resultados inventados.

Para que la app arranque y detenga **su propio proceso Tor**:

```sh
sudo apt-get install tor
electrum-lite --network signet --data-dir /ruta/lite \
  --core-cookie /ruta/core/signet/.cookie --core-rpc http://127.0.0.1:38332 \
  --tor serve
```

No necesitas configurar ni arrancar un daemon Tor externo. El ejecutable `tor`
sí debe estar instalado; se usa un DataDirectory privado por red y SOCKS loopback
19050 (cambiable con `--tor-socks`). No carga el torrc del sistema. La app verifica
su PID mediante control autenticado con cookie y lo detiene al salir; también
configura vigilancia de su proceso padre. Bootstrap puede tardar: las consultas
locales siguen disponibles, pero el histórico remoto falla sin fallback directo.
Alternativa: `--socks-proxy 127.0.0.1:9050` para un proxy ya existente.
No pueden combinarse `--tor` y `--socks-proxy`.

**Privacidad:** sin Tor el peer ve tu IP y qué bloques pides; puede inferir intereses.
Con Tor disminuye la exposición de IP pero **no se garantiza** que sea imposible
relacionar peticiones con una cartera. No hay señuelos. No se envían consultas de
scripthash/TXID a servicios públicos; sí se solicitan hashes de bloques completos.
Core y el broadcast conservan su transporte propio: `--tor` solo afecta al P2P
histórico de Lite, no transforma Core en un nodo Tor.

Comprobaciones adicionales (opcionales, no necesarias para la prueba básica):

```sh
# Dos regtests offline, reserva 3 GiB: IBD P2P, backpressure y poda física.
python3 scripts/managed_prune_smoke.py --bitcoind /ruta/bitcoin/bin/bitcoind \
  --temp-dir target/regtest-smoke-tmp

# Tor real, red pública deshabilitada deliberadamente: proceso y fallo cerrado.
python3 scripts/tor_smoke.py --bitcoind /ruta/bitcoin/bin/bitcoind --tor-bin /ruta/tor

# Opt-in explícito para comprobar también el bootstrap en la red Tor pública.
python3 scripts/tor_smoke.py --bitcoind /ruta/bitcoin/bin/bitcoind \
  --tor-bin /ruta/tor --public-bootstrap
```

El test de Tor no envía scripthashes/TXIDs a terceros ni demuestra anonimato.
Para verificar además una descarga P2P pública **solo de génesis mainnet** por Tor,
sin cartera, sin poda y manteniendo Core offline en altura 0:

```sh
python3 scripts/tor_public_smoke.py --bitcoind /ruta/bitcoin/bin/bitcoind \
  --tor-bin /ruta/tor --allow-public
```

Los peers públicos pueden rechazar conexiones o no estar disponibles. Esta prueba
acotada no es una sincronización mainnet ni una garantía de disponibilidad futura.

## 7. LAN y acceso remoto

Loopback por defecto. LAN explícita:

```sh
electrum-lite --network regtest --data-dir /ruta/lite \
  --core-cookie /ruta/core/regtest/.cookie --core-rpc http://127.0.0.1:19443 \
  --listen 192.168.1.20:50001 --allow-lan serve
```

Usa IP concreta y firewall. Electrum TCP no cifra ni autentica: la LAN no es privada
por definición. Para remoto usa WireGuard o SSH; onion entrante es trabajo futuro.

## 8. Backup, rollback y poda de pruebas

Detén Lite antes de abrir el mismo índice desde otro comando. Checkpoint nuevo:

```sh
electrum-lite --network regtest --data-dir /ruta/lite \
  --core-cookie /ruta/core/regtest/.cookie --backup-dir /ruta/backup-NUEVO backup
```

El checkpoint es el directorio del índice, no el data root. Para restaurarlo,
con Lite detenido, copia a **OTRA** raíz: `/otra-raiz/regtest/index`, conservando
fuente y backup. Sirve con `--data-dir /otra-raiz`. Nunca mezcles generaciones ni
abras thin-v1 con el binario anterior al rewrite. Conserva este binario con el backup.
Los hardlinks de un checkpoint no protegen frente a fallo del disco: copia fuera.

La poda es manual, experimental y **bloqueada en mainnet**. Solo un Core de pruebas
dedicado, `prune=1`, P2P explícitamente apagado, mempool vacía, sin otros escritores.
Necesita `--confirm-prune`, `--audit-dir` NUEVO para un replay semántico completo
y `--backup-dir` NUEVO; nunca usar en datos valiosos. Auditoría/backup pueden
necesitar otro índice completo y, tras una poda previa, descargas históricas.
Si hay `prune-pending`, una operación anterior quedó incierta: conservar todo y
revisar/recuperar antes de continuar. No borrar el marcador para eludir el bloqueo.

## Límites importantes

- Un fsync por lote de hasta 64 bloques y mempool reconstruida: rendimiento aún por mejorar.
- Mempool máximo 10000 transacciones / 32 MiB; fuera de presupuesto falla cerrado.
- Historial/UTXO máximo 100000 resultados; error, nunca truncado.
- Undo de 288 bloques. Reorg más profunda requiere reconstruir en raíz nueva.
- Checkpoints de cabecera (`cp_height > 0`) no implementados; batches JSON de hasta 100 peticiones.
- Cache histórica sin expulsión automática; vigila disco. No hay ahorro medido.
- Sin TLS nativo, autenticación, onion entrante ni GUI.
- Capacidad/rendimiento mainnet, recuperación profunda y fuentes públicas sostenidas pendientes.

## Comprobaciones de desarrollo

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```
