# Electrum Lite — servidor thin experimental

**Versión `0.0.1-beta.1`.** Servidor Electrum en Rust construido desde cero:
Core valida, RocksDB conserva índices y los bloques históricos ausentes se obtienen
completos por P2P. Tor opcional. Sin APIs públicas ni servidores Electrum externos.

**Solo pruebas iniciales, sin fondos reales.** No está demostrada la velocidad de
IBD ni la capacidad mainnet. El supervisor de Core y la poda coordinada son experimentales.
No usar `prune=550` automático: usar un **Core nuevo dedicado con `prune=1`**.
No se ha probado una GUI de cartera; el smoke comprueba el protocolo TCP.
Diseño, estado y límites: [docs/project-status.md](docs/project-status.md).
Evidencia y alcance de las pruebas: [docs/verification.md](docs/verification.md).

Los mensajes de progreso y supervisor detectan `LC_ALL`, `LC_MESSAGES` y `LANG`,
en ese orden: español para `es` e inglés para los demás idiomas. La traducción
es parcial: algunos errores técnicos, secciones y diagnósticos de la biblioteca
CLI, instaladores y mensajes de Core/Tor aún aparecen en inglés. Los eventos JSON
y el protocolo Electrum conservan sus nombres y formato originales.

## 1. Instalar desde cero (Debian/Ubuntu, Linux x86-64)

### Binario listo para instalar o actualizar

Instala las dependencias con el gestor de paquetes de tu
distribución. En Debian/Ubuntu:

```sh
sudo apt-get update && sudo apt-get install -y curl coreutils libstdc++6 python3 gnupg procps ca-certificates
```

Después, esta línea instala o actualiza Lite y Bitcoin Core como usuario normal:

```sh
curl -fsSL https://raw.githubusercontent.com/AlbercoinDev/electrum-lite/main/scripts/install-release.sh -o /tmp/electrum-lite-installer.sh && sh /tmp/electrum-lite-installer.sh
```

El instalador comprueba SHA256 contra el valor fijado en el script, comprueba que
el binario arranca y conserva el ejecutable anterior con un nombre único. También
instala o actualiza Core con las verificaciones y restricciones de la sección 2,
antes de sustituir Lite. No inicia servicios ni modifica datos de nodos. Si Core
falla, no sustituye Lite; si después falla la instalación de Lite, Core puede haber
quedado actualizado. El checksum detecta errores o sustituciones del
archivo, pero **no es una firma**: el script y el checksum se distribuyen mediante
GitHub, por lo que dependen de la confianza en esa cuenta y en HTTPS. Puedes leer
el script descargado antes de ejecutarlo. Esta compilación Linux x86-64 depende
de glibc/libstdc++ compatibles con la máquina de compilación; no se garantiza en
distribuciones antiguas. Core se instala en el prefijo de usuario de la sección 2.

```sh
export PATH="$HOME/.local/bin:$PATH"
electrum-lite --version
```

### Alternativa: compilar desde fuentes

```sh
sudo apt-get update
sudo apt-get install -y build-essential clang libclang-dev pkg-config git curl python3 gnupg procps ca-certificates
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

También instala o actualiza Bitcoin Core siguiendo las restricciones de la sección 2.
No instala servicios, no inicia Core y no modifica datos de nodos existentes. El
binario previo se guarda como `electrum-lite.previous` al reinstalar. Core siempre
usa `~/.local`, aunque `PREFIX` cambie el destino de Lite.

### Instalar esta versión local sin recompilar

`python3 scripts/package.py` genera `dist/electrum-lite-thin-managed-experimental-linux-x86_64.tar.gz` con el binario Linux
x86-64, los fuentes y las instrucciones, sin datos de nodos ni `.git`.
Para instalarlo en una máquina compatible (glibc/libstdc++):

```sh
sha256sum -c electrum-lite-thin-managed-experimental-linux-x86_64.tar.gz.sha256
mkdir electrum-lite-instalacion-nueva
tar -xzf electrum-lite-thin-managed-experimental-linux-x86_64.tar.gz -C electrum-lite-instalacion-nueva
cd electrum-lite-instalacion-nueva/electrum-lite-thin-experimental
python3 scripts/install-bitcoin-core.py
mkdir -p "$HOME/.local/bin"
install -m 755 bin/electrum-lite "$HOME/.local/bin/electrum-lite"
"$HOME/.local/bin/electrum-lite" --version
```

El checksum detecta cambios respecto al paquete local; no es una firma de autoría
ni una release oficial. Guarda una copia del binario anterior antes de sustituirlo.

## 2. Instalar Bitcoin Core para las pruebas

### Debian/LMDE limpio: instalar o actualizar Core por separado

Las instrucciones anteriores ya lo hacen por defecto. Para hacerlo por separado,
ejecuta como usuario normal (pedirá contraseña de `sudo` para paquetes):

```sh
sudo apt-get update && sudo apt-get install -y curl ca-certificates && curl -fsSL https://raw.githubusercontent.com/AlbercoinDev/electrum-lite/main/scripts/install-bitcoin-core.sh -o /tmp/electrum-lite-core-install.sh && sh /tmp/electrum-lite-core-install.sh
```

Instala `python3`, `gnupg` y `procps` si faltan. Busca la versión estable que
anuncia la página oficial, verifica SHA256 y exige firmas válidas de las claves
fijadas de Ava Chow y Michael Ford. Si cambian las claves o faltan esas firmas,
falla cerrado y habrá que revisar/actualizar el instalador: no se garantiza que
toda versión futura esté firmada por ambos. Las claves se fijan por hash en el
script; verifica sus huellas en `scripts/install-bitcoin-core.py` y en fuentes
independientes. El script descargado desde GitHub no está firmado por nosotros.

Instala en `~/.local/opt/electrum-lite-bitcoin-core/releases/` y crea enlaces en
`~/.local/bin`. Repetir actualiza esa instalación y conserva versiones anteriores.
No modifica datos ni arranca Core. Si `bitcoind` está ejecutándose, detén primero
Core/Electrum Lite. Si detecta otro ejecutable Core en PATH o archivos ajenos en
`~/.local/bin`, se detiene sin sobrescribirlos: esas instalaciones deben actualizarse
por su propio método. No garantiza que otro usuario no arranque Core simultáneamente.

```sh
export PATH="$HOME/.local/bin:$PATH"
bitcoind --version
```

Para Signet gestionado, usa `--bitcoind "$HOME/.local/bin/bitcoind"` en el
comando de la sección 5. En este entorno se verificó la instalación y repetición
aisladas de Core 32.1; no se ha validado una IBD Signet completa con esa versión.

### Instalación manual

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

El Core gestionado utiliza por defecto una caché de base de datos de 64 MiB.
Si la máquina tiene RAM suficiente, se puede añadir antes de `managed-serve`
`--core-dbcache-mib 256` (rango permitido 64–1024). Es una configuración de
Core, no un límite de memoria total: Lite, Core y el sistema usan RAM adicional.
No se ha medido si acelera esta carga de trabajo ni garantiza eliminar pausas.

**Coordinación:** si el índice se retrasa 2000 bloques, pausa P2P de su propio Core;
lo reanuda al bajar a 200 (`--pause-lag` / `--resume-lag`). Además, pausa si el
espacio disponible para el usuario baja de 2048 MiB en cualquiera de los sistemas
de archivos de Core o Lite (`--min-free-mib`); para reanudar necesita 512 MiB
adicionales y cumplir el umbral de retraso. Se comprueba aproximadamente cada
5 segundos mediante GNU `df`; un error de medición detiene el supervisor y su
Core dedicado. Es un margen de seguridad, no una garantía de espacio: Core,
Lite, las auditorías y otros procesos pueden consumir espacio entre mediciones.
El umbral mayor puede aumentar considerablemente el uso temporal de disco; los
bloques no tienen un tamaño fijo. Core siempre usa
`prune=1`, nunca poda por su cuenta. Con `--auto-prune`, por defecto comprueba
tras cada bloque nuevo (`--prune-interval`, valor predeterminado 1) y, estando
al día y con un archivo candidato, prepara y audita una instantánea y crea un
backup a partir de ella mientras Electrum sigue activo. Tras el éxito de ambos
procesos pausa P2P de Core y mantiene el servidor Electrum en ejecución mientras
compara auditoría, instantánea y backup con el índice congelado y poda tras volver
a comprobar Core. Dos podas físicas en regtest conservaron el mismo proceso y una
conexión TCP existente; esto no garantiza disponibilidad continua de consultas.
La razón de conservar la desconexión P2P y las alternativas pendientes se explican
en [la investigación de poda](docs/p2p-pruning-investigation.md).
Durante la reserva se rechaza temporalmente la difusión de transacciones y algunas
consultas históricas pueden fallar o retrasarse; `server.health` indica que no
está listo. Si Core confirma que la punta cambió durante la preparación, el
supervisor aplaza la poda y reintenta después de limpiar únicamente generaciones
temporales reconocidas. Si no puede confirmar el cambio o encuentra contenido
inesperado, detiene el mantenimiento y conserva lo que no pueda limpiar con
seguridad. Los errores RPC y otros fallos no se interpretan como cambios de punta.
Con los parámetros normales de Core en las redes de prueba, no intenta podar
antes de altura **1000**; no usa `-fastprune` ni elude esa protección.
Antes del mantenimiento, el supervisor inspecciona en segundo plano las cabeceras
de archivos `blk` cerrados, comprueba que cada archivo no haya cambiado y pide a
Core las alturas de todas sus cabeceras. Omite la auditoría si no encuentra un
archivo por debajo del margen de 288 bloques. La inspección es conservadora: Core
sigue siendo la autoridad sobre la poda y puede conservar archivos por otras
razones. Si la inspección falla, se aplaza y reintenta; no se eliminan datos.
Si Core no borra ningún archivo, los candidatos sin cambios se posponen durante
144 bloques o 15 minutos; el siguiente intento requiere además un bloque nuevo.
Este aplazamiento se conserva solo mientras el supervisor está en ejecución.
Sigue aplicándose el intervalo configurado. Esta detección es experimental y
ha superado una prueba de poda física regtest desechable durante IBD; no se ha
validado una sincronización Signet completa ni todos los posibles fallos.
SIGINT/SIGTERM cierran Electrum, sincronizan su WAL y detienen el Core propio.
Un fallo de mantenimiento detiene Core y no reanuda automáticamente.
Una terminación no interceptable del gestor (`SIGKILL`/fallo del sistema) no puede
ejecutar su limpieza: comprueba y detén manualmente su Core dedicado si continúa
activo. Nunca adopta ese proceso automáticamente; `prune=1` impide que Core pode
por su cuenta sin el gestor.

**No es un límite de 550 MiB.** Core conserva al menos 288 bloques y archivos
completos; backups y auditorías también consumen espacio. Tras cada mantenimiento
automático exitoso, el supervisor verifica la nueva auditoría y backup y elimina
parejas anteriores identificadas por registros completos propios en
`RED/supervisor/`. Conserva la nueva pareja, registros, generaciones incompletas,
contenido desconocido y enlaces simbólicos; si hay dudas omite la limpieza.
Una interrupción puede dejar una pareja antigua parcialmente eliminada. La limpieza
no se ejecuta si hay `prune-pending`. Estos checkpoints pueden compartir archivos
en el mismo disco y no sustituyen copias externas. Vigila el espacio disponible.
La primera auditoría es completa. En podas posteriores puede reutilizar una
pareja anterior con registro completo, identidad y contenido verificados: comprueba
su ascendencia contra el índice y Core, reproduce los bloques nuevos y compara
todo el estado confirmado y los undo retenidos. Ante una rama distinta vuelve a
la auditoría completa; un fallo RPC interrumpe el mantenimiento. La confianza
en el prefijo procede de su auditoría anterior, no de volver a procesar todos sus
bloques; los registros no son firmas contra manipulación. Esta auditoría incremental
es experimental. En mantenimiento gestionado Electrum sigue activo durante la
auditoría, el backup, la comprobación final y la poda. La red de Core permanece
pausada desde la comprobación final hasta que termine la limpieza; sus pares
deben reconectarse después. Un fallo de mantenimiento detiene el servicio y Core.
Para auditorías completas pueden necesitarse fuentes archivadoras: el supervisor
conserva candidatos conocidos antes de apagar P2P, o puedes indicarlos explícitamente
con `--block-peer host:puerto`.
La disponibilidad de esas fuentes no está garantizada.
Los bloques recuperados solo para auditar se verifican y descartan en memoria:
no llenan la caché persistente de consultas de cartera.

Sin `--auto-prune` solo supervisa y controla el retraso: Core sigue acumulando
bloques hasta que realices mantenimiento. La poda automática **mainnet está
bloqueada**, sin excepción insegura. `prune-pending` bloquea su arranque automático.
Los eventos están en `RED/supervisor/events.jsonl`, sin consultas de cartera.

### Si el arranque falla por un archivo inexistente

`No such file or directory (os error 2)` no indica por sí solo qué ruta falta.
Comprueba primero el ejecutable indicado en `--bitcoind`, sin modificar los datos:

```sh
ls -l "$HOME/.local/bin/bitcoind"
"$HOME/.local/bin/bitcoind" --version
command -v bitcoind
```

Si usas otra ruta en `--bitcoind`, comprueba esa ruta en su lugar. Un enlace roto
o un ejecutable que no puede arrancar también requieren investigación. Si Core
está instalado en otro lugar, usa su ruta real; si no está instalado, consulta la
sección 2. Si el ejecutable funciona, revisa las rutas configuradas y, si existe,
`RED/supervisor/events.jsonl`. Antes de compartir diagnósticos, revisa si contienen
rutas o direcciones privadas. No borres ni recrees los directorios de Core o Lite
para intentar resolver el error, ni elimines `prune-pending`.

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
abras thin-v1 con un binario incompatible. Conserva este binario con el backup.
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
