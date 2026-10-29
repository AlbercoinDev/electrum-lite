# Co-sincronización pública Signet — resultado negativo

Binario publicado: `f8fcfb6`, descargado mediante el instalador público.
SHA-256 del ejecutable: `1870c0ba47306f76b6933ce45db79bcc840ca8454134813f0492a107a377b78a`.
Core: Bitcoin Core daemon version v31.0.0. Linux x86-64; datos de esta ejecución en tmpfs. Nodo nuevo, 8 conexiones máximo, `dbcache=64`, `prune=1`, sin wallets. No se ejecutaron otras pruebas de carga simultáneas; no equivale a hardware dedicado libre de procesos del escritorio.

Presupuesto: hasta 600 segundos / 512 MiB recibidos (más datos en vuelo), prefill de 4096 bloques antes de arrancar Lite. Core siguió descargando sin pausa. No hubo poda ni ventanas de mantenimiento. Se detuvieron ambos procesos propios después del muestreo.

| Muestra | Tiempo desde inicio | Core antes/después de health | Lite | Retraso mínimo/máximo |
|---|---:|---:|---:|---:|
| Primera de co-sync | 56,612 s | 5844 / 5848 | 255 | 5589 / 5593 |
| Última | 255,261 s | 100087 / 100107 | 29439 | 70648 / 70668 |

67 muestras válidas; ventana de aproximadamente 198,65 segundos. Core avanzó **al menos 94 239 bloques**, Lite **29 184**. El retraso creció inequívocamente. Último ciclo: 256 bloques, espera 1280 ms, commit 270 ms, total 2174 ms. Primera muestra: espera 1382 ms, commit 120 ms, total 2016 ms. Estos tiempos son del último ciclo, no promedios ni perfiles completos.

Resultado observado:

```json
{
  "result": "NOT_DEMONSTRATED",
  "qualifying_window": null,
  "valid_samples": 67,
  "full_sync": false,
  "maintenance_seconds": 0,
  "p2p_pauses_during_measurement": 0,
  "measurement_started": true,
  "elapsed_seconds": 258.4607122710004
}
```

La ejecución terminó por el presupuesto de descarga; el tiempo incluye arranque/cierre, no se usa como denominador de velocidad. **No se ha cumplido la condición de rendimiento.** Un replay sintético favorable no sustituye este resultado.

Un ensayo exploratorio posterior con grupos RPC de 16 se invalidó por presión de espacio en tmpfs (Lite activó su umbral de seguridad y Core terminó). No se usa como evidencia comparativa y ese cambio no se publica. Los datos y logs de ambos ensayos se conservaron fuera de tmpfs, sin borrar ni podar bloques.

## Reproducir y clasificar sin contar pausas

```sh
BITCOIND=/ruta/a/bitcoind ELECTRUM_LITE=/ruta/al/binario/publicado \
python3 scripts/cosync_smoke.py --network signet \
  --root /ruta/nueva/en/disco/con/espacio --seconds 600 --max-download-mib 512
```

El directorio debe ser nuevo y se conserva. Se puede elegir Testnet4. La herramienta no activa poda ni cambia nodos existentes; limita tiempo/red, comprueba espacio y registra metadatos/hash, logs y JSONL. Un resultado positivo exige al menos 120 segundos de intervalos con avance de Core, 1000 bloques de avance, cobertura activa del 80% de la ventana y reducción conservadora del retraso tanto global como durante intervalos activos. Los períodos con Core detenido, fallos o huecos largos no acreditan velocidad. Los tests offline rechazan explícitamente esos falsos positivos. Aun un éxito sería acotado a ese tramo, no sincronización completa ni garantía futura.
