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
