# Reescritura desde cero — diseño y estado

Versión `0.0.1-beta.1`, formato independiente `thin-v1`. El tag original no cambia.
El historial anterior sigue en Git, pero su implementación no se ha restaurado.
Código original en Rust, un paquete, RocksDB como biblioteca genérica.

## Objetivo

Core hace IBD desde génesis. Lite lee los bloques locales, conserva índices y
descarta sus cuerpos. Las consultas de cartera se resuelven localmente. Si hacen
falta bytes históricos ausentes de Core, se descarga **el bloque completo**, por
P2P, y se verifica contra la cabecera indexada. Se conserva en caché persistente.

La instalación debe ser probada primero en un regtest dedicado y sin fondos.
Mainnet/Signet/Testnet4 son redes seleccionables, NO sincronizaciones completas
validadas. No se garantiza seguir la velocidad de IBD ni un tamaño final.

## Correcciones al plan de conversación

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
- No se prometen 115–165 GB, tiempos de desarrollo, rendimiento Tor, ni velocidad
  doble a Core. Esas cifras del diálogo eran hipótesis sin mediciones.
- Una caché de capacidad ilimitada no descarga cada bloque "una vez en la vida":
  pérdida, corrupción, borrado o una futura expulsión requieren volver a obtenerlo.

## Estado del código

### Implementado para prueba inicial

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
   Ningún fallback de proxy a directo ni a API pública.
10. Checkpoint RocksDB a destino nuevo y comparación completa de claves/valores.
11. Auditoría semántica: replay completo en índice nuevo, comparando todas las
    claves y valores; se conserva la generación auditada.
12. Poda manual solo en redes de prueba: Core `prune=1`, P2P apagado, mempool
    vacía, índice sincronizado, auditoría semántica, backup nuevo y marcador durable
    antes del RPC.
13. `managed-serve`: Core dedicado con marcador/configuración/PID verificados,
    backpressure 200/20, parada de Electrum antes de auditar/podar, reanudación
    solo tras éxito y parada de los hijos propios ante error. SIGINT/SIGTERM
    sincronizan WAL y detienen Core; un proceso existente nunca se adopta.
14. Tor privado sin torrc del sistema, control con cookie y comprobación de PID,
    sin fallback directo. Las consultas locales no esperan al bootstrap público.

### Pendiente de optimización/validación

- Pipeline paralelo de fetch/compresión, batching y spool rodante.
- Validar el supervisor durante IBD sostenido en redes públicas y medir recursos;
  no recomendar `prune=550` automático. El supervisor usa exclusivamente `prune=1`.
- Esquema compacto con números de transacción anchos y resolución exacta de colisiones.
- Mempool incremental, lectores concurrentes, presupuestos globales de red/caché.
- Recuperación profunda asistida, auditoría incremental y migraciones futuras.
- Recuperación P2P sostenida con fuentes públicas y GUI Electrum/Sparrow
  (no confundir el smoke TCP con GUI). Evidencia de regtest/Tor: `verification.md`.
- Capacidad de mainnet, recursos, sincronización completa, ataques y energía.

## Almacenamiento thin-v1

Todos los enteros de las claves ordenables usan big-endian. Los importes binarios
usan little-endian. `serialize(OutPoint)` usa el formato de consenso de Bitcoin.

| Prefijo | Clave | Valor |
|---|---|---|
| identity | literal | formato y red |
| tip | literal | altura u32 |
| B | altura u32 | cabecera 80 B + TXIDs ordenados 32 B cada uno |
| T | prefijo TXID 8 B + altura u32 + posición u32 | vacío |
| H | SHA256(script) 32 B + altura u32 + posición u32 | vacío |
| C | outpoint 36 B | script hash 32 B + importe u64 + altura u32 + posición u32 |
| S | script hash 32 B + altura u32 + posición u32 + vout u32 | importe u64 |
| U | altura u32 | preimágenes de claves modificadas |

Los duplicados históricos de TXID se conservan como ocurrencias distintas en T.
Las colisiones del prefijo TXID se resuelven comparando el hash **completo** del
registro B. Historial y UTXO por script usan ocurrencias para evitar repetir TXIDs.
El hash de script se invierte únicamente en la frontera Electrum.
Cabecera, UTXO, historial, undo y tip se publican en un único WriteBatch durable.
El reinicio usa la recuperación WAL de RocksDB; no relee todos los bloques.
Un corte puede perder un sufijo del lote aún sin fsync; Core manualmente podado
conserva esos bloques y el siguiente arranque los vuelve a leer. Nunca se poda
automáticamente basándose en un cursor aún no sincronizado a disco.
Una corrupción de índice requiere preservar la raíz y restaurar un checkpoint o
reconstruir en otra raíz. No hay reparación silenciosa ni compatibilidad con SQLite.

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
La poda mainnet está prohibida en código, sin override.
Se respeta además el umbral de altura 1000 de Core 31 en redes de prueba normales.
Las auditorías de un prefijo ya podado recuperan y verifican bloques en memoria:
no los incorporan a la caché de cartera. Los índices auditados y backups sí se
conservan; sus costes de disco y mantenimiento incremental siguen pendientes de
optimización.

## Fases siguientes

1. **Base comprobable:** compilación, smoke offline, instrucciones manuales.
2. **Eficiencia:** medir replay/espacio; después batching, pipeline y mempool incremental.
3. **Operación IBD:** ampliar validación del supervisor, spool acotado y medición de carrera/poda.
4. **Endurecimiento:** cortes/ENOSPC/EIO, reorg profunda, auditorías y recuperación.
5. **Validación pública:** prefijos Signet/Testnet4 aislados; luego sincronización
   completa medible. Mainnet sin poda hasta validación de seguridad y capacidad.

Pruebas inicialmente básicas por petición del usuario. Nada de esto convierte la
beta en un servidor certificado para fondos o exposición pública.
