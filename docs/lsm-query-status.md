# LSM: consultas de generación, 2026-10-30

[DECIDIDO] Continuar la nueva implementación LSM; el banco comparativo con el
backend anterior queda aparcado a petición del usuario. La ejecución grande
interrumpida no produjo informe y NO cuenta como evidencia de rendimiento.
Se confirmó ausencia de procesos activos y se eliminó su directorio de 30 MiB.

## Implementado y probado

[CONFIRMADO] `src/experimental_lsm/query.rs` añade la API de lecturas confirmadas:

- Snapshot KV que obtiene generación y valores desde la misma snapshot RocksDB.
  El coordinador entrega ese lector junto con archivo y estado de la generación.
- Saldo por script en índice agregado durable, actualizado atómicamente con
  gastos, outputs, sobrescrituras BIP30, undo y cursor. Consulta de saldo sin
  recorrer el historial o todas las monedas.
- Historial y lista de UTXO con límites explícitos: error, nunca truncado.
  La vista de script reúne generación, punta, saldo, historial y monedas del
  mismo snapshot; comprueba la suma contra el agregado y ambos índices UTXO.
- Cabeceras persistidas en el lote de la generación: chunks de hasta 2016,
  sin leer bloques completos para responder cabeceras y sin formato Electrum 1.6.
- Ocurrencias por TXID, bloque y posición; orden por altura/posición. Consulta
  de bytes originales, incluidos witness, con altura opcional y límite de bytes.
  Sin altura elige la ocurrencia canónica más reciente, sin asumir unicidad TXID.
- Prueba Merkle local, duplicación de último nodo en capas impares, comprobación
  de raíz y hash de cabecera. Archive::read verifica también witness commitment:
  la prueba Merkle por sí sola NO protege los bytes witness.
- Historial de cartera excluye génesis; archivo, transacción y prueba lo retienen.
  Scripthash interno sigue siendo SHA256 raw; inversión solo en futura frontera
  Electrum. Las consultas no necesitan Core histórico.

Nuevos namespaces `A` (cabeceras) y `V` (saldo). Se incluyen en auditoría semántica,
undo, recuperación y backup/restauración completos. Identidad experimental
`lite-bitcoin-lsm-v2`: una raíz v1 se RECHAZA, sin adaptación ni migración silenciosa.
No cambia ELBETA01 ni la versión de aplicación `0.0.1-beta.1`.

## Evidencia

```sh
cargo fmt --check
cargo clippy --release --locked --lib --tests --features experimental-lsm -- -D warnings
cargo test --release --locked --lib --tests --features experimental-lsm
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

[CONFIRMADO] Formato, Clippy y build normales correctos. **71 tests con la
feature**, incluidos 14 de integración LSM; 52 tests normales sin feature.
Pruebas adicionales de esta entrega:

1. Snapshot KV conserva dato y generación anteriores después de un WriteBatch
   posterior, incluyendo su iterador de prefijo.
2. Vista de cartera completa, agregado, cabeceras, transacción witness byte-exact,
   pruebas Merkle y rechazo de límites; génesis sin saldo ni historial de cartera.
3. Rewind elimina cabeceras/ocurrencias/saldo de la rama anterior; nueva rama y
   backup restaurado conservan consultas y generación nuevas.
4. Capas Merkle impares y rechazo de ramas manipuladas.
5. TXID repetido en bloques diferentes, consulta explícita por altura, selección
   de ocurrencia reciente y retirada correcta de la ocurrencia tras rewind.
6. Raíz experimental v1 rechazada; sigue pudiendo abrirse como v1 después del
   rechazo, sin conversión automática.

Las nuevas pruebas son locales, deterministas y LSM-only. La suite existente
contiene también verificaciones históricas del backend anterior, ejecutadas
como regresión, no como nuevo banco de rendimiento. No se ejecutó otro banco
de comparación ni se descargó cadena pública en este bloque.

Todos los datos de tests usan TemporaryDirectory. Se eliminan también los
builds y herramientas temporales propios al cerrar el ensayo. Solo se publica
código fuente/evidencia: no se empaqueta un binario desde el árbol con cambios
pendientes de catch-up, ni se cambia la pareja Lite/Core instalada públicamente.

## Sigue pendiente

[PENDIENTE] Conectar este API al nuevo sincronizador, mempool separada y servidor
Electrum; readiness requiere alcanzar Core observado y mempool completa.
El préstamo del snapshot impide mutar el coordinador hasta soltarlo: todavía no
se ha implementado un lector de archivo concurrente independiente. Las consultas
se usarán primero desde un worker serializado y acotado, fuera de tareas async.

Faltan caps acumulados, optimización del replay semántico de arranque, transporte
binario opcional, pruebas de fallos de E/S, integración de poda LSM y ensayo
sostenido Signet/Testnet4. No hay demostración de rendimiento superior a Core,
sincronización pública completa ni permiso de poda mainnet.

El lanzador publicado sigue sirviendo el motor anterior hasta publicar un
servicio LSM integrado y validado. Las instrucciones Signet existentes NO deben
presentarse como ejecución del nuevo LSM todavía.
