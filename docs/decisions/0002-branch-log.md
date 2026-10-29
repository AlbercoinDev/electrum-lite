# ADR-002 — Log de ramas y SQLite schema 2

Estado: implementado en main, manteniendo app `0.0.1-beta.1`. No se mueve el tag inicial.

## Problema

Un archivo que exige siempre altura anterior+1 no permite sustituir una rama sin truncar datos. Revertir solo SQLite tampoco basta: al reiniciar se volverían a aplicar los bloques huérfanos o la reconstrucción elegiría otra generación.

## Decisión

Mantener los registros físicos ELBETA01 y añadir semántica de log de intenciones con cursor durable en SQLite schema 2. El lector de Store habilita ramas; el lector lineal de referencia mantiene su comportamiento anterior.

- Cada bloque nuevo se prepara en SQL, se archiva/fsync y se confirma con su ubicación y cursor en una transacción.
- Para rewind se verifica que existe todo el undo, se vuelve a archivar el bloque ancestral y se sincroniza. Esa repetición de un bloque ya activo es una intención durable de volver a esa altura, no una segunda aparición de sus transacciones.
- Después se revierte descendiendo, se actualiza la referencia ancestral y se confirma el cursor. Si falla entre pasos, la recuperación detecta la intención aún no publicada y la termina. No se elimina ningún bloque del log.
- Recuperación verifica primero archivos y referencias activas/cursor. Reproduce solamente registros posteriores al cursor; los huérfanos ya procesados no se reaplican.
- Una reconstrucción desde cero reproduce tanto bloques como rewind markers en orden. Por eso reproduce también una generación ancestral donde todavía no llegó una rama nueva.

Los lectores de red comparten un único actor con el escritor: una consulta no se intercala con rollback/aplicación. No retienen transacciones SQLite durante writes al socket. En fallos/reorgs el actor no declara ready hasta reconciliar cadena y mempool contra Core.

## Migración y límites

Schema 1 requiere `migrate-index` y backup completo antes de la transacción. Schema 2 añade header de 80 bytes por bloque, cursor y prune_audit; las cabeceras de filas migradas se completan y comprueban desde el archivo durante inspect/recovery. El formato físico no se reescribe. Rollback operativo: copia previa + binario previo en otra raíz; no usar lectores lineales con un log ramificado.

Undo inicial: 288 bloques; reorg más profunda falla sin reset silencioso. `recover-chain` crea otra raíz a partir de bloques locales coincidentes y la rama canónica disponible en Core, sin modificar el origen. Si faltan bytes canónicos se requiere otra fuente confiable.

Los huérfanos no tienen GC inicial y ocupan espacio. Verificar todo el log e índices al arrancar cuesta E/S proporcional al histórico. Schema 2 no certifica durabilidad frente a hardware que ignore fsync ni implica que mainnet esté validada para poda.

## Commit de sincronización por lotes

La sincronización puede confirmar hasta 64 bloques/16 MB serializados por transacción (serve solicita hasta 20). Se preparan índices en orden y se escriben los registros; se sincronizan todos los segmentos y el directorio antes de confirmar SQLite y publicar el cursor. El lector no observa la transacción parcial. Un fallo exige escanear antes de volver a escribir; un registro incompleto falla cerrado, sin truncado automático. Registros completos posteriores al cursor se reimportan idempotentemente. La validación y el undo conservan sus reglas anteriores. No se cambia el esquema ni la configuración durable de SQLite. Una caída de proceso ensayada no demuestra comportamiento ante pérdida física de energía.
