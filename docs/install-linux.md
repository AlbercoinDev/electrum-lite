# Linux: instalación del ejecutable, conexión y uso

## Instalación y actualización conjunta

El bootstrap nuevo instala y actualiza Lite y la última versión estable oficial
de Core en `~/electrum-lite/`, sin sudo y sin tocar `~/.bitcoin`.
Requiere Linux x86-64, glibc 2.34+, Python 3.11+, curl y GnuPG.
No requiere una instalación previa de Core ni Rust.

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/AlbercoinDev/electrum-lite/main/install.sh | sh
"$HOME/electrum-lite/electrum-lite" --network signet
```

La implementación local pasó instalación desde descargas públicas,
arranque Signet **offline**, rechazo de actualización con Lite activo, cierre
SIGTERM de Lite/Core y repetición idempotente preservando datos (2026-10-29).
No equivale a una sincronización pública completa.

Ctrl+C cierra el supervisor y su Core dedicado. Para actualizar, cierra primero
Lite y repite la línea de instalación. El instalador no mata procesos, no hace
downgrade automático y rechaza una última versión no validada: no elige otra
anterior. Conserva versiones y datos; no elimina backups automáticamente.

Distribución: `versions/{lite,bitcoin-core,runtime}/`, `config/`, `data/`,
`backups/`, `logs/`, `tmp/`, `active.json` y lanzador `electrum-lite`.
Por red, Core reside en `data/signet/core/` y el motor en `data/signet/lite/`.
El motor actual añade todavía su subdirectorio `signet` bajo esa raíz.
Electrum Signet escucha solo en `127.0.0.1:60601`, RPC dedicado en 39332.

La poda es automática y coordinada por Lite, mientras Core usa `prune=1`.
Requiere auditoría, backup verificado y margen de reorg; mainnet permanece
bloqueada en el supervisor. Ni el crecimiento total de disco ni la superioridad
de rendimiento sobre Core están demostrados. El motor sigue siendo SQLite:
la estructura LSM propuesta todavía no está implementada.

## Flujo manual anterior (paquetes publicados previamente)

Paquete experimental `0.0.1-beta.1`, compilado desde el commit indicado
en `BUILD-INFO.txt`, no desde el código del tag original. Descárgalo junto con su
checksum en https://github.com/AlbercoinDev/electrum-lite/releases/tag/v0.0.1-beta.1.
El tag original no se modifica. Windows se prepara por separado.

## Requisitos

- Linux **x86-64**, glibc **2.34 o posterior**, cargador dinámico y `libgcc_s.so.1`.
  No es un ejecutable estático ni para Alpine/musl o Raspberry Pi ARM.
- `tar`, `sha256sum` y shell POSIX para extracción/instalación.
- Bitcoin Core: integración probada con **31.0**, instalado por separado y
  verificado desde https://bitcoincore.org/en/download/.
- Espacio para Core, archivo histórico, SQLite y backups. No hay mínimo de RAM
  ni ahorro de disco demostrado. No necesita Rust para ejecutar el paquete.

## 1. Extraer e instalar

Desde el directorio que contiene el `.tar.gz` y su `.sha256`:

```sh
PAQUETE="electrum-lite-0.0.1-beta.1-linux-x86_64-COMMIT.tar.gz" # Sustituye COMMIT por el del archivo descargado.
sha256sum --check "$PAQUETE.sha256"
tar -xzf "$PAQUETE"
cd "${PAQUETE%.tar.gz}"
sh ./install.sh
"$HOME/.local/bin/electrum-lite" --version
```

El instalador no usa sudo, no modifica Core, no inicia servicios y **rechaza
sobrescribir otro ejecutable**. Puedes usar otro prefijo absoluto, por ejemplo
`sh ./install.sh "$HOME/aplicaciones/electrum-lite"`, o ejecutar `./electrum-lite`
directamente sin instalar. Un checksum detecta alteraciones, pero no autentica
el origen: esta entrega no tiene firma criptográfica del mantenedor.

Opcional: añade `export PATH="$HOME/.local/bin:$PATH"` a la configuración de tu
shell. Los ejemplos siguientes usan la ruta completa para no depender de PATH.

## 2. Preparar Bitcoin Core (Signet)

Empieza con un nodo **dedicado de laboratorio**, sin fondos. No cambies ni podes
un nodo existente valioso. Puedes usar una raíz separada:

```sh
mkdir -p "$HOME/.bitcoin-electrum-lite-lab"
bitcoind -signet -datadir="$HOME/.bitcoin-electrum-lite-lab" \
  -server=1 -prune=1 -txindex=0
```

Este comando mantiene Core en primer plano; deja esa terminal abierta. Debe estar
disponible `bitcoind` en PATH. Signet requiere conexión a Internet y descarga de
bloques. `prune=1` es poda **manual**: no elimina bloques automáticamente ni
limita el crecimiento del disco. No ejecutes poda para comenzar.

No expongas RPC a Internet ni uses opciones que lo publiquen fuera de localhost.
No copies la cookie en documentación o mensajes. Si ya tienes un Core adecuado,
usa su ruta real de cookie en lugar de la del laboratorio.

Comprueba la conexión desde otra terminal:

```sh
"$HOME/.local/bin/electrum-lite" --network signet check-core \
  --cookie-file "$HOME/.bitcoin-electrum-lite-lab/signet/.cookie"
```

Un índice nuevo necesita bloques desde génesis. Si Core ya eliminó histórico,
se requiere un archivo confiable o un nodo con esos bloques: no se omiten.

## 3. Arrancar Electrum Lite

```sh
"$HOME/.local/bin/electrum-lite" --network signet \
  --data-dir "$HOME/.local/share/electrum-lite" serve \
  --cookie-file "$HOME/.bitcoin-electrum-lite-lab/signet/.cookie" \
  --listen 127.0.0.1:60601
```

Mantén esa terminal abierta. La app crea el subdirectorio `signet` dentro de la
raíz de datos, comprueba archivos, indexa y reconcilia mempool. No modifica la
configuración de Core. La sincronización completa puede tardar y aún no ha sido
validada públicamente de forma sostenida en este proyecto.

La terminal del servidor muestra progreso aproximadamente cada cinco segundos:
alturas de Core e índice, retraso, velocidad entre muestras, espacio libre y
alertas de poda o errores. La observación puede estar desactualizada o demorarse
si el worker está ocupado. `serve` normal no detiene Core. `managed-serve` es un
modo Linux experimental para un Core de prueba dedicado y distinto, con poda
manual, descrito en `docs/operations.md`; no adopta tu Core existente. La poda
automática de Core no está validada para preservar el histórico.

Desde otra terminal:

```sh
"$HOME/.local/bin/electrum-lite" --network signet health --address 127.0.0.1:60601
```

Espera `ready: true`. Revisa también IBD, altura Core/índice, pares, red activa y
espacio. Si mempool excede los límites o falta histórico, no se sirven respuestas
parciales. `ready` no garantiza por sí solo que Core esté conectado a pares.

Para seguimiento continuo, abre otra terminal y ejecuta:

```sh
"$HOME/.local/bin/electrum-lite" --network signet monitor --address 127.0.0.1:60601
```

Produce una línea JSON cada cinco segundos; Ctrl+C detiene solo el monitor. La
velocidad es la diferencia entre muestras, no una garantía ni una estimación de
finalización. Las alturas de Core son observaciones con edad indicada, no un
estado atómico global. Alerta si detecta poda automática o histórico faltante.
Core con `prune=1` usa poda manual; no activa poda automática para ahorrar disco.

## 4. Conectar y usar una cartera

En una cartera con soporte de Signet y Electrum privado:

1. Selecciona **Signet**, no mainnet.
2. Elige **Private Electrum server** (en Sparrow, ajustes de servidor).
3. Host **127.0.0.1**, puerto **60601**, TCP sin SSL **solo en este equipo**.
4. Usa una cartera de laboratorio sin fondos reales. Comprueba historial,
   recepción y difusión de una transacción de prueba cuando el servicio esté listo.

La GUI real de Sparrow **no se ha validado** aquí; sus opciones dependen de su
versión. No importes una semilla real para probar compatibilidad. Electrum Lite
no gestiona semillas, claves privadas ni crea una cartera.

Para otra máquina, usa un túnel SSH; ejecuta en el equipo de la cartera:

```sh
ssh -N -L 60601:127.0.0.1:60601 usuario@equipo-del-servidor
```

La cartera sigue apuntando a su localhost. No publiques TCP sin cifrar ni el RPC.

## 5. Parar, volver a arrancar y hacer backup

Ctrl+C detiene Electrum Lite. Vuelve a ejecutar el mismo comando para continuar
con los mismos datos. Core es un proceso separado: parar la app no para Core.

Para detener el Core del laboratorio de forma ordenada:

```sh
bitcoin-cli -signet -datadir="$HOME/.bitcoin-electrum-lite-lab" stop
```

Con Electrum Lite detenido, crea un backup en un destino **nuevo**:

```sh
"$HOME/.local/bin/electrum-lite" --network signet \
  --data-dir "$HOME/.local/share/electrum-lite" backup \
  --output "$HOME/electrum-lite-backup-signet-01"
```

No borres datos para solucionar errores. Conserva ejecutable anterior y backup
antes de actualizar. Schema 1 requiere migración explícita; consulta
`docs/operations.md` incluido. Para desinstalar, elimina únicamente el ejecutable
instalado; los datos y Core no se eliminan automáticamente.

## Otras redes y límites

| Red | `--network` | Cookie habitual de Core | Puerto Electrum sugerido |
|---|---|---|---|
| Signet | signet | ~/.bitcoin/signet/.cookie | 60601 |
| Testnet4 | testnet4 | ~/.bitcoin/testnet4/.cookie | 61001 |
| Mainnet experimental | mainnet | ~/.bitcoin/.cookie | 50001 |

Adapta red, cookie y puerto en **todos** los comandos y en la cartera. `--network`
siempre va antes del subcomando. **La poda mainnet está bloqueada.** No se ofrece
certificación de producción, recuperación ante pérdida física de energía ni
rendimiento garantizado. Licencia propia pendiente; avisos de terceros incluidos.
# Instalador antiguo: referencia histórica, no usar para el flujo conjunto

```sh
# El bootstrap de main ahora usa el flujo conjunto descrito al principio.
```

Requisitos: Linux x86-64, Python 3.11+, curl y las bibliotecas dinámicas requeridas por el paquete. Ejecutar como usuario normal, sin sudo. El bootstrap y el paquete se descargan por HTTPS. El código remoto se ejecuta con los permisos de tu usuario: revísalo antes si no confías en el repositorio. Los paquetes actuales **no están firmados**; comprobar SHA-256 contra un archivo servido por el mismo GitHub no aporta autenticación independiente.

La misma orden instala o actualiza el ejecutable. Selecciona por fecha de publicación el paquete Linux con checksum más reciente de `v0.0.1-beta.1`; distingue entregas por commit aunque compartan versión. No sigue automáticamente futuras releases ni acepta otros formatos/versiones sin actualizar el instalador.

Se instalan el binario y su identificación de compilación bajo `~/.local/share/electrum-lite/versions/`. `current` identifica la entrega activa y `previous` la anterior; `~/.local/bin/electrum-lite` apunta a `current`. Si falta `~/.local/bin` en PATH, usa la ruta completa o añade ese directorio al PATH. Las entregas anteriores nunca se borran automáticamente. Un ejecutable antiguo instalado directamente en esa ruta se rechaza: consérvalo y muévelo explícitamente antes de adoptar el instalador nuevo.

**No configura ni arranca Core/Lite, no modifica servicios ni datos, no hace backup de datos ni migraciones.** Un daemon ya iniciado continúa usando el binario anterior. Antes de reiniciarlo para usar la nueva entrega:

1. Detén Lite limpiamente.
2. Usa el binario en `previous/electrum-lite` con tu red/data-root para crear y verificar un backup completo en un destino nuevo.
3. Revisa la compatibilidad de esquema. Migra únicamente con el procedimiento explícito y su backup obligatorio, si hace falta.
4. Arranca con `current/electrum-lite` y comprueba `health`; sincronizando no equivale necesariamente a fallo.

Rollback: detén Lite y usa directamente `~/.local/share/electrum-lite/previous/electrum-lite`. Si el esquema cambió, restaura el backup en **otra raíz** y usa allí el binario anterior; nunca abras un esquema incompatible ni sobrescribas la generación fallida. Consulta también `docs/operations.md`. El cambio de enlace es atómico, pero no se promete durabilidad del instalador ante pérdida física de energía: los datos de cadena no se tocan y las versiones se conservan.
