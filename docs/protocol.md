# Perfil Electrum privado — beta 0.0.1

Negociación y anuncio: **1.4–1.4**. Se acepta un rango que incluya 1.4; no se anuncian 1.4.1/1.4.2/1.6/1.7. El cliente JSON-RPC de integración no es Sparrow: su GUI y las redes públicas completas siguen pendientes.

## Métodos

| Grupo | Métodos |
|---|---|
| Servidor | server.version, features, ping, banner, donation_address, peers.subscribe, add_peer |
| Cabeceras | blockchain.headers.subscribe, blockchain.block.header, blockchain.block.headers |
| Scripts | blockchain.scripthash.get_balance, get_history, get_mempool, listunspent, subscribe |
| Transacciones | blockchain.transaction.get, get_merkle, id_from_pos, broadcast |
| Fees | blockchain.estimatefee, blockchain.relayfee, mempool.get_fee_histogram |

Extensiones: `blockchain.scripthash.unsubscribe` y `server.health`. `server.add_peer` devuelve false, pares vacíos, sin anuncio público. Donación vacía. `server.features.pruning=null` se refiere al historial de scripts, no a la poda de Core.

## Semántica

- Mensajes por nueva línea sobre TCP loopback. JSON-RPC 2.0, requests legacy sin `jsonrpc`, lotes acotados y notificaciones sin respuesta. Parámetros posicionales y por nombre; desconocidos/tipos inválidos producen error.
- Negociación/features/ping/health funcionan durante sincronización. Las consultas de cadena/cartera requieren tip coincidente con Core observado, IBD terminado y mempool reconciliada completa dentro de los presupuestos.
- Scripthash externo: SHA-256(scriptPubKey), bytes invertidos. SQLite usa hash canónico binario; las respuestas status son SHA-256 hexadecimal del historial `txid:height:`.
- Historial confirmado por altura y posición. Génesis coinbase no se anuncia como historial Electrum, aunque se conserva en archivo/CLI. Mempool ordenada por `(-height, txid)`; height=0 si todas las entradas están confirmadas y -1 si hay padres pendientes. Se incluye fee en satoshis.
- Saldo confirmado procede del UTXO confirmado. `unconfirmed` es delta neto; listunspent elimina salidas gastadas en mempool y añade pendientes no gastadas. No confundir saldo con importe gastable ajustado a madurez de coinbase.
- RBF, expulsiones y confirmaciones se reconstruyen desde un snapshot RPC estable, con comprobación del conjunto de TXID y tip antes de publicar. ZMQ no es necesario.
- La suscripción inicial devuelve status; se notifican cambios de scripts y cabeceras. También se notifican scripts al cambiar la cabecera, incluso si su status coincide, para no perder reorgs de igual altura.
- `transaction.get` recupera bytes locales con witness; TXID repetidos usan la última ocurrencia activa. Verbose usa decoderawtransaction con esos bytes y metadatos locales; no exige un bloque histórico disponible en Core.
- Merkle de TX y pruebas checkpoint de cabeceras se calculan bajo demanda. `block.headers` devuelve `hex`, `count`, `max` (formato 1.4), no el array `headers` de versiones posteriores.
- Estimación de fees depende de Core, devuelve -1 si no hay estimación. Histograma mempool usa buckets aproximados de potencias de dos con vbytes por intervalo.

## Transporte y presupuestos

TCP se limita a loopback. Para remoto se necesita SSH/túnel privado o proxy TLS con certificado verificado. TLS nativo, websocket, padding anti-análisis de tráfico y resistencia de servidor público no se implementan ni anuncian.

Por defecto 16 conexiones/1 000 scripts por conexión (configurables), cola worker 64, batch 64, hasta 200 operaciones por 10 s/conexión, 8 100 000 bytes por mensaje y 16 200 000 por respuesta. Lectura parcial conserva bytes entre ticks de suscripción y rechaza EOF truncado. Idle 120 s, escritura 15 s, espera worker 90 s. Límites excedidos pueden producir error o cierre, nunca una respuesta parcial como si fuese completa.

Historial/listas de UTXO acotadas por `--max-query-results`; error 10001 cuando exceden el presupuesto de script. Chunks de 2 016 cabeceras, checkpoint máximo 2 000 000 (hasta unos 64 MB de hashes temporales). Son límites de recursos, no benchmarks de latencia.

Mempool configurable por transacciones, bytes serializados y entradas input/output. Se cachean bytes entre ciclos y se descargan hasta 500 transacciones nuevas por ciclo; el primer snapshot puede tardar. Nunca se expone un snapshot parcial. RPC general limita respuesta a unos 8 MB; listados mempool tienen un presupuesto proporcional a su límite, hasta 64 MiB. Valores demasiado grandes dejan el servicio no listo; revisa health y los flags antes de usar una cartera.

## Validación

`scripts/electrum_smoke.py`, llamado por `core_smoke.py`, prueba negociación, named params, cabeceras/checkpoints, TX/verbose/Merkle/posición, saldo contra scantxoutset, génesis, mempool padre/hijo, RBF, confirmación, reorg, suscripciones, unsubscribe, batch, malformed JSON y errores.

`prune_smoke.py` elimina realmente datos de Core y verifica TX/Merkle históricos por el socket Electrum. Los tests Rust cubren Merkle impar contra rust-bitcoin, frames fragmentados/cancelados y sobredimensionados, requests legacy/notificaciones y fallo cerrado.

Para aceptación con Sparrow: instala una versión con la red deseada, configura **Private Electrum server** en loopback con puerto de la app y espera health.ready. Prueba una cartera de laboratorio sin fondos reales: recuperación, recibir/enviar, reemplazo, confirmar, reiniciar y consultar después de poda. Registra versión de Sparrow y divergencias. No se ha ejecutado este ensayo aquí (no hay Java/Sparrow/entorno gráfico disponibles).

Referencias: [Electrum protocol basics](https://electrum-protocol.readthedocs.io/en/latest/protocol-basics.html), [métodos y cambios por versión](https://electrum-protocol.readthedocs.io/en/latest/protocol-methods.html), [Sparrow: tipos de servidor](https://sparrowwallet.com/docs/quick-start.html). Se revisaron detalles 1.4; no aplicar sin revisión cambios de `latest`.
