# Evidencia local — reescritura thin

Fecha: 2026-10-30. Linux x86-64, Rust 1.99.1, Bitcoin Core 31.0.0.
Pruebas de laboratorio offline, no sincronización pública ni GUI.

## Detección de poda durante IBD — cambios locales posteriores

`scripts/managed_prune_smoke.py --bitcoind /tmp/opencode/bitcoin-31.0/bin/bitcoind
--electrum-lite target/release/electrum-lite --temp-dir target/prune-smoke`
ejercitó dos nodos regtest desechables offline. Los bloques fueron generados con
fechas antiguas y se comprobó `initialblockdownload=true` antes y después de la
poda. La salida registró eliminación física de `blk00000.dat`, `pruneheight=743`
y reducción de archivos `blk` de 435757599 a 301586404 bytes. Recuperó por P2P
una transacción witness byte-exacta y verificó Merkle; un bloque más no provocó
un mantenimiento redundante. Un `prune-pending` inyectado bloqueó el reinicio.
No es una prueba de sincronización pública ni de todos los reorgs o fallos de I/O.

Una repetición posterior usó el intervalo predeterminado de **1** bloque (sin
pasar `--prune-interval`): durante IBD borró `blk00000.dat`, `pruneheight=743`,
435757601 → 301586405 bytes; también pasaron recuperación witness, Merkle y la
comprobación de no repetir mantenimiento con el siguiente archivo no elegible.
La política de espera tras una poda sin archivos borrados se comprobó mediante
un test unitario; esa rama aún no se ha provocado con un bloqueo real de Core.

Una ejecución adicional incorporó la retención automática: tras la poda física
durante IBD y la verificación de la pareja nueva, eliminó una pareja anterior
completa sintética y conservó otra con un archivo ajeno y una auditoría huérfana.
También verificó witness, Merkle y el bloqueo por `prune-pending`. Las pruebas
unitarias cubrieron enlaces simbólicos, operación pendiente y discrepancia entre
auditoría y backup. No se utilizaron ni eliminaron datos de un nodo existente.

La preparación experimental de una base para auditoría incremental se probó
copiando una generación cerrada, incluido su WAL, a un destino nuevo y comparando
todas las claves. La prueba rechazó una rama divergente y confirmó que extender
la copia no modificó la fuente. Un checkpoint de RocksDB creado desde una apertura
de solo lectura perdió datos en la prueba y no se utiliza. La poda selecciona una
base con registro completo y pareja coincidente, comprueba la ascendencia con Core,
reproduce el tramo nuevo y compara el estado completo. Un test de RPC simulado
verificó que solo se solicita el bloque nuevo, que una rama divergente provoca
replay completo y que un error RPC aborta. La prueba física de dos podas terminó
con un error del entorno y no tiene resultado concluyente. Electrum sigue detenido
durante auditoría y poda, y se reinicia antes de limpiar generaciones antiguas;
este nuevo orden no se ha probado con una poda física de extremo a extremo.

Trabajo local posterior: el smoke regtest sencillo creó un checkpoint mediante
un socket Unix en un directorio privado, rechazó un destino inválido y uno ya
existente, comprobó que el método no está expuesto por Electrum TCP y confirmó que
`server.ping` respondía después. No se ha medido disponibilidad durante
checkpoints grandes.
Otra prueba sencilla mantuvo bloqueada una auditoría de una copia de ese
checkpoint y comprobó que el servidor Electrum vivo respondió a `server.ping`.

Trabajo local posterior: el proceso de auditoría puede abrir un checkpoint
existente de `supervisor/snapshot-ALTURA-SELLO` sin abrir el índice activo y
utiliza la caché histórica de la raíz original y bases verificadas. El smoke
regtest mantuvo esa auditoría detenida en una petición de bloque mientras
Electrum respondió a `server.ping`, y rechazó un nombre con altura distinta sin
crear la auditoría. No se ha probado una poda preparada con instantánea.
Una comprobación local adicional compara la auditoría con la instantánea y esta
con el índice bloqueado, incluyendo los undo retenidos. Un test rechazó usar el
mismo directorio, modificar el índice vivo a igual altura y modificar el estado
auditado. La igualdad no prueba que la reproducción se haya ejecutado: el
supervisor también comprueba el fin correcto de su propio proceso auditor.
En trabajo local, el servidor gestionado con `--auto-prune` crea también un
socket privado en un directorio nuevo con permisos 0700. El smoke dedicado
solicitó una instantánea mediante ese socket mientras el servidor seguía activo.
Las rutas de socket Unix largas se rechazan; los directorios privados de ejecuciones
anteriores se conservan y no se adoptan.
El smoke gestionado también ejecutó un auditor separado sobre el checkpoint y
comprobó que Electrum respondió después. Otra prueba hizo avanzar Core mientras
la auditoría estaba detenida: la auditoría falló y conservó la instantánea y el
resultado parcial. En otra ejecución regtest desechable, el flujo automático
completo terminó la auditoría antes de detener Electrum y realizó dos podas
físicas; la segunda reutilizó una base incremental verificada. La primera borró
`blk00000.dat` (435757602 → 301586406 bytes; `pruneheight=743`); pasaron la
recuperación witness, prueba Merkle, limpieza de checkpoint temporal, retención
conservadora y bloqueo por `prune-pending`. No demuestra disponibilidad
ininterrumpida: Electrum se detiene para backup, verificación final y poda.
Una prueba regtest separada rechazó una prueba preparada inválida y una
instantánea desactualizada antes de crear el backup o llamar a la poda; conservó
las generaciones y el `pruneheight` no cambió. No es una simulación exhaustiva de
caídas o fallos de disco.

Trabajo local posterior: el backup se crea a partir de la instantánea mientras
el servidor sigue activo. Un smoke regtest gestionado creó auditoría y backup por
separado, rechazó sobrescribir el backup y comprobó `server.ping` después. Otra
prueba rechazó un backup de otra altura sin podar ni eliminar generaciones. La
comparación final del índice con el backup incluye todas las claves y undo. Una
ejecución posterior del smoke gestionado verificó el orden auditoría, backup,
desconexión y poda y completó dos podas físicas regtest; la primera borró
`blk00000.dat` (435757602 → 301586406 bytes, `pruneheight=743`). Pasaron también
la retención conservadora, witness, Merkle y el bloqueo por `prune-pending`.
Un test unitario adicional creó un backup desde el checkpoint y rechazó una
modificación de undo aunque la altura siguiera siendo la misma.

Trabajo local posterior: el servidor puede iniciar por el socket privado una
comparación del índice, auditoría, instantánea y backup en un hilo lector mientras
el trabajador evita modificar el índice. En regtest esa comparación terminó,
se rechazó un backup corrupto y Electrum respondió a `server.ping`; el método no
está expuesto por TCP. La prueba no demuestra que `ping` llegara mientras la
comparación aún estaba ejecutándose ni mide latencia con bases grandes. Tras una
comparación correcta se mantiene una reserva experimental de 30 segundos; un
regtest verificó que no se sirve una cabecera obsoleta si Core avanza y que la
liberación explícita reanuda la sincronización. Una prueba integrada separada
comprobó también el vencimiento automático. El resultado aislado no autoriza
podar: el supervisor local ejecuta después la poda dentro de la reserva.

Trabajo local adicional: la fase irreversible se separó de la CLI en
`src/prune.rs`, con la misma intención durable y comprobación de Core antes de
la RPC. Una prueba RPC simulada falló después de crear la intención y confirmó
que permanece `prune-pending` sin registro de finalización. Localmente se ha
conectado al servidor mediante el socket privado: un regtest con 1000 bloques
pequeños rechazó una prueba inválida y ejecutó la RPC de poda con el servidor
activo, conservando el registro durable. Esa prueba no provocó ni demostró
borrado físico. Una prueba física posterior con el supervisor integrado y la
barrera de difusión conservó el mismo proceso y una conexión TCP durante dos
podas; borró `blk00000.dat` (435757596 → 301586401 bytes; `pruneheight=743`).
También pasaron recuperación witness, Merkle, auditoría incremental y retención.
La conexión se comprobó antes y después del mantenimiento, no se midió cada
consulta durante la poda. No se verificaron todos los posibles fallos.

La caducidad real de la reserva de 30 segundos pasó un smoke regtest: después
del vencimiento el servidor rechazó su liberación y pudo volver a verificar. La
reserva no caduca durante la RPC de poda; si el hilo de poda falla, las consultas
de cadena quedan deshabilitadas y se preserva cualquier intención pendiente.
Un regtest adicional inyectó un error en `pruneblockchain`: se conservó
`prune-pending`, el servidor rechazó consultas de cadena y la liberación de la
reserva, y `server.ping` siguió respondiendo. El servidor rechaza también la
difusión de transacciones durante la reserva o poda, para evitar cambios del
mempool desde Electrum; esta barrera pasó un smoke con compilaciones debug y
release. La prueba física conservó una sesión, pero no inyectó una difusión
durante la RPC de poda.

Trabajo local posterior: un cambio de punta confirmado durante la auditoría se
clasifica con un código distinto de los errores RPC y aplaza el mantenimiento.
En regtest gestionado se pausó el auditor, se avanzó Core y se confirmó que el
supervisor eliminó solo los temporales reconocidos, mantuvo ambos procesos vivos
y reintentó hasta completar dos podas. Los cambios adicionales para detectar una
punta obsoleta al solicitar la instantánea o tras el backup pasan pruebas sencillas.
Una compilación se efectuó mientras un smoke físico estaba en curso, por lo que
ese resultado no se considera prueba íntegra de un único binario final. No se han
inyectado las carreras de instantánea y backup de extremo a extremo.

La investigación de poda con pares conectados está en
`docs/p2p-pruning-investigation.md`. Un test RPC simulado adicional comprobó que,
con P2P activo, la fase irreversible rechaza la operación antes de llamar a
`pruneblockchain` y conserva la intención pendiente. No se ha implementado poda
conectada ni se han relajado las comprobaciones existentes.

Un regtest dedicado desechable arrancó con `--core-dbcache-mib 256`; se verificó
en los argumentos del proceso Core `-dbcache=256` y se detuvo limpiamente. Este
ajuste limita esa caché de Core, no la RAM total, y no es una medición de velocidad.

La preparación anticipada local se comparó en el mismo Core regtest aislado con
301 bloques pequeños, alternando versiones y usando índices nuevos. Tiempos de
un ensayo: anterior 1,157 y 1,153 s; nueva 1,158 y 1,160 s. No se observa una
mejora en esta carga; no representa Signet ni mide RAM o disco bajo presión.
`scripts/prefetch_smoke.py` también inyectó un error RPC en altura 20: no pidió
una altura posterior, falló sin colgarse y, al reanudar, alcanzó altura 300.
Una segunda inyección devolvió un bloque de otra rama en altura 20: el escritor
lo rechazó, el lector no pasó de la altura 21 y el reinicio alcanzó altura 300.
La equivalencia completa del índice y los límites de memoria bajo presión no se
han medido en esta comparación.

Una carga adicional de regtest con 301 bloques y 500 transferencias de cartera
produjo 1,168/1,164 s en la versión anterior y 1,150/1,154 s con lectura
anticipada; no es una mejora concluyente. En la versión nueva, los tiempos
acumulados fueron aproximadamente 503–505 ms en RPC de hashes, 570–577 ms en
RPC de bloques más decodificación y 52–56 ms indexando. La lectura y escritura se
solapan, por lo que no deben sumarse como tiempo transcurrido. Es una carga
pequeña y sintética, no una medición de Signet o de un índice grande. Sugiere
examinar RPC agrupado antes de optimizar RocksDB, sin establecer el cuello de
botella de la VM del usuario. El freno de seguridad no se ha modificado.

Después se agruparon consultas de hasta 16 hashes por RPC, comprobando todas las
respuestas antes de utilizarlas. En otra ejecución aislada de la misma carga de
301 bloques y 500 transferencias: versión anterior 1,173/1,172 s; versión nueva
0,698/0,690 s; tiempo agregado de RPC de hashes 42/41 ms. Es una comparación
pequeña local, no una garantía de velocidad ni una medición Signet. La prueba
inyectó un error en un lote y un hash de otra rama, verificando que se rechazan
y que se puede reiniciar; también comprobó el límite de hashes solicitados y de
bloques adelantados. Los tests unitarios rechazan respuestas ausentes, repetidas,
erróneas y malformadas. El smoke regtest incluye una reorganización real. Los
resultados previos de límite de altura se refieren al lector sin lotes.

`scripts/disk_guard_smoke.py` arrancó un Core regtest dedicado con una reserva
superior al espacio realmente disponible y confirmó el evento de pausa por disco
y `networkactive=false`; tras detenerlo, lo reanudó con reserva inferior y
confirmó `networkactive=true`. Los tests unitarios cubren los dos umbrales de
retraso y la histéresis de 512 MiB. No se ha probado un agotamiento real de disco,
ni una poda física con la nueva política. La comprobación periódica no garantiza
evitar ENOSPC si el espacio cae rápidamente o se necesita para auditoría/backup.

## Build

Se ejecutó sobre el árbol nuevo:

```text
cargo fmt --check                                      OK
cargo clippy --locked --all-targets -- -D warnings      OK
cargo test --locked                                    5 passed; 0 failed
cargo build --release --locked                         OK
```

Los cinco tests cubren restricción de RPC a loopback sin userinfo, rechazo de
hash erróneo, rechazo de witness completamente eliminado aunque el TXID/Merkle
coincidan, ausencia de conexión directa tras un fallo SOCKS y persistencia/undo
con ocurrencias duplicadas de TXID.

## Prueba TCP extremo a extremo

```sh
python3 scripts/smoke_regtest.py \
  --bitcoind /tmp/opencode/bitcoin-31.0/bin/bitcoind \
  --bitcoin-cli /tmp/opencode/bitcoin-31.0/bin/bitcoin-cli \
  --electrum-lite target/release/electrum-lite
```

Resultado real:

```text
Verified checkpoint: .../backup (historical cache is replaceable and not included)
Semantic replay verified at height 103: .../audit
PASS: initial sync, JSON batch, mempool/RBF, raw/verbose tx, subscriptions, balance, UTXO, Merkle proof
PASS: simulated missing Core block -> witness-block P2P -> persistent cache; corrupt witness rejected
PASS: reorg, restart, verified backup, semantic replay audit
PASS: offline disposable regtest smoke (no existing node touched)
```

La primera ejecución encontró un problema del generador de prueba: al invalidar
un bloque y minar inmediatamente podía recrearse el mismo bloque marcado inválido.
Se corrigió avanzando el mocktime para la nueva rama; la ejecución posterior pasó.
No se disfrazó este caso de prueba como un error del índice.

La ausencia histórica fue **simulada** mediante un proxy RPC loopback. El bloque
se descargó realmente por Bitcoin P2P de un Core local aislado. Se modificó un
byte witness de la caché y fue rechazado. No se borraron bloques físicamente.

## Instalación comprobada

`sh install.sh` terminó correctamente y dejó el ejecutable en:

```text
/home/albit/.local/bin/electrum-lite
electrum-lite 0.0.1-beta.1
```

SHA256 idéntico para `target/release/electrum-lite` y el ejecutable instalado:

```text
8b8a78a6a9eb41ea3d4bcd3c7fe652db5d479cb6e55ab757aaec7f37c60a2733
```

`ldd` solo requiere el loader GNU, libc, libm, libgcc_s y libstdc++; libclang no es
dependencia de ejecución. Eso no demuestra compatibilidad con cualquier Linux.
Se ejecutó también `scripts/devnet.py` con una raíz nueva: creó 101 bloques e
imprimió los comandos manuales; después se detuvo su Core dedicado.

## Historial preservado

Se recuperó `.git` desde el remoto, no el código anterior. HEAD histórico:
`a971a2d1077b5a6805c94e6541f8004095ae1389`.
Tag original intacto:
`v0.0.1-beta.1 -> accc8cd00c061fa2a1cff9455826d89d76584351`.
En el momento de esa comprobación, la reescritura seguía sin publicar; esta
sección registra el estado histórico, no el estado actual de GitHub.

## Ampliación: supervisor y Tor opcional

La compilación ampliada supera nuevamente fmt, Clippy y release. Ahora son
**7 tests unitarios**: añade rechazo de directorios Core ajenos y el bloqueo
duro de poda automática mainnet, antes de iniciar procesos.

SHA256 del ejecutable ampliado:

```text
5fed1a39f75abb926cfd2a137cd6e4c8cdd880a3ada743ec61cc002ca6b8e150
```

Se volvió a ejecutar el smoke TCP completo sobre esa compilación: todos los
PASS anteriores, incluyendo audit/restart/reorg/RBF, siguen pasando.

### Tor real sin fallback

Se utilizó el ejecutable real Tor 0.4.9.15 del paquete Debian, sin instalar un
servicio global ni reutilizar torrc/datos del usuario:

```sh
python3 scripts/tor_smoke.py \
  --bitcoind /tmp/opencode/bitcoin-31.0/bin/bitcoind \
  --tor-bin /tmp/opencode/tor-runtime/usr/bin/tor
```

Resultado observado:

```text
PASS: actual private Tor process started and identified over cookie-authenticated control
PASS: rejected Tor request and stopped Tor both fail closed; sentinel observed zero direct connections
PASS: explicit direct mode retrieves byte-exact witness transaction from local P2P; Tor remains optional
```

El caso offline desactiva la red de Tor de forma deliberada. Se encontró que
`DisableNetwork=1` cierra también SOCKS; por eso la app autentica y comprueba el
PID por el control privado, sin exigir que SOCKS/bootstrap estén disponibles para
servir consultas locales. Ante la falta de Tor, el histórico remoto falla.

### Tor público + P2P real (solo génesis)

```sh
python3 scripts/tor_public_smoke.py \
  --bitcoind /tmp/opencode/bitcoin-31.0/bin/bitcoind \
  --tor-bin /tmp/opencode/tor-runtime/usr/bin/tor --allow-public
```

Resultado observado:

```text
PASS: real Tor public bootstrap and Bitcoin P2P witness-block retrieval; byte-exact mainnet genesis transaction verified
PASS: Core stayed offline at height 0; only genesis requested; no wallet or public API used; not full mainnet sync
```

Tor alcanzó bootstrap 100%. La transacción génesis descargada por el transporte
P2P real de la app coincidió byte a byte con Core. Ninguna cartera ni consulta
de scripthash fue enviada fuera. Esto **no demuestra anonimato**, rendimiento,
disponibilidad de cualquier histórico ni sincronización completa de mainnet.

### Supervisor: IBD P2P, poda física y recuperación

```sh
python3 scripts/managed_prune_smoke.py \
  --bitcoind /tmp/opencode/bitcoin-31.0/bin/bitcoind \
  --electrum-lite target/release/electrum-lite \
  --temp-dir target/regtest-smoke-tmp
```

Dos nodos regtest dedicados offline. El archivador minó **601 bloques pequeños
y 450 de unos 950 kB** (cerca del peso máximo por su tamaño sin witness).
El Core del supervisor hizo IBD **por P2P**, no por copia de archivos ni submitblock.
Se verificaron pausa/reanudación por lag, rechazo de un segundo gestor intentando
adoptar el mismo Core y cierre limpio de los hijos propios.

Resultado real de la compilación ampliada:

```text
PASS: owned startup, lag pause/resume and clean managed shutdown
PASS: physical deletion ['blk00000.dat'], pruneheight=743, blk bytes 435757598 -> 301586402
PASS: original witness transaction recovered over P2P after deletion; repeat served from cache with archival P2P disabled; local Merkle proof valid
PASS: second semantic audit streamed the pruned prefix over P2P without growing the wallet cache
PASS: pending prune blocks automatic restart; no existing node used
```

No es únicamente un RPC exitoso: se comprobó la desaparición del archivo y el
rechazo `getblock` para el bloque histórico, antes de recuperar la transacción
original desde el otro Core por P2P. Después se apagó P2P del archivador y se
repitió la consulta desde caché, con prueba Merkle calculada localmente.
Una segunda operación auditó el prefijo ya podado sin añadirlo a la caché.
Finalmente se inyectó un intento inconcluso y se comprobó que el modo automático
no arrancaba otro Core. Todos los nodos se detuvieron; la raíz temporal se eliminó.

Hallazgos corregidos durante la prueba:

- La salud compartía la cola del indexador y un lote grande podía bloquear al
  supervisor más de 10 segundos. Ahora es una instantánea publicada independiente;
  la cancelación entre bloques permite cerrar el worker y sincronizar WAL.
- El primer volumen de 551 bloques no era podable con Core normal: exige al menos
  1000 en estas redes. Se añadió preflight y se llevó la prueba a altura 1051,
  sin `fastprune`. El rechazo previo dejó el intento incierto protegido, sin
  reanudación automática ni borrado de marcadores para eludir el bloqueo.

Las cifras de bytes corresponden **solo a los archivos blk del Core podado**,
no al total de Core + archivador + índice + backups ni al pico temporal completo.
No son una medición de capacidad/rendimiento mainnet.

### Instalación ampliada comprobada

Se ejecutó nuevamente `sh install.sh` tras las pruebas y se verificó SHA256
idéntico entre `target/release/electrum-lite` y
`/home/albit/.local/bin/electrum-lite`:
`5fed1a39f75abb926cfd2a137cd6e4c8cdd880a3ada743ec61cc002ca6b8e150`.
El `--help` instalado incluye `managed-serve`, backpressure, `--auto-prune` y Tor
opcional. La versión sigue siendo `0.0.1-beta.1`; el binario anterior queda en
`electrum-lite.previous`. La instalación no inicia servicios ni nodos.

El paquete actual se denomina
`dist/electrum-lite-thin-managed-experimental-linux-x86_64.tar.gz`; el paquete
básico anterior no se sobrescribe. Son artefactos locales sin firma y no son
construcciones del tag inicial.

## No demostrado

Interoperabilidad GUI, IBD público sostenido, recuperación profunda, fuentes
públicas sostenidas, tamaño mainnet, presupuestos RSS y cortes físicos de energía.
La prueba física anterior es regtest; no acredita poda segura en mainnet ni
co-sincronización sostenida en Signet/Testnet4. La poda mainnet sigue bloqueada.
La evidencia acredita una **versión para probar**, no la finalización de todas
las optimizaciones ni aptitud para fondos reales.
