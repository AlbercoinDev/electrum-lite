# LSM: validación local mínima de capacidad

[CONFIRMADO] Caso acotado offline, solicitado sin iniciar la prueba pública.
Binario publicado `86379be`, SHA256 de paquete
`0e17ba331be0cc674bc540ae5cdbc0855b04fbc6b8f459c2286ab91977ee5ade`,
descargado y comprobado antes de ejecutar. Core 31.0 de pruebas preexistente.

```sh
python3 scripts/lsm_electrum_smoke.py \
  --bitcoind /ruta/bitcoin-31.0/bin/bitcoind \
  --bitcoin-cli /ruta/bitcoin-31.0/bin/bitcoin-cli \
  --electrum-lite /ruta/al/binario/publicado/electrum-lite \
  --capacity-outputs 256
```

El modo nuevo ejecuta SOLO este caso, no la suite TCP extensa. Crea un regtest
P2P offline, mina 101 bloques para madurez y un bloque con una transacción
witness firmada con 256 salidas a scripts distintos. LSM importa hasta altura 102.
Se consultan por TCP las 256 monedas e historiales, posición correcta y bytes
exactos de la transacción. Límite del generador: hasta 1024 salidas; esta ejecución
solo valida 256. Cierre y eliminación en finally/TemporaryDirectory.

## Resultado observado

| Métrica | Valor |
|---|---:|
| Transacción serializada | 8098 bytes |
| Arranque + importación observada | 4,183 s |
| Tiempo hasta acabar consultas | 16,975 s |
| RSS Lite, muestra tras consultas | 19 324 928 bytes |
| RSS Core, muestra tras consultas | 64 081 920 bytes |
| Disco Core, lógico / asignado | 18 083 675 / 18 132 992 bytes |
| Disco Lite, lógico / asignado | 724 013 / 741 376 bytes |

Entorno: Linux 6.12.113+deb13-amd64, glibc 2.41, AMD Ryzen 9 7940HS;
MemTotal reportado 8 099 768 kB. No se fijó frecuencia ni se aisló la máquina.

```text
PASS bounded local LSM capacity: distinct UTXO/history and exact witness transaction
PASS capacity test node/data and downloaded executable removed
```

Los tiempos incluyen sondeo y RPC, no son un perfil del escritor. RSS es una
muestra, NO pico ni límite garantizado. El disco se mide con los procesos vivos,
no como snapshot atómico; no incluye coste de backups de poda en este caso.
No se ejecutaron más suites extensas. Dos tests mínimos del helper de disco
también pasaron. Ningún nodo personal fue usado ni modificado.

## No demostrado

[PENDIENTE] Bloques cercanos a límites de consenso con muchos inputs/outputs,
presión de lote KV/undo, límites de memoria, crecimiento de larga duración y
picos de disco durante mantenimiento. Una transacción de 8 KiB NO valida capacidad
de bloques de 4 MWU ni toda mainnet. Tampoco prueba rendimiento en hardware antiguo.

La co-sincronización pública sigue reservada al usuario. Se necesita su JSONL del
[observador](lsm-install-status.md#tu-prueba-pública-observación-sin-controlar-los-nodos)
para analizar retraso mientras Core avanza, junto con duración y mantenimiento.
Sin esos datos no se marca la validación sostenida como completada.

## Segundo caso: bloque de 3,73 MWU, 1024 salidas

[CONFIRMADO] Se ejecutó un único caso adicional local con el mismo paquete
publicado, hash comprobado y Core 31.0 aislado, sin cadena pública:

```sh
python3 scripts/lsm_electrum_smoke.py \
  --bitcoind /ruta/bitcoin-31.0/bin/bitcoind \
  --bitcoin-cli /ruta/bitcoin-31.0/bin/bitcoin-cli \
  --electrum-lite /ruta/al/binario/publicado/electrum-lite \
  --capacity-outputs 1024 --capacity-payload-bytes 900000
```

| Métrica | Valor |
|---|---:|
| Peso real del bloque, validado por Core | 3 728 261 WU (93,2 % de 4 MWU) |
| Transacción witness serializada | 931 925 bytes |
| Salidas a scripts distintos consultadas | 1024 |
| Payload OP_RETURN adicional | 900 000 bytes |
| Arranque + importación observada | 4,287 s |
| Tiempo hasta acabar consultas | 55,406 s |
| RSS Lite, muestra tras consultas | 30 056 448 bytes |
| RSS Core, muestra tras consultas | 102 567 936 bytes |
| Disco Core, lógico / asignado | 19 238 748 / 19 288 064 bytes |
| Disco Lite, lógico / asignado | 1 806 685 / 1 826 816 bytes |

```text
PASS bounded local LSM capacity: distinct UTXO/history and exact witness transaction
PASS high-load local test data and downloaded executable removed
```

Verificadas las 1024 posiciones de UTXO, historial por script y transacción
witness byte-exact por TCP. El fixture usa stdin de bitcoin-cli para evitar
límites de tamaño de argumentos del sistema. Core validó el bloque generado.
Mismo entorno de la primera ejecución; no se fijaron frecuencia/carga.

Esto añade evidencia de bytes/peso cercanos al límite, **no del peor coste de
indexación**: el payload es cero deliberadamente compresible y OP_RETURN no crea
UTXO persistente. Faltan bloques densos en transacciones/inputs, witness grande,
presión máxima de undo/lotes, largos historiales y crecimiento sostenido. No se
deducen ahorro de disco, picos de memoria, throughput público ni capacidad general.
No se ejecutó la suite extensa ni se arrancó Signet. La prueba pública continúa
reservada al usuario; sin su JSONL no puede validarse convergencia sostenida.

## Tercer caso: transacciones e inputs, sin payload artificial

[CONFIRMADO] Un único caso adicional sobre el mismo binario publicado y Core
31.0 offline, mismo entorno. Se prepararon 1024 salidas distintas en altura 102
y se gastaron TODAS en altura 103 mediante 128 transacciones witness firmadas,
cada una con 8 inputs, sin OP_RETURN:

```sh
python3 scripts/lsm_electrum_smoke.py \
  --bitcoind /ruta/bitcoin-31.0/bin/bitcoind \
  --bitcoin-cli /ruta/bitcoin-31.0/bin/bitcoin-cli \
  --electrum-lite /ruta/al/binario/publicado/electrum-lite \
  --capacity-outputs 1024 --capacity-fanin 8
```

| Métrica | Valor |
|---|---:|
| Transacciones de gasto / inputs totales | 128 / 1024 |
| Bytes serializados de los gastos, con witness | 157 051 |
| Peso real del bloque de gastos | 299 635 WU |
| Arranque + importación observada | 4,279 s |
| Tiempo hasta acabar consultas | 90,880 s |
| RSS Lite / Core, muestras tras consultas | 27 992 064 / 70 340 608 bytes |
| Disco Core, lógico / asignado | 18 675 216 / 18 726 912 bytes |
| Disco Lite, lógico / asignado | 4 299 585 / 4 317 184 bytes |

Comprobado por TCP: las 1024 monedas originales desaparecen, sus saldos quedan
a cero y cada historial contiene creación y gasto. Las 128 monedas nuevas tienen
posición/valor/saldo correctos. Los bytes witness de todas las transacciones
coinciden exactamente con los firmados por Core. Core valida el bloque generado.

```text
PASS bounded local LSM capacity: distinct UTXO/history and exact witness transaction
PASS dense local test data and downloaded executable removed
```

RSS sigue siendo muestra, no pico. La medición temporal empieza tras generar
los bloques; no incluye preparación/firma de fixtures. Incluye inicialización
de índice nuevo, importación y sondeo. No hubo mantenimiento/pruning en este caso.
La prueba agrega evidencia de trabajo sobre prevouts e índices frente al payload
compresible anterior, pero este bloque pesa solo el 7,5 % del máximo: NO demuestra
la peor densidad posible ni combina máximo peso y máximo coste de indexación.
Faltan límites de muchos scripts/historial largo, undo y carga sostenida.

No se lanzan más pruebas públicas automáticamente: el siguiente dato necesario
para convergencia sostenida es el JSONL del usuario, todavía no recibido.
