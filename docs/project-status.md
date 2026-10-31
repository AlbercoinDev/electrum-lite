# Electrum Lite — diseño y estado

Versión `0.0.1-beta.1`, release `thin-v3-txnum-fast-2026-10-31.1`, formato de
almacenamiento `thin-v3-txnum`. Aplicación nueva,
desarrollada desde cero en Rust como un único paquete y con RocksDB como biblioteca
genérica de almacenamiento.

## Objetivo

Core hace IBD desde génesis. Lite lee los bloques locales, conserva índices y
descarta sus cuerpos. Las consultas de cartera se resuelven localmente. Si hacen
falta bytes históricos ausentes de Core, se descarga **el bloque completo**, por
P2P, y se verifica contra la cabecera indexada. Se conserva en caché persistente.

La instalación debe ser probada primero en un regtest dedicado y sin fondos.
Mainnet/Signet/Testnet4 son redes seleccionables, NO sincronizaciones completas
validadas. No se garantiza seguir la velocidad de IBD ni un tamaño final.

## Principios de diseño y límites

- **Privacidad no garantizada:** pedir bloques concretos permite inferencias y
  correlación entre peticiones. Tor reduce exposición de IP; no elimina el patrón
  de bloques ni protege de todo adversario. Sin señuelos por petición del usuario.
- El TXID no compromete el witness: verificar bloque, Merkle y witness commitment.
- Prefijos de script, aunque largos, necesitan resolución de colisiones. Esta
  versión usa los **32 bytes completos**, sin probabilidad de mezclar carteras.
- No se omite OP_RETURN del historial. Solo se excluye del conjunto UTXO.
- No se ignoran colas corruptas ni se reutilizan números globales después de una
  reorg: esta versión identifica ocurrencias por altura y posición.
- El índice **no** permite reconstruir cuerpos completos. Sin fuente histórica
  disponible, `transaction.get` falla; nunca inventa ni omite resultados.
- No existe anonimato demostrable por una prueba que busque TXIDs en paquetes:
  los propios bloques recibidos contienen transacciones. Se revisa qué mensajes
  se **envían**, sin confundirlos con los bytes recibidos.
- No se prometen tamaños de datos, tiempos de desarrollo, rendimiento Tor ni
  velocidad relativa a Core sin mediciones reproducibles.
- Una caché de capacidad ilimitada no descarga cada bloque "una vez en la vida":
  pérdida, corrupción, borrado o una futura expulsión requieren volver a obtenerlo.

## Estado del código

### Implementado para pruebas experimentales

1. CLI, red explícita, directorio separado por red e identidad persistente.
2. RPC autenticado por cookie, solo loopback, con límites y timeouts.
3. Índice RocksDB binario, WAL activo, WriteBatch atómico por bloque y fsync
   agregado cada 64 bloques o al cerrar el lote; no se publican consultas entre lotes.
4. Prevouts ordenados, historial deduplicado por transacción/script, UTXO e undo.
5. Detección de rama; preflight de profundidad y rewind transaccional hasta 288.
6. Mempool separada, reconstruida con límite de 10000 transacciones/32 MiB,
   dependencias, tasas y semántica 0/-1. Si cambia durante el muestreo, no se publica.
7. TCP Electrum 1.4, consultas, suscripciones por polling y broadcast mediante Core.
8. Descarga de bloques witness por P2P, validación y caché durable por hash.
9. Transporte directo, SOCKS5 externo o proceso Tor privado iniciado por la app.
   Ningún fallback de proxy a directo. Los bloques podados se reconstruyen con
   una ventana alineada de 16 hashes de bloque a Esplora/onion; nunca se envían
   scripthashes ni TXID.
10. Checkpoint RocksDB a destino nuevo y comparación completa de claves/valores.
11. Auditoría semántica: replay completo en índice nuevo, comparando todas las
    claves y valores; se conserva la generación auditada.
12. Poda coordinada: Core `prune=1` en modo gestionado; Lite comprueba
    `pruneheight` y pausa la red si Core adelanta al índice durable. La poda manual
    mainnet exige `--confirm-pruned-mainnet`. Sin backup externo, la recuperación
    es un re-IBD completo.
13. `managed-serve`: Core dedicado con marcador/configuración/PID verificados,
    backpressure configurable (por defecto 2000/200 bloques) y freno por espacio.
    Electrum sigue activo durante auditoría, backup y poda; Core pausa P2P para
    la fase final. Algunas consultas pueden fallar o demorarse y la difusión se
    rechaza durante la reserva. Ante error se detienen los hijos propios.
    SIGINT/SIGTERM sincronizan WAL y detienen Core; nunca se adopta otro proceso.
14. Tor privado sin torrc del sistema, control con cookie y comprobación de PID,
    sin fallback directo. Las consultas locales no esperan al bootstrap público.

### Pendiente de optimización/validación

- Ampliar y medir la lectura anticipada y los lotes RPC ya implementados; pipeline
  de compresión y spool rodante pendientes.
- Validar el supervisor durante IBD sostenido en redes públicas y medir recursos;
  no recomendar `prune=550` automático. El supervisor usa exclusivamente `prune=1`.
- Esquema compacto con números de transacción anchos y resolución exacta de colisiones.
- Mempool incremental, lectores concurrentes, presupuestos globales de red/caché.
- Recuperación profunda asistida y migraciones futuras; ampliar la validación de
  la auditoría incremental experimental ya implementada.
- Recuperación P2P sostenida con fuentes públicas y GUI Electrum/Sparrow
  (no confundir el smoke TCP con GUI). Evidencia de regtest/Tor: `verification.md`.
- Capacidad de mainnet, recursos, sincronización completa, ataques y energía.

## Almacenamiento thin-v3-txnum

Todos los enteros de las claves ordenables usan big-endian. Los importes binarios
usan little-endian. `serialize(OutPoint)` usa el formato de consenso de Bitcoin.

| Prefijo | Clave | Valor |
|---|---|---|
| identity | literal | formato y red |
| tip | literal | altura u32 |
| B | altura u32 | cabecera 80 B + TXIDs ordenados 32 B cada uno |
| N | altura u32 | primer TxNum u32 + número de transacciones u32 |
| T | prefijo TXID 8 B + TxNum u32 | vacío |
| H | SHA256(script) 24 B + TxNum u32 | vacío |
| C | outpoint 36 B | script hash 24 B + importe u64 (sin altura/posición) |
| S | script hash 24 B + TXID 32 B + vout u32 | altura u32 + importe u64 |
| P | último TxNum del trozo u32 | offsets/tamaños de hasta 64 transacciones |
| U | altura u32 | preimágenes y marcadores de restauración de UTXO |

Cada transacción recibe un `TxNum` secuencial global. Historial y búsqueda por
TXID usan 4 B de TxNum en lugar de altura+posición. El UTXO se identifica por
script y outpoint, de modo que `/rest/spenttxouts` permite borrarlo sin leer la
fila `C` del prevout. Las colisiones del prefijo TXID se resuelven comparando el
hash completo del registro B. Los duplicados históricos de TXID se conservan.
El hash de script usa 192 bits (24 B) y se invierte únicamente en la frontera
Electrum. La probabilidad de colisión es despreciable; no hay post-filtrado.
Cabecera, UTXO, historial, undo y tip se publican en un único WriteBatch durable.
El commit rápido usa `/rest/spenttxouts` y no hace lecturas aleatorias de `C` por
entrada. El undo guarda un marcador para las filas `S` gastadas y resuelve la
altura original por `T` solo al revertir un bloque. El reinicio usa la
recuperación WAL de RocksDB; no relee todos los bloques.
Un corte puede perder un sufijo del lote aún sin fsync; Core manualmente podado
conserva esos bloques y el siguiente arranque los vuelve a leer. Nunca se poda
automáticamente basándose en un cursor aún no sincronizado a disco.
Una corrupción de índice requiere preservar la raíz y restaurar un checkpoint o
reconstruir en otra raíz. No hay reparación silenciosa ni compatibilidad con SQLite.

## Modo rápido y presupuesto

Lite detecta CPU/RAM/cgroup/disco y reserva margen para el sistema, Core y Tor.
`--fast-sync` agrupa 32 bloques por `WriteBatch`, desactiva WAL/autocompacciones,
hace flush periódico, omite Merkle/witness de bloques de Core, no genera `txpos`
y usa `spenttxouts=auto` (C en Signet/regtest, REST en mainnet). Al alcanzar el
tip hace flush y vuelve al modo normal.

La reorg en fast-sync no usa undo: reindexa el bloque obsoleto, borra sus claves
y restaura UTXO desde `spenttxouts`; requiere Core REST. `server.health` publica
`index_rate`, `bulk`, `durable_height` y el último lote.

## Fase 4 opcional: fuera del alcance de esta release

Las siguientes optimizaciones se evaluaron y **no se incluyen** en esta release:

- fichero plano de TXIDs (`txnum * 32`) para sustituir la parte de TXIDs del LSM;
- parser cero-copia con `bitcoin_slices`;
- eliminar por completo la tabla `C` y resolver la mempool con `gettxout`.

Motivo: cambian el formato de almacenamiento o el modelo de consulta y exigen
reindexado/migración y una nueva ronda de validación. No son necesarias para el
objetivo actual (budget automático, fast-sync, lotes, reorg por reindexado y
verificación selectiva). Quedan como trabajo futuro, no como deuda oculta.

## Concurrencia inicial

Un worker dueño del índice serializa RPC, actualizaciones y consultas. La cola es
acotada; conexiones TCP independientes atienden mensajes y notificaciones. Es una
primera versión comprobable, no la arquitectura final de máximo rendimiento.
Una descarga histórica lenta puede retrasar consultas: documentado, no ocultado.
`server.health` usa una instantánea de progreso publicada al terminar cada lote,
sin abrir el índice ni esperar en la cola de consultas. Esto evita que un lote
grande bloquee al supervisor. La cancelación se comprueba entre bloques; al cerrar
se sincroniza el WAL antes de liberar el proceso de Electrum.

## Seguridad de poda

Usar **prune=1** en un Core dedicado. Lite no modifica la configuración de un
nodo existente. Nunca prueba poda en datos valiosos.
`prune=550` automático puede adelantarse al índice; si faltan bloques, se falla.
`managed-serve --confirm-dedicated` autoriza backpressure sobre su propio Core;
`--auto-prune` autoriza mantenimiento en redes de prueba, no sobre nodos ajenos.
Una poda solicitada pero sin completar deja `prune-pending` y bloquea más podas.
No borrar ese marcador para saltarse el problema: inspeccionar Core y recuperar.
La poda mainnet solo se permite en modo gestionado o con `--confirm-pruned-mainnet`; exige que Core pode por detrás del tip durable de Lite y que el usuario acepte que la recuperación es un re-IBD completo.
Se respeta además el umbral de altura 1000 de Core 31 en redes de prueba normales.
Las auditorías de un prefijo ya podado recuperan y verifican bloques en memoria:
no los incorporan a la caché de cartera. Tras un mantenimiento gestionado
exitoso se conserva la pareja auditada y el backup más recientes y se eliminan solo
parejas antiguas reconocidas y verificadas; las generaciones incompletas o
desconocidas se conservan. Los costes de disco siguen pendientes de medición.

## Fases siguientes

1. **Base comprobable:** compilación, smoke offline e instrucciones manuales.
2. **Eficiencia:** medir replay/espacio y batching existente; ampliar pipeline y mempool incremental.
3. **Operación IBD:** ampliar validación del supervisor, spool acotado y medición de carrera/poda.
4. **Endurecimiento:** cortes/ENOSPC/EIO, reorg profunda, auditorías y recuperación.
5. **Validación pública:** prefijos Signet/Testnet4 aislados; luego sincronización
   completa medible. Mainnet sin poda hasta validación de seguridad y capacidad.

Pruebas inicialmente básicas por petición del usuario. Nada de esto convierte la
beta en un servidor certificado para fondos o exposición pública.
