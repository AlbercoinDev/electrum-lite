# Mempool separada para LSM, 2026-10-30

Continuación: [servidor LSM integrado y pruebas TCP Core 31.0/32.1](lsm-server-status.md).
Los pendientes de servidor descritos abajo corresponden a esta entrega histórica.

[CONFIRMADO] `src/experimental_lsm/mempool.rs` añade reconciliación y vistas
de cartera no confirmadas sobre snapshots LSM. No escribe archivo, undo ni
índices confirmados. No está todavía conectado al servidor público.

## Consistencia y readiness

- RPC `getrawmempool(false, true)` obtiene TXIDs y `mempool_sequence`; se exige
  la misma punta Core que el snapshot confirmado y la misma lista/secuencia
  antes y después de obtener transacciones y antes de publicar resultados.
- La caché de calentamiento se liga a punta, secuencia, conjunto completo e
  instancia Core. Un token opaco SHA256 de la cookie estándar detecta rotación
  de instancia sin guardar credenciales en la caché mempool ni imprimirlas. Cookies vacías o >4096 B
  se rechazan. El modelo supone cookies automáticas que Core renueva al arrancar;
  no afirma detectar reinicios con credenciales estáticas reutilizadas a mano.
- Fuente oficial consultada para contrato/secuencia:
  https://github.com/bitcoin/bitcoin/blob/v31.0/src/rpc/mempool.cpp
  Se utiliza la biblioteca RPC genérica del proyecto; no código de otros indexadores.
- Calentamiento incremental, máximo 512 descargas por pasada (128 por defecto).
  Caché incompleta devuelve Warming y NUNCA una vista parcial. Una secuencia o
  instancia distinta reinicia la caché, evitando conservar bytes witness de
  otra instancia aunque coincida TXID.
- Límites separados para TXIDs, bytes originales, entradas input/output,
  descargas por pasada, tiempo y antigüedad de la validación. Los presupuestos
  son cotas lógicas de trabajo, no un límite RSS medido del proceso completo.
- Toda reconciliación invalida readiness anterior desde el inicio. Errores,
  límites o cancelación no dejan disponibles resultados antiguos. Una vista
  publicada solo sirve al mismo hash/punta y generación LSM; caduca por tiempo
  monotónico (10 s por defecto, configurable hasta 60 s).
- Esto es una observación consistente en un instante, no conocimiento continuo
  de Core. El futuro worker deberá invalidar antes de sincronizar y comprobar
  Core observado + generación + mempool completa antes de servir carteras.

## Construcción y consultas

El overlay se prepara aparte y se publica completo de una vez. Orden topológico
con padres únicos y dependientes acotados; no depende del orden RPC/TXID.
Prevouts salen de padres pendientes o del snapshot confirmado. Rechaza conflictos,
prevouts ausentes/no gastables, dependencias cíclicas, negativos y overflows.

Historial deduplicado por transacción/script, comisiones calculadas en satoshis y
orden estable `0` (sin padres pendientes) seguido de `-1` (con padres pendientes).
Los outputs pendientes ya gastados no se listan como monedas. La vista reúne
saldo confirmado, delta no confirmado, historial confirmado/pendiente y UTXO
efectivos; límites combinados dan error, nunca truncado. Recupera bytes originales
incluidos witness únicamente desde una caché COMPLETA de la generación correcta.

La mempool NO se incluye en backups de estado confirmado: tras restauración o
reorg hay que volver a reconciliar. Las consultas locales confirmadas y el archivo
histórico no dependen de conservar esta caché.

## Evidencia ejecutada

```sh
cargo fmt --check
cargo clippy --release --locked --all-targets --features experimental-lsm -- -D warnings
cargo test --release --locked --all-targets --features experimental-lsm
cargo build --release --locked --features experimental-lsm --example lsm_mempool_check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

[CONFIRMADO] Todos pasaron: **80 tests con feature, 53 sin feature**. Nuevos tests:

- Calentamiento de padre/hijo/nieto: no hay readiness hasta completar; estados
  0/-1, fees, saldos y monedas gastadas, sin cambiar generación confirmada.
- RBF y eliminación de descendientes; rechazo de consultas contra otra generación.
- Cambios de secuencia/instancia, raw witness distinto con mismo TXID, fallos de
  fuente, cancelación, presupuestos pequeños, conflictos/prevouts/fees inválidos.
- Caducidad de overlay, mempool vacía completa y rotación/tamaño de cookie.

Ensayo LSM-only con Core 31.0 real, wallet nueva y regtest P2P offline:

```sh
python3 scripts/lsm_mempool_smoke.py \
  --bitcoind /tmp/opencode/bitcoin-31.0/bin/bitcoind \
  --bitcoin-cli /tmp/opencode/bitcoin-31.0/bin/bitcoin-cli \
  --example target/release/examples/lsm_mempool_check
```

```text
PASS LSM complete bounded mempool: chain 0/-1, spent outputs, fees, balances and exact witness bytes
PASS LSM RBF and budget failure invalidate stale wallet results
PASS LSM confirmation, real Core reorg and reconfirmation require current generation plus complete mempool
PASS owned Core stopped; wallet, cookies, raw transactions, LSM data and all temporaries removed
```

Se minaron 101 bloques para gastar una coinbase madura; se firmó y envió una
cadena de tres transacciones witness. Un reemplazo de mayor fee desalojó los
descendientes. Se confirmó el reemplazo, se invalidó su bloque y volvió a
mempool con otra generación LSM; después se confirmó en una nueva rama.
Un límite de un byte bloqueó consultas hasta una reconciliación completa nueva.
El worker exige confirmación explícita de regtest aislado, red offline y nombre
de wallet de prueba antes de emitir RPC que modifiquen el nodo.

El script solo crea y detiene su Core dedicado en TemporaryDirectory; ejecuta
finally ante errores y elimina wallet/cookies/archivo/DB. Builds y herramientas
temporales del ensayo se eliminan al terminar. No se ejecutó banco comparativo
ni se descargó cadena pública. Tests normales son regresión, no trabajo nuevo
sobre el motor anterior. No se empaquetó el árbol con catch-up pendiente.

## Pendiente

[PENDIENTE] Servidor Electrum LSM y su worker/readiness, notificaciones, diagnóstico
y límites de transporte; poda LSM con supervisor/backup; capacidad real y
co-sincronización Signet/Testnet4, integración completa con Core 32.1.

La invalidación conservadora de caché cuando cambia secuencia puede reiniciar
calentamiento en una mempool pública muy activa. Su convergencia/latencia y el
consumo de memoria total no se han medido. No se afirma velocidad superior a Core
ni preparación de producción. El lanzador publicado todavía no selecciona LSM;
versión beta y tag original no cambian.
