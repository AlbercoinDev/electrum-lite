# Linux: instalación del ejecutable, conexión y uso

Paquete experimental `0.0.1-beta.1`, compilado desde el commit `5145bd3` indicado
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
sha256sum --check electrum-lite-0.0.1-beta.1-linux-x86_64-5145bd3.tar.gz.sha256
tar -xzf electrum-lite-0.0.1-beta.1-linux-x86_64-5145bd3.tar.gz
cd electrum-lite-0.0.1-beta.1-linux-x86_64-5145bd3
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

Desde otra terminal:

```sh
"$HOME/.local/bin/electrum-lite" --network signet health --address 127.0.0.1:60601
```

Espera `ready: true`. Revisa también IBD, altura Core/índice, pares, red activa y
espacio. Si mempool excede los límites o falta histórico, no se sirven respuestas
parciales. `ready` no garantiza por sí solo que Core esté conectado a pares.

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
