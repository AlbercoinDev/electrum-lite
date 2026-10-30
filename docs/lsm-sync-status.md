# Sincronizador LSM, 2026-10-30

[CONFIRMADO] `src/experimental_lsm/sync.rs` conecta RPC autenticado a las
generaciones y consultas del nuevo motor LSM. Feature `experimental-lsm`.
El ensayo nuevo NO usa SQLite ni continúa el banco comparativo aparcado.

## Contrato implementado

- Verifica red y génesis mediante Core; el adaptador reutiliza RPC cookie-auth,
  endpoints numéricos loopback y errores sanitizados de `rpc::Core`.
- Obtiene punta observada y comprueba su hash como ancla antes de cada commit.
  Verifica además hash del bloque recibido y pertenencia activa antes de publicar.
- Compara cabeceras locales con Core y busca ancestro dentro del undo retenido.
  Errores RPC se PROPAGAN: no equivalen a fork ni autorizan rewind.
- Reorg conserva el orden durable del coordinador: repetición ancestral en el
  archivo/fsync, undo y cursor en transacción LSM. Profundidad >288, undo
  ausente o lote que exceda el presupuesto fallan sin bypass hacia DB-only rewind.
- Core ya podado no permite una importación vacía ni reemplazos cuyo primer
  bloque requerido falte: pide histórico confiable/recuperación a otra raíz.
- Ciclos configurables hasta 64 bloques/16 MiB de bytes originales, una descarga
  de bloque a la vez. Cada bloque sigue teniendo archivo fsync y commit WAL sync.
  No se omite undo ni se relaja durabilidad durante la carga inicial.
- Parada cooperativa y presupuesto temporal comprobados entre operaciones;
  **una RPC en curso conserva su timeout de socket**, no se promete cancelación
  instantánea. Umbral de espacio comprobado antes de rewind y de importar.
- Al terminar vuelve a observar Core y comprueba el ancla. El resultado distingue
  `confirmed_caught_up`; eso NO es readiness de cartera y NO autoriza poda.
  El servidor deberá exigir además una mempool completa y de la misma rama.

La ejecución es bloqueante y se destina a un worker acotado dedicado, no a una
tarea Tokio. No arranca/detiene nodos, no poda y no toca nodos ajenos.

## Pruebas ejecutadas

```sh
cargo fmt --check
cargo clippy --release --locked --all-targets --features experimental-lsm -- -D warnings
cargo test --release --locked --features experimental-lsm --lib --tests
cargo build --release --locked --features experimental-lsm --example lsm_sync_check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

[CONFIRMADO] Todos pasaron: 75 tests con feature, 52 normales. Cuatro tests nuevos
del sincronizador cubren ciclos acotados, fork y cancelación; fallo RPC sin
mutación; identidad/poda inválida; ancla cambiante; falta de espacio; reorg
profundo rechazado sin append. Son tests deterministas, no cifras de velocidad.

Ensayo real con **Core 31.0**, P2P explícitamente deshabilitado y un regtest nuevo:

```sh
python3 scripts/lsm_sync_smoke.py \
  --bitcoind /tmp/opencode/bitcoin-31.0/bin/bitcoind \
  --bitcoin-cli /tmp/opencode/bitcoin-31.0/bin/bitcoin-cli \
  --example target/release/examples/lsm_sync_check
```

```text
PASS LSM authenticated RPC cycle: height=102, imported=103, rewound=0; local headers/TX/witness/proofs match Core
PASS wrong RPC cookie rejected without archive mutation or credential leak
PASS LSM authenticated RPC cycle: height=104, imported=3, rewound=1; local headers/TX/witness/proofs match Core
PASS LSM authenticated RPC cycle: height=104, imported=0, rewound=0; local headers/TX/witness/proofs match Core
PASS LSM-only sync, real Core reorg and restart; owned Core stopped and all test data removed
```

El worker usa ciclos de máximo ocho bloques, verifica todos los bloques locales
contra Core y recupera cada transacción/prueba desde LSM/archivo, incluyendo un
gasto witness firmado. Cookie errónea produjo fallo sin cambiar los bytes del
segmento ni filtrar el secreto de prueba. La rama reemplazada se creó mediante
invalidateblock y tres bloques nuevos. La segunda reapertura importó cero.

Un primer intento de fixture intentó minar otra vez al mismo payout tras
invalidar y Core rechazó generar el reemplazo. Se corrigió usando payout nuevo;
el intento fallido también detuvo Core y eliminó su raíz. Solo la ejecución
completa posterior anterior se toma como evidencia de reorg.

TemporaryDirectory contiene nodos, wallet, cookies, archivo y DB. El script
detiene su hijo en finally incluso ante fallos y elimina datos al salir. Builds
y herramientas temporales propios se eliminan también al cerrar este bloque.

## Pendiente para el servicio nuevo

[PENDIENTE] Integrar readiness y protocolo/transportes del servidor,
integración del supervisor/poda con este motor, capacidad y ensayo sostenido
Signet/Testnet4. La comprobación de ancla por bloque y la auditoría semántica
completa de startup son conservadoras: su coste todavía no está optimizado.

No se ejecutó co-sync pública, no se afirma rendimiento superior a Core ni
compatibilidad del servicio LSM con Core 32.1 basándose solo en este regtest.
Mainnet sigue siendo objetivo de producto y su poda continúa bloqueada.
No se modifica el tag ni la pareja binaria pública: el lanzador Signet publicado
todavía no selecciona LSM. No se empaqueta el árbol con cambios pendientes de
catch-up y no se cambia la versión `0.0.1-beta.1`.

Continuación: [mempool separada LSM, readiness por generación y pruebas Core](lsm-mempool-status.md).
