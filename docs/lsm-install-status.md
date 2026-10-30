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
