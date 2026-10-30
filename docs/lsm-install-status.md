# Instalación LSM experimental — 2026-10-30

[CONFIRMADO] Paquete Linux x86-64 publicado desde worktree limpio del commit
`86379be`, con `--features experimental-lsm`, versión `0.0.1-beta.1` sin cambios.
No se construyó desde el tag original, que permanece intacto.

Paquete: `electrum-lite-0.0.1-beta.1-linux-x86_64-86379be.tar.gz`.
SHA256: `0e17ba331be0cc674bc540ae5cdbc0855b04fbc6b8f459c2286ab91977ee5ade`.
Pareja permitida: ese hash exacto + Core estable oficial **32.1** autenticado.
Otra última estable no validada se rechaza, sin fallback silencioso.

## Instalar o actualizar, sin sudo

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/AlbercoinDev/electrum-lite/main/install.sh | sh
```

Requiere Linux x86-64, Python 3.11+, curl y GnuPG. Cierra Lite antes de actualizar.
Instala ambos ejecutables bajo `~/electrum-lite/`. Datos LSM nuevos y Core dedicado
en `~/electrum-lite/data/signet/lsm-v3/{lite,core}/`; backups en `backups/signet/`.
Raíces de instalaciones anteriores se conservan, sin migración/adopción automática.
El nuevo Core descarga de nuevo su propia cadena. No usa `~/.bitcoin`.

## Ejecutar Signet

```sh
"$HOME/electrum-lite/electrum-lite" --network signet
```

Mantén esa terminal abierta. Lite configura/inicia Core dedicado y coordina su
poda; **Ctrl+C** detiene ambos. No inicies otro escritor sobre ese Core.
Electrum privado: `127.0.0.1:60601`, TCP sin TLS nativo. No expongas el puerto.
Las consultas de cartera quedan bloqueadas hasta alcanzar Core observado y
completar mempool; eso no certifica que Core haya terminado sincronización pública.

Diagnóstico local desde otra terminal:

```sh
python3 - <<'PY'
import json, socket
with socket.create_connection(('127.0.0.1',60601),timeout=5) as s:
    s.sendall(b'{"id":1,"method":"server.health","params":[]}\n')
    print(json.loads(s.makefile('rb').readline(65536)))
PY
```

Debe indicar `backend: lsm` y `network: signet`. Errores de presupuesto, disco,
histórico ausente o auditoría inconclusa no deben eludirse borrando archivos.
Conserva backup y generación anterior; no mezcles índices/archivos entre raíces.

## Evidencia y límites

Prueba mínima `python3 scripts/lsm_bundle_smoke.py`, paquete descargado de GitHub:

```text
PASS published LSM pair: Signet offline, backend=lsm, owned Core and isolated LSM generation
PASS both children stopped; idempotent update preserved existing data
PASS disposable installation and all test data removed
```

También se repitió esa comprobación con la **línea curl publicada exacta**,
HOME temporal y bootstrap remoto fijado automáticamente al commit
`b9cad4eec1930577a573f1348baddc7e9722de90`, tanto instalación como actualización:

```text
PASS exact published one-line bootstrap, Signet offline start/stop and repeat update
```

Se confirmó backend LSM, Core hijo dedicado y ausencia de índices SQLite en la
raíz nueva. Los dos procesos se cerraron; HOME, datos y artefactos temporales de
compilación/empaquetado se eliminaron tras conservar esta evidencia.

Se corrigió durante esa prueba la inicialización de una raíz nueva: crear la
carpeta contenedora no equivale a tener un índice existente. El paquete fallido
`ecbf37e` fue retirado. La prueba usa `--offline`: no demuestra co-sync pública.
Pruebas previas de poda efectiva, backup, witness/Merkle y fallo seguro con
Core 31.0/32.1: [mantenimiento](lsm-maintenance-status.md).

[PENDIENTE] Capacidad, uso total de disco/RAM y co-sincronización sostenida que
reduzca retraso mientras Core avanza. A petición del usuario no se ejecutan más
suites extensas: la prueba pública queda en sus manos. No se afirma rendimiento,
producción, interoperabilidad GUI Sparrow ni seguridad de poda mainnet.
Mainnet gestionada continúa bloqueada. Esta entrega sigue siendo experimental.

## Tu prueba pública: observación sin controlar los nodos

Con Signet ejecutándose, desde otra terminal puedes registrar 30 minutos:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/AlbercoinDev/electrum-lite/main/scripts/lsm_observe.py | python3 - --seconds 1800 --interval 30 --output "$HOME/electrum-lite/logs/signet-observation-$(date +%s).jsonl"
```

El observador no inicia, pausa, detiene ni poda nodos. Lee RPC cookie local,
health, tamaños de disco y RSS de ejecutables de esta instalación. No registra
credenciales, scripts ni transacciones. Rechaza sobrescribir el archivo de salida.
No lo ejecutes mientras otra red use la misma instalación: el RSS agregado incluye
todos los procesos de los binarios del bundle, no solo Signet. RSS sumado puede
contar páginas compartidas dos veces; no es PSS ni memoria total del sistema.
Los recorridos de disco añaden E/S y no son snapshots atómicos; pueden omitir picos
entre muestras. Errores se registran explícitamente, nunca como ceros.

Comparte el JSONL al terminar. Para evaluar co-sync hacen falta intervalos donde
**Core avance y el retraso disminuya**, sin pausas de Core ni ventanas de poda
contadas como mejora del indexador. `core_ibd` y cabeceras ayudan a distinguir
alcanzar el Core local de terminar la cadena pública. Un prefijo corto, un solo
intervalo favorable o `ready=true` no prueban convergencia sostenida.

Registra también hardware, duración, interrupciones/mantenimiento y cualquier
error visible. `bundle_disk` incluye binarios, temporales y backups; compara con
Core+Lite por separado. La prueba no establece por sí sola capacidad de bloques
grandes, límites RSS, consumo máximo ni seguridad ante fallos físicos.

[CONFIRMADO] Solo se probaron localmente los helpers del observador (2 tests),
sin arrancar nodos ni descargar cadena pública. La evidencia de co-sync real
sigue pendiente de la ejecución del usuario.
