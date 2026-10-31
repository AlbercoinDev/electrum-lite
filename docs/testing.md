# Herramientas y pruebas

Guía de ejecución local. Las pruebas de integración crean nodos regtest
desechables; nunca se deben apuntar a un nodo existente ni a un directorio con
datos que se quieran conservar. Los smokes que podan o inyectan fallos deben
ejecutarse únicamente en sus entornos temporales aislados.

Tres scripts (`disk_guard_smoke.py`, `prefetch_smoke.py` y
`prepared_prune_failure_smoke.py`) usan `/tmp/opencode` como raíz temporal y
requieren que exista; los demás usan esa ruta si está disponible y, si no, el
temporal predeterminado del sistema.

## Herramientas

- Rust del proyecto (`rust-toolchain.toml`), Cargo, rustfmt y Clippy para
  compilar y ejecutar las pruebas Rust.
- Python 3 para los scripts de `scripts/`.
- Bitcoin Core (`bitcoind` y `bitcoin-cli`) para regtest. Los scripts aceptan
  rutas explícitas cuando el Core no está en `PATH`.
- Tor es opcional; los smokes Tor aceptan `--tor-bin`.
- Electrum GUI y Sparrow no son requisitos para los smokes automatizados. La
  interoperabilidad de GUI sigue pendiente de validación manual.

## Comprobaciones locales

Desde la raíz del repositorio:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
python3 - <<'PY'
import ast, pathlib
for path in pathlib.Path("scripts").glob("*.py"):
    ast.parse(path.read_text(), filename=str(path))
print("Python syntax OK")
PY
sh -n install.sh scripts/install-bitcoin-core.sh scripts/install-release.sh
```

La comprobación Python anterior solo valida sintaxis: no sustituye los smokes.
`pytest` no es el runner de este proyecto; las pruebas de integración son
scripts ejecutables directamente.

### Toolchain aislado de esta revisión

En el entorno revisado, Rust sí está instalado, pero fuera de `PATH`, en
`/tmp/opencode/electrum-rustup`; Cargo usa un caché separado y RocksDB necesita
libclang. Esta configuración es específica de esta máquina y no se debe copiar
a otros equipos. Para reproducir aquí las comprobaciones:

```sh
export RUSTUP_HOME=/tmp/opencode/electrum-rustup
export CARGO_HOME=/tmp/opencode/electrum-lite-cargo
export CARGO_TARGET_DIR="$PWD/target"
export PATH="/tmp/opencode/electrum-rustup/toolchains/1.99.1-x86_64-unknown-linux-gnu/bin:/tmp/opencode/electrum-cargo/bin:$PATH"
export LIBCLANG_PATH=/tmp/opencode/clang/usr/lib/x86_64-linux-gnu
export LD_LIBRARY_PATH="/tmp/opencode/clang/usr/lib/x86_64-linux-gnu${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export BINDGEN_EXTRA_CLANG_ARGS="-resource-dir=/tmp/opencode/clang/usr/lib/llvm-19/lib/clang/19 -isystem /usr/lib/gcc/x86_64-linux-gnu/14/include"
```

El `CARGO_TARGET_DIR` externo que ya existía en `/tmp/opencode` pertenece a
root y no es escribible por el usuario de esta revisión; por eso los builds se
hicieron en el `target/` local. En un entorno normal, instala Rust con rustup y
Clang/libclang mediante el gestor de paquetes de la distribución.

## Smokes y alcance

| Script | Qué comprueba | Requisitos / alcance |
|---|---|---|
| `smoke_regtest.py` | Sincronización inicial, protocolo, mempool, reorg, backup y recuperación P2P simulando un bloque ausente | Core regtest local, binario release y espacio temporal; no hace poda física ni usa peers públicos. |
| `managed_prune_smoke.py` | IBD P2P aislada, backpressure, poda física, recuperación witness y retención | Dos nodos regtest desechables y al menos 3 GiB libres en el temporal indicado. Prueba destructiva solo sobre su Core propio. |
| `prepared_prune_failure_smoke.py` | Rechazo fail-closed de pruebas/backup desactualizados y fallo RPC tras la intención durable | Core y Lite regtest efímeros; no debe recibir rutas de datos externas. |
| `disk_guard_smoke.py` | Pausa/reanudación por reserva de espacio y checkpoint/auditoría/backup | Core gestionado desechable; requiere al menos 1 GiB libre y crea sus propios directorios temporales. No llena el disco real. |
| `prefetch_smoke.py` | Comparación baseline/candidate e inyección de error RPC y rama equivocada | Core regtest aislado; se deben proporcionar dos binarios identificados (`--baseline`, `--candidate`). Las cifras son de una carga sintética. |
| `tor_smoke.py` | Inicio/control de Tor y transporte P2P sin fallback directo | Core local y Tor explícitos. `--public-bootstrap` permite bootstrap público; no es una prueba offline. |
| `tor_public_smoke.py` | Bootstrap público de Tor y recuperación P2P de la transacción de génesis mainnet | Acceso a Internet y autorización explícita `--allow-public`; no demuestra privacidad ni sincronización completa. |
| `devnet.py` | Preparación de una red regtest manual con raíz nueva | El destino debe ser nuevo; el script no limpia el entorno al terminar. |

Ejemplo de smoke offline básico:

```sh
python3 scripts/smoke_regtest.py \
  --bitcoind /ruta/bitcoin/bin/bitcoind \
  --bitcoin-cli /ruta/bitcoin/bin/bitcoin-cli \
  --electrum-lite target/release/electrum-lite
```

Smokes más costosos o con efectos irreversibles en su Core desechable:

```sh
python3 scripts/managed_prune_smoke.py \
  --bitcoind /ruta/bitcoin/bin/bitcoind \
  --electrum-lite target/release/electrum-lite
python3 scripts/prepared_prune_failure_smoke.py \
  --bitcoind /ruta/bitcoin/bin/bitcoind \
  --bitcoin-cli /ruta/bitcoin/bin/bitcoin-cli \
  --electrum-lite target/release/electrum-lite
python3 scripts/disk_guard_smoke.py \
  --bitcoind /ruta/bitcoin/bin/bitcoind \
  --bitcoin-cli /ruta/bitcoin/bin/bitcoin-cli \
  --electrum-lite target/release/electrum-lite
```

No se deben ejecutar `tor_public_smoke.py` ni el modo público de `tor_smoke.py`
como parte de una comprobación offline. No se recomienda correr todos los smokes
automáticamente en CI sin revisar primero recursos, duración y política de red.

Pruebas Tor, usando binarios ubicados explícitamente:

```sh
python3 scripts/tor_smoke.py \
  --bitcoind /ruta/bitcoin/bin/bitcoind \
  --electrum-lite target/release/electrum-lite \
  --tor-bin /ruta/tor
# Solo con autorización de red pública:
python3 scripts/tor_public_smoke.py \
  --bitcoind /ruta/bitcoin/bin/bitcoind \
  --tor-bin /ruta/tor --allow-public
```

Para comparar el rendimiento sintético y probar fallos de sincronización,
proporciona dos binarios conocidos y regtest aislado:

```sh
python3 scripts/prefetch_smoke.py \
  --bitcoind /ruta/bitcoin/bin/bitcoind \
  --bitcoin-cli /ruta/bitcoin/bin/bitcoin-cli \
  --baseline /ruta/binario-baseline \
  --candidate /ruta/binario-candidato
```

## Scripts auxiliares

- `query.py`: consulta un servidor Electrum Lite que ya esté ejecutándose.
- `package.py`: empaqueta el binario release existente y las fuentes; no compila
  ni firma. Compilar y comprobar el binario antes de generar el paquete.
- `install-bitcoin-core.py` y sus wrappers `.sh`: instaladores, no pruebas; revisar
  sus requisitos y confirmaciones antes de ejecutarlos.
