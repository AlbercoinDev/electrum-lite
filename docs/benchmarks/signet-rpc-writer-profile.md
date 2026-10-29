# Perfil RPC y escritor en disco — meta todavía no cumplida

Todas las co-sincronizaciones de esta nota usaron nodos Signet nuevos y disco ext4 con espacio suficiente, sin poda, sin mantenimiento, sin pausas P2P ni otras pruebas de carga concurrentes. Son ventanas públicas acotadas, no sincronizaciones completas. Los tramos y pares no son idénticos: no constituyen una comparación A/B controlada. Core 31.0, dbcache 64 MiB. Cada ensayo conservó metadatos, SHA-256 del ejecutable, JSONL y logs bajo `~/.local/share/electrum-lite-experiments/` en la máquina de prueba.

## Perfil inicial: commits 16, RPC 4, caché SQLite 64 MiB

69 muestras válidas durante una ventana de 240 s (242 s incluyendo cierre). Primera muestra: Core 6354 / Lite 255. Última: Core 96 659 / Lite 26 367; retraso final máximo 70 292. `NOT_DEMONSTRATED`, mantenimiento 0 s, pausas P2P 0.

Medianas del último ciclo observado de 256 bloques, en ms: total **2172**, espera **522**, preparación/commit agregado **651**, comprobaciones de rama antes de commit **743**. Los grupos RPC sumaron medianas de 5 170 511 µs para hashes y 6 846 902 µs para bloques, **sumados entre workers**, no tiempo de pared; no se deben sumar a las otras métricas como fases excluyentes.

## Generaciones 64 / RPC 8

86 muestras válidas en una ejecución de 300 s. Core 6017 → 120 183; Lite 511 → 40 447. Medianas por ciclo: total **1352 ms**, comprobaciones de rama **89,5 ms**, commit **422 ms**, espera **458 ms**. El retraso siguió creciendo. El ahorro en comprobaciones no bastó para cumplir la meta.

Ensayo más largo (602 s / límite de 2 GiB recibidos): Core 5981 → 222 076, Lite 255 → 51 967; retraso final 170 109. 141 muestras válidas, `NOT_DEMONSTRATED`. A mayores alturas cambió el coste dominante: commit mediano **2246 ms** de un ciclo mediano de **3189 ms**. Las sondas RPC independientes dieron medianas de **2,87 ms** para `uptime` y **20,46 ms** para búsqueda de altura, con picos de 414 ms en esta última: evidencia de contención/latencia dependiente de la consulta, no prueba de un fallo de TCP.

Una medición diagnóstica adicional, con P2P desactivado y **sin contar para la meta**, separó cuatro generaciones de 64 bloques en el histórico propio:

```text
STORE_PROFILE blocks=64 prepare_us=754861 append_us=22149 sync_us=3221 sqlcommit_us=321130
STORE_PROFILE blocks=64 prepare_us=662692 append_us=16734 sync_us=2740 sqlcommit_us=385386
STORE_PROFILE blocks=64 prepare_us=681280 append_us=17219 sync_us=3092 sqlcommit_us=311678
STORE_PROFILE blocks=64 prepare_us=673206 append_us=16465 sync_us=2792 sqlcommit_us=300435
```

La espera RPC cayó a 40 ms, pero el commit agregado de 256 bloques siguió en 4206 ms. Ya no era correcto atribuir todo el problema a RPC.

## Ensayos descartados y overlay UTXO

Se probaron generaciones de 256 y caché sugerida de 256 MiB: sin éxito sostenido. No se adoptan ni el aumento de caché ni las generaciones de 256. El default permanece en 64 MiB y la generación se limita a **64 bloques / 16 MB**; nunca se omiten fsync, comprobación de rama por generación, DELETE/EXTRA ni claves foráneas.

Se implementó un overlay **temporal por bloque**, no otro índice persistente: precarga prevouts, resuelve gastos en orden y aplica solo el delta UTXO final en lotes. Guarda undo del estado anterior y marcadores de ausencia para outputs gastados dentro del bloque. BIP30 conserva su ruta específica. Una prueba independiente compara 20 bloques y cientos de gastos ramificados/intra-bloque contra el Index de referencia y verifica rewind/auditoría.

La ejecución exploratoria del overlay con generación 256 (no el default final) mantuvo caché 64 MiB: 165 muestras, ~599 s, Core 5002 → 222 005, Lite 255 → 59 135; retraso final 162 870. RSS Lite máximo muestreado 133 276 KiB; más de 5 GB libres al final. **NOT_DEMONSTRATED**. No se presenta esa diferencia de alturas entre ensayos como mejora causal: los bloques y pares difieren. El default final de 64 bloques requiere nueva medición con el overlay.

## Reproducir y seguir perfilando

```sh
ELECTRUM_LITE_PROFILE_STORE=1 BITCOIND=/ruta/a/bitcoind \
ELECTRUM_LITE=/ruta/al/binario \
python3 scripts/cosync_smoke.py --network signet \
  --root /directorio/nuevo/en/disco --seconds 600 \
  --max-download-mib 2048 --rpc-probes
```

Se conservan JSONL, metadatos/hash y logs. Las métricas `anchor_ms` y `rpc_*_worker_us` separan comprobación de rama y trabajo agregado de grupos RPC; `rpc_decode_worker_us` mide hex/consensus decode, no la validación Merkle/witness posterior. `STORE_PROFILE` separa preparación SQL, append, fsync de archivo y commit SQL; `SQL_SCAN` informa pasos Fullscan de las sentencias cacheadas, no un perfil completo de SQLite. Solo registra contadores y tiempos, no cookies ni consultas de cartera. La lectura `/proc` registra RSS de los dos procesos propios, no presupuesto total ni pico garantizado.

**Conclusión:** el perfil identificó y redujo una sobrecarga, pero el objetivo de retraso decreciente sigue sin cumplirse. La siguiente mejora necesita abordar rendimiento durable de escritura y/o transporte sin sacrificar seguridad. La poda coordinada no constituye evidencia de mayor velocidad.
