# Operación en servidores pequeños (4 GB RAM, 2 vCPU, HDD/SSD)

Esta guía describe el modo recomendado: Lite arranca y detiene su propio Core
dedicado, Core usa `prune=1` (poda máxima) y Lite mantiene el índice completo.
Core solo se usa para indexar y para mempool/broadcast; los cuerpos de bloques
antiguos los reconstruye Lite con una ventana ciega de bloques públicos.

## Requisitos

- Debian/Ubuntu x86-64, terminal.
- 4 GB RAM mínimo; 2 vCPU mínimo.
- 300 GB de disco para el índice `thin-v3-txnum` (~150-180 GB), Core podado
  (~15 GB), caché (~3 GB) y margen de compactación.
- SSD/NVMe recomendado. HDD SATA soportado, incluido 2,5" 5400 rpm, con más
  latencia en consultas en frío. Evitar discos SMR: las pausas de escritura
  pueden ser de decenas de segundos.
- No hay backup integrado obligatorio. Sin una copia externa, la recuperación
  es una re-sincronización completa (re-IBD). El comando `backup` existe para
  cuando se conecte un disco externo.

## ext4

Montaje recomendado:

```text
UUID=... /srv ext4 noatime,nodiratime,data=ordered,commit=30,errors=remount-ro 0 2
```

`ext4` no protege los datos con checksums; RocksDB verifica sus propios bloques
y Lite falla cerrado ante corrupción.

Sysctl recomendado:

```text
vm.swappiness=10
vm.dirty_background_ratio=5
vm.dirty_ratio=10
vm.dirty_expire_centisecs=3000
vm.dirty_writeback_centisecs=1500
vm.vfs_cache_pressure=50
```

En HDD:

```sh
# Evitar el aparcado del disco (si el dispositivo lo soporta).
hdparm -B 254 -S 0 /dev/sdX
# Planificador adecuado para HDD.
echo mq-deadline > /sys/block/sdX/queue/scheduler
```

## Perfil de almacenamiento

`--storage hdd` (por defecto) usa 64 MiB de write buffer, 3 memtables, 256 MiB
por SST, un job de compactación y 8 MiB de readahead. `--storage ssd` usa 128 MiB
por SST y dos jobs. El perfil se fija al crear el índice y no se cambia en caliente.

## systemd

Crea un usuario de sistema y los directorios antes de instalar la unidad:

```sh
sudo useradd --system --home /srv/electrum-lite --shell /usr/sbin/nologin electrum-lite
sudo install -d -o electrum-lite -g electrum-lite /srv/electrum-lite /srv/electrum-lite-core
```

Copia `contrib/electrum-lite.service` a `/etc/systemd/system/`, ajusta usuario,
rutas y binarios, y:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now electrum-lite
```

`KillMode=control-group` hace que Core muera con Lite. Sin esto, un fallo de Lite
podría dejar Core descargando y podando por delante del índice.

## Poda de Core y recuperación

- Core arranca con `-prune=1`, `-rest=1`, `-disablewallet=1`, `-txindex=0`,
  `-blockfilterindex=0`, `-maxmempool=50`, `-par=1`.
- Lite comprueba `pruneheight` en cada ciclo. Si Core podó por delante del índice,
  pausa la red y falla cerrado.
- No se adopta un Core ya podado por debajo del tip de Lite. En ese caso se
  necesita un snapshot del índice o un re-IBD con Core nuevo.
- La poda mainnet manual exige `--confirm-pruned-mainnet`; el modo gestionado
  usa la poda automática de Core con backpressure.

## Privacidad de la reconstrucción

Lite nunca envía scripthashes, direcciones ni TXID a servicios públicos. Cuando
Core ya no tiene un bloque, pide el **bloque crudo** por hash dentro de una
ventana alineada de 16 bloques, en orden aleatorio, con una descarga señuelo y
un circuito Tor por ventana. Verifica hash, Merkle root y witness commitment.
Los endpoints por defecto son las direcciones onion de mempool.space y
Blockstream.info. `ELECTRUM_LITE_ESPLORA` permite sustituirlos.

Sin Tor ni `--socks-proxy`, la reconstrucción pública solo se activa con
`--allow-public-data`; en ese caso la IP queda expuesta al proveedor.

No se garantiza anonimato: el proveedor ve conjuntos de ventanas y su momento.
La ventana oculta el bloque exacto, no el patrón global de consultas.

## Límites

- La primera sincronización en HDD 5400 rpm y 2 vCPU puede durar semanas.
- Las consultas de scripts fríos pueden tardar decenas de ms o segundos.
- `transaction.get` de una transacción antigua puede tardar decenas de segundos
  la primera vez (ventana + Tor); después se sirve de caché.
- Sin backup, un fallo del disco implica re-IBD completo.

## Actualización a `thin-v3-txnum`

Esta release no migra índices `thin-v1`/`thin-v2` en el sitio. Detén el servicio,
conserva el índice y el binario antiguos, y arranca con un data-dir nuevo. Si el
Core anterior está podado, necesita un `core-dir` nuevo y una sincronización
completa; un Core podado no puede reconstruir el índice. Si el Core anterior
conserva todos los bloques, puede reutilizarse.

Las rutas de ejemplo y el procedimiento systemd están en el apartado
“Actualizar desde `thin-v1` o `thin-v2`” del README.
