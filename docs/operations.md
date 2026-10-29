# Operación y recuperación — 0.0.1-beta.1

## Modelo operacional

Un proceso con un worker de almacenamiento/RPC y tareas Tokio de transporte. El worker serializa sincronización, mempool y consultas; no hay transacciones SQLite de lectura retenidas mientras un cliente consume la red. Colas, mensajes, conexiones, suscripciones y respuestas están acotados. No usar como servidor público ni con fondos reales sin validación adicional.

El servicio verifica el archivo completo al arrancar. Si no alcanza Core, mempool excede presupuesto, falta histórico, cambia la rama o falla la integridad, las consultas de cartera fallan en vez de devolver datos parciales. `server.health`/`health` siguen proporcionando diagnóstico cuando el worker está operativo. Los errores de sincronización no contienen direcciones ni scripts consultados.

Controla por separado disco de Core, disco de la app y backups, tips/lag, IBD, pares/red activa y `ready`. Los presupuestos son de elementos/bytes serializados, no límites precisos de RSS. Un archivo completo puede ocupar cientos de GB o más; backups y auditorías pueden necesitar otra copia de archivo/base. No hay benchmark mainnet ni garantía de ahorro frente a Core archivador.

## Servicio systemd de usuario (ejemplo)

`config/electrum-lite@.service` es una plantilla, no una instalación ejecutada/validada de systemd en este entorno. Corre con tu usuario, no root. Necesita leer la cookie del Core dedicado y escribir solo sus propios datos.

Después de compilar, conservar el binario anterior y, si corresponde, migrar datos:

```sh
cargo install --path . --locked
mkdir -p "$HOME/.config/systemd/user" "$HOME/.config/electrum-lite"
cp config/electrum-lite@.service "$HOME/.config/systemd/user/"
cp config/examples/signet.env "$HOME/.config/electrum-lite/signet.env"
```

Edita `signet.env`: sustituye `/home/CHANGE_ME` y comprueba puerto/endpoint. No pongas el contenido de la cookie en ese fichero. Para otra red usa su ejemplo; mainnet tiene cookie en el datadir base, no en `mainnet/`.

```sh
systemctl --user daemon-reload
systemctl --user enable --now electrum-lite@signet.service
journalctl --user -u electrum-lite@signet.service
electrum-lite --network signet health --address 127.0.0.1:60601
```

La plantilla usa `~/.local/share/electrum-lite` como raíz de datos, umask 0077 y parada de hasta 120 s. Reinicia ante fallo del proceso, pero una base incompatible debe resolverse con migración/restore, no reinicios infinitos. Persistencia del servicio tras cerrar sesión depende de la configuración de usuario/lingering de tu sistema; no se modifica automáticamente.

La plantilla NO arranca Core, abre firewall, habilita poda ni configura TLS. Para actualizar un binario ya instalado `cargo install` necesita `--force`: detén antes el servicio y conserva una copia del ejecutable previo.

## Supervisor Linux experimental

`managed-serve` arranca primero Electrum Lite y espera a que su worker termine
de abrir y recuperar el índice antes de iniciar un **Core de pruebas dedicado**.
Exige Linux, un directorio Core nuevo o creado anteriormente por este modo,
puertos loopback libres y `--confirm-dedicated`. No adopta, configura ni detiene
tu nodo existente. Mainnet está rechazada. Emplea `prune=1`, no `prune=550`.

```sh
electrum-lite --network signet --data-dir "$HOME/electrum-lab-index" managed-serve \
  --bitcoind /ruta/absoluta/a/bitcoind \
  --core-dir "$HOME/electrum-lab-core" --rpc-port 38333 \
  --listen 127.0.0.1:60601 --confirm-dedicated
```

No ejecutes ese ejemplo con datos valiosos. El Core se crea sin cartera y
necesita espacio para sincronizar. El directorio dedicado debe ser distinto y
externo al índice; el supervisor no adopta carpetas existentes sin su marcador.
Los datos se conservan al detenerlo. `--offline` solo sirve para laboratorio.

Si el daemon termina, no responde durante 45 s o informa errores persistentes,
el supervisor solicita SIGTERM al Core hijo que él creó, espera hasta 120 s y
notifica si la parada se confirma. Tres errores persistentes se consideran
fallo, salvo calentamiento normal de mempool. Durante el arranque de Core existe
un periodo de gracia para esperar su RPC. Ctrl+C detiene ambos. En Linux se usa
también `PR_SET_PDEATHSIG` para solicitar SIGTERM a los hijos si muere el
supervisor; se rechazan ejecutables con setuid/setgid o capacidades de archivo.

Esto **no es una garantía universal**: una orden puede no ser atendida, una VM o
el hardware pueden fallar, o puede haber otros Core iniciados fuera del supervisor.
No hay adopción de procesos existentes ni control del nodo normal mediante su
RPC. El test automatizado crea su propio Core regtest offline y simula SIGKILL
del daemon y del supervisor; no equivale a pruebas de energía ni a sincronización
real sostenida. La compatibilidad segura con `prune=550` sigue pendiente.

## Transporte remoto

TCP solo se enlaza a loopback. No desactives esa restricción ni abras RPC. Ejemplo de túnel SSH desde la máquina de la cartera:

```sh
ssh -N -L 60601:127.0.0.1:60601 usuario@servidor
```

Conecta la cartera a su propio `127.0.0.1:60601`. También es posible un proxy TLS con certificado verificado o un túnel privado correctamente configurado. TLS nativo, WebSocket y publicidad de pares no están implementados; la prueba automatizada utiliza TCP local, no certifica tu túnel/proxy.

## Migración 1 → 2

1. Detén todas las CLI/servicios de esa red y conserva el binario anterior.
2. `migrate-index --backup-dir DIRECTORIO_NUEVO` copia red completa y journals, sincroniza el backup y cambia esquema en una transacción.
3. `inspect` verifica archivos, backfillea cabeceras migradas y valida referencias/contadores. `audit` reconstruye semántica independientemente.
4. Solo después arranca el servicio y comprueba `ready` y red.

Schema 2 incorpora cabeceras y cursor del log de ramas. El formato físico ELBETA01 se conserva, pero ahora hay repeticiones durables de un bloque ancestral como intención de rewind. Después de crear ramas **no se puede volver al lector lineal anterior sobre los datos actuales**. Para rollback restaura la copia previa a otro directorio y usa el binario previo. Nunca mezcles una base nueva con journals viejos ni muevas la etiqueta Git publicada.

No hay migración silenciosa entre esquemas. Una importación de archivos sin base crea schema 2; una base 1 existente exige la acción explícita anterior. Una interrupción deja el esquema viejo o nuevo completo según SQLite; el backup original se conserva. Si algo falla, no borres datos para intentar arrancar.

## Backups y restauración

`backup --output NUEVO` se ejecuta con el archivo bloqueado y sin otro escritor. Copia todos los archivos regulares, excepto el lock, incluyendo un journal presente; rechaza enlaces/subdirectorios. El manifiesto `BACKUP_COMPLETE.json` con hashes identifica una copia finalizada.

`verify-backup --directory BACKUP` valida red, basenames seguros, hashes y ausencia de archivos no manifestados. No autentica un backup obtenido de terceros: SHA-256 no sustituye una fuente confiable.

`restore --backup-dir BACKUP --output-root NUEVO` verifica, copia, sincroniza y verifica de nuevo. Jamás sobrescribe un destino existente ni modifica el origen. Después usa `inspect` y `audit` en la raíz restaurada. Un backup schema 1 necesita el binario previo o migración explícita. El manifiesto es de la copia original; una raíz restaurada usada para sincronizar pasa a ser datos de trabajo, no un backup inmutable.

Mantén una copia fuera del dispositivo de trabajo y prueba restauración. Una copia en el mismo disco no protege de averías de ese disco. Las pruebas regtest usan copias cerradas; no certifican backups en caliente ni snapshots arbitrarios de VM.

## Colas corruptas, esquema desconocido o discrepancias

La app no trunca ni borra una cola corrupta automáticamente. Conserva el conjunto completo y el error. Si el archivo está íntegro y solo hay duda sobre SQLite, `rebuild-index --output NUEVO.sqlite` reconstruye aparte y rechaza destinos existentes. `audit` compara las tablas semánticas contra una reconstrucción independiente.

Para sustituir una base, trabaja en una nueva raíz que contenga los bloques y la nueva base; no dejes un journal de la base anterior a su lado. Mantén el original como rollback. Si falta/corrompe el propio archivo de bloques, se necesita un backup verificado u otra fuente histórica confiable.

## Reorganizaciones profundas

El servicio revierte solo si conserva todo el undo necesario. Antes de modificar índices escribe/sincroniza la intención de rewind al log; la recuperación tras crash la termina. Los bloques antiguos quedan como huérfanos archivados, sin GC inicialmente. La reconstrucción desde el log reproduce también el rewind, incluso si todavía no se aplicó una nueva rama.

Fuera del undo disponible: modo no listo, no poda y ningún reset silencioso. Detén el servicio y construye una raíz nueva:

```sh
electrum-lite --network signet --data-dir ./data recover-chain \
  --cookie-file "$HOME/.bitcoin/signet/.cookie" --output-root ./recovered-data
electrum-lite --network signet --data-dir ./recovered-data inspect
electrum-lite --network signet --data-dir ./recovered-data audit
```

Reutiliza bloques de la cadena activa archivada cuando coinciden con Core; obtiene de Core la rama canónica que falta. Conserva intacta la generación original. Necesita espacio para otra copia y puede ser lento. Si Core ya no tiene los bloques canónicos necesarios, falla: se requiere una fuente confiable que sí los tenga. No promete recuperar cualquier reorg sin datos.

Opcionalmente puede configurarse otro Bitcoin Core histórico de confianza en
loopback (o mediante un túnel seguro). **Solo se contacta si el Core local
realmente devuelve error de bloque podado y su `pruneheight` confirma la
ausencia.** Al detectar el primer bloque faltante, pide escribir `SI` en una
terminal interactiva antes de conectar a la fuente; no hay confirmación por
defecto ni conexión silenciosa. La confirmación cubre los bloques ausentes de
esa recuperación. La fuente puede aprender qué bloques se solicitan.

```sh
electrum-lite --network signet --data-dir ./data recover-chain \
  --cookie-file "$HOME/.bitcoin/signet/.cookie" --output-root ./recovered-data \
  --historical-rpc-url http://127.0.0.1:38333/ \
  --historical-cookie-file /ruta/privada/a/cookie-del-core-historico
```

Son parámetros de ejemplo: la fuente debe existir, ser confiable, estar en la
misma red y conservar los bloques. El hash canónico se obtiene del Core local;
se valida la identidad de la fuente, el hash del bloque, Merkle y witness al
importarlo. Si se deniega, no hay terminal, falla la red o no se confirma poda,
no se contacta la fuente. La raíz nueva puede quedar incompleta; consérvala para
revisión y usa otra raíz nueva en un intento posterior. El índice original no
se sustituye. La prueba de confirmación usa servicios RPC simulados; falta una
prueba pública sostenida con fuente histórica real.

## Poda explícita de redes de prueba

1. Usa un Core dedicado, `prune=1`, sin poda automática ni otros consumidores que necesiten bloques antiguos.
2. Detén Electrum Lite y otros escritores de cadena/minería. Desactiva explícitamente la red P2P de Core (`bitcoin-cli -signet setnetworkactive false`, ajustando la red).
3. Ejecuta `prune --confirm --backup-dir NUEVO`. Comprueba espacio suficiente para backup y auditoría; el mínimo de espacio libre no calcula el tamaño final de esas copias.
4. Se verifica prefijo contra Core, se espera su cola de validación, se reconstruye/audita el índice, se crea backup y se confirma una petición en `prune_audit` antes de llamar al RPC destructivo.
5. La altura máxima es tip durable menos 288; Core aplica sus propios mínimos y elimina archivos completos. Se registra altura real y se verifica que un bloque reportado eliminado ya no está disponible en Core.
6. Revisa el resultado, vuelve a activar la red de Core cuando sea seguro y reinicia el servicio. Verifica `ready`, pares y actualizaciones.

Puede podarse un prefijo durable mientras Core vaya por delante durante IBD, pero nunca datos no archivados/indexados. Esta coordinación requiere una ventana de mantenimiento sin otros cambios de cadena; dos RPC separados no son una transacción global con Core. No se modifica automáticamente su conectividad.

Si el proceso falla después de registrar/solicitar poda, una petición `requested` sin finalizar impide otra. Conserva `prune_audit`, backup, logs de Core y alturas; revisa el estado antes de cualquier recuperación. No hay botón de “ignorar auditoría”. La poda de Core es irreversible localmente y no se deshace restaurando su configuración.

**Mainnet: comando bloqueado.** La evidencia regtest no sustituye validación sostenida de mainnet, interoperabilidad de cartera y capacidad de disco. No hay flags `unsafe`, `no-fsync` ni poda automática del daemon.
# Poda automática coordinada

Linux, experimental, opt-in y solo Core dedicado de Signet/Testnet4/regtest. Ejemplo con rutas editadas:

```sh
mkdir -p "$HOME/electrum-prune-backups"
electrum-lite --network signet --data-dir "$HOME/electrum-data" managed-serve \
  --bitcoind /ruta/real/a/bitcoind --core-dir "$HOME/electrum-core-dedicado" \
  --rpc-port 38342 --listen 127.0.0.1:60601 --confirm-dedicated \
  --auto-prune --prune-backup-root "$HOME/electrum-prune-backups"
```

El Core root debe ser nuevo o tener la identidad creada por este supervisor; no se adopta un nodo existente. No debe haber otros escritores de cadena/RPC contra el nodo dedicado. Internamente Core usa `prune=1`, pero **el supervisor decide y ejecuta automáticamente la poda**; no es poda independiente `prune=550`. Mainnet sigue bloqueada sin override.

Cada 2016 nuevas alturas elegibles (opción `--auto-prune-interval-blocks`, mínimo 288), se detiene limpiamente el daemon, se adquiere su bloqueo exclusivo y se verifica/reproduce el archivo. Se desactiva P2P, se vacía la cola de validación de Core y se reutiliza el mantenimiento probado: identidad, prefijo coincidente, margen 288, integridad SQLite, auditoría semántica, backup completo nuevo con manifiesto y verificación de hashes, intención durable, prune RPC, verificación de indisponibilidad histórica y cierre de auditoría. Solo después se restaura el estado anterior de P2P y se reinicia el daemon. El servidor TCP no está disponible durante la ventana: las carteras deben reconectar.

En caso de fallo no se reanuda automáticamente el servicio: se detiene el Core propio. No se borran datos, backups ni auditorías para reintentar. Una auditoría `requested` pendiente bloquea el arranque del modo automático, incluso tras reinicio. Revisa causa, conserva log/backup/binario y usa los procedimientos de recuperación/restauración a otra raíz; no cambies a `prune=550` para eludir el bloqueo. El supervisor no garantiza reacción inmediata a Ctrl+C mientras espera una operación bloqueante de mantenimiento; su muerte termina el proceso Core propio mediante protección Linux de padre.

Las copias quedan en `PRUNE_BACKUP_ROOT/auto-prune-*/backup/`, siempre nuevas y nunca eliminadas automáticamente. Dimensiona **Core + archivo + SQLite + todas las copias y picos temporales**: no existe un presupuesto total garantizado, y el histórico de Lite sigue creciendo. Espacio insuficiente implica aborto sin autorizar el borrado. Los logs `Poda automática completada` incluyen altura pedida, resultado real, altura de poda observada y ruta de backup. El campo `automatic_pruning` de health sigue describiendo al daemon (false), no al supervisor.

Para deshabilitar: detener el supervisor y arrancar el mismo Core dedicado sin `--auto-prune` ni `--prune-backup-root`; conserva backups y auditorías. Para rollback del software: binario anterior más backup verificado, restaurado en raíz nueva si fuera necesario; esquema 2 y archivo ELBETA01 no cambian con esta función. Un binario previo no automatizará la poda.

Evidencia reproducible: `AUTO_PRUNE=1 BITCOIND=/ruta/a/bitcoind ELECTRUM_LITE=./target/release/electrum-lite python3 scripts/prune_smoke.py`. Crea un nodo propio, genera cientos de MiB, detiene por completo el generador antes del mantenimiento, verifica poda efectiva, TX witness/Merkle locales y cursor persistente tras reiniciar. No toca nodos existentes y no demuestra sincronización pública sostenida ni resistencia a pérdida física de energía.

**Rendimiento independiente:** las pausas de mantenimiento no cuentan como alcanzar/superar a Core. La sincronización ahora agrupa cuatro hashes y cuatro bloques en dos peticiones HTTP por grupo y procesa hasta 256 bloques por ciclo, manteniendo commits de 16 bloques/16 MB, colas acotadas, validaciones y fsync. Solo una prueba pública sostenida, midiendo por separado estas pausas, podrá demostrar co-sincronización sin retraso creciente.
