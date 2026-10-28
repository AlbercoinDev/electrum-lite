---
document_id: ELE-SPEC-001
title: "Electrum Lite Engine"
language: es-ES
version: "1.3.0-draft"
date: "2026-10-28"
status: "BETA_IMPLEMENTACION_PARCIAL"
implementation_started: true
benchmarks_executed: false
---

# Electrum Lite Engine — Especificación técnica

> Diseño objetivo con implementación parcial en la beta `0.0.1-beta.1`: CLI, RPC, archivo Zstandard e índices en memoria. No hay servidor Electrum, base transaccional, undo, mempool ni controlador de poda todavía. No hay benchmarks propios ni garantías de compatibilidad completa o consumo. Se conserva el nombre del archivo para no romper referencias. La arquitectura simplificada sustituye el diseño original de LMDB + segmentos propios + archivo individual de transacciones.

## 1. Objetivo y prioridades

**[DECIDIDO]** Crear un indexador Bitcoin y servidor Electrum originales en Rust para conectar carteras como Sparrow a Bitcoin Core con poda manual. Core valida consenso; Electrum Lite conserva localmente el histórico necesario para consultar cualquier script, recuperar transacciones completas y generar pruebas Merkle después de la poda, sin servidores Electrum externos.

Prioridades: corrección y durabilidad, simplicidad de implementación y operación, recursos acotados y eficiencia medida. Buscar un buen compromiso, no el mínimo absoluto de disco a cualquier coste.

- Hardware objetivo: x86-64 antiguo sin AVX2 obligatorio, poca RAM y SSD SATA. Los perfiles de 2–4 GiB son hipótesis, no mínimos garantizados para Core + aplicación.
- Conservar witness y bytes originales. No eliminar datos para aparentar ahorro.
- **[DECIDIDO]** La aplicación debe soportar mainnet, Signet y Testnet4. Mainnet es requisito del producto, no una funcionalidad existente; las redes de prueba no sustituyen su implementación.
- **[DECIDIDO]** Publicar el proyecto en GitHub como `AlbercoinDev/electrum-lite`, con README de instalación y operación actualizado a cada entrega. No publicar binarios ni comandos de la aplicación como utilizables antes de verificarlos.
- No prometer ahorro frente a Core sin podar. Comparar también contra Core completo + servidor Electrum, que presta un servicio más equivalente.
- No objetivos iniciales: consenso propio, otras monedas, explorador web, REST pública, GUI ni servicio público sin controles.

## 2. Reglas de desarrollo

- Distinguir `[CONFIRMADO]` por fuentes verificadas, `[DECIDIDO]` por el proyecto, `[HIPÓTESIS]` por medir y `[PENDIENTE]` por resolver.
- No copiar código ni arquitectura de Electrs, Fulcrum o bindex. Pueden servir para interoperabilidad; reutilizar bibliotecas genéricas maduras respetando licencias.
- Antes de implementar, verificar versiones y APIs oficiales de Rust, Bitcoin Core, rust-bitcoin, Tokio, Zstandard y backend. Fijar toolchain y dependencias mediante `Cargo.lock`. Versiones exactas y licencia propia siguen pendientes.
- Los contratos descritos no son APIs validadas. No afirmar implementación, compatibilidad o rendimiento sin evidencia reproducible.
- Cambios persistentes, poda, borrado y restauración requieren backup, recuperación/migración, rollback y fallo seguro. Nunca desactivar durabilidad para ganar rendimiento en producción.
- Cada entrega debe indicar cambios, pruebas reproducibles, límites y riesgos; actualizar decisiones si cambia un contrato.

## 3. Arquitectura inicial

```text
Bitcoin Core [consenso, prune=1, txindex=0]
    │ RPC autenticado por cookie
    ▼
Obtención y comprobación de bloques
    ▼
Archivo inmutable de bloques completos comprimidos con Zstandard
    ▼
Indexador → una base de datos transaccional
    │        UTXO, historial, ubicaciones, cadena, undo, metadatos
    ▼
Consultas ← mempool transitoria reconciliada por RPC
    ▼
Servidor Electrum → Sparrow
```

- Un ejecutable y un proceso Rust. Empezar con un paquete Cargo con biblioteca, módulos pequeños y CLI; separar crates solo si aporta una ventaja real.
- Tokio para la futura red Electrum y coordinación; colas y cachés acotadas, un escritor lógico confirmado y lectores con snapshots consistentes. La CLI beta actual es síncrona y no necesita Tokio. No bloquear su futuro runtime con compresión o E/S síncrona pesada.
- Una única base transaccional para todos los índices y metadatos. **[PENDIENTE]** Elegir backend mediante una comparación pequeña de bibliotecas maduras; no imponer LMDB ni mantener varios backends en producción.
- No construir inicialmente segmentos históricos, compactación, motor KV ni codificación de transacciones propios. El backend gestiona sus estructuras internas.
- RPC autenticado inicialmente. `getblock(hash, 0)` devuelve serialización hexadecimal dentro de JSON, no un cuerpo binario puro. Decodificar antes de archivar. REST y ZMQ quedan fuera del alcance inicial.
- Core sigue siendo necesario para nuevos bloques, mempool, estimación de comisiones y difusión, pero no para recuperar histórico ya archivado.

## 4. Archivo e índices

- Conservar bloques completos, cabeceras y witness, en frames Zstandard independientes por bloque. Agrupar registros en archivos append-only de tamaño limitado; no crear un archivo por bloque.
- Versionar el formato e identificar red, hash de bloque, tamaños comprimido/descomprimido y checksum. Guardar segmento y ubicación en la base.
- Acotar tamaño y memoria al descomprimir. Como optimización posterior, permitir registros sin compresión si comprimir aumenta el tamaño, identificándolo con una versión de formato compatible; la beta inicial usa siempre Zstandard.
- TXID → ocurrencia/ubicación permite encontrar una transacción; inicialmente descomprimir su bloque para recuperarla. No guardar una segunda copia permanente de transacciones.
- Generar pruebas Merkle desde el bloque recuperado. Añadir tablas TXID ordenadas o cachés persistentes solo con justificación medida.
- El archivo debe permitir reconstruir índices desde génesis. Esto no implica reconstrucción rápida ni sustituye backups consistentes.
- Verificar hash, raíz Merkle y compromiso witness cuando aplique. TXID y raíz Merkle de transacciones no verifican por sí solos witness. Probar ida y vuelta byte a byte y TXID/wTXID.

Datos lógicos de la base: UTXO por outpoint y script; historial por scripthash; ubicación TX por bloque y posición; cadena activa y cabeceras; undo; manifiesto del archivo y estado de sincronización. Claves y serialización finales están pendientes. Usar binario, satoshis enteros y referencias compactas cuando sean útiles, sin duplicar cargas completas ni truncar hashes inicialmente.

Invariantes:

- TXID histórico no es globalmente único: representar ocurrencias por `(block_hash, position)` y probar BIP30.
- Una transacción aparece una sola vez por scripthash en el historial aunque existan múltiples entradas/salidas del mismo script.
- Procesar transacciones en orden para resolver gastos intra-bloque. Prevout ausente implica corrupción o datos incompletos y debe fallar.
- Tratar génesis y coinbase explícitamente. No insertar salidas demostrablemente no gastables en UTXO, pero conservar el historial correspondiente.
- Scripthash es SHA-256 de scriptPubKey; invertir bytes solo en el borde Electrum.
- Los índices UTXO deben coincidir. No mezclar generaciones ni mantener como confirmadas transacciones expulsadas de la cadena activa.

## 5. Persistencia y reorganizaciones

Archivo y base no comparten ACID global. Orden obligatorio:

1. Comprobar continuidad y pertenencia del bloque a la cadena validada por Core.
2. Escribir el registro completo y sincronizar bytes a disco y metadatos de directorio cuando proceda.
3. En una transacción durable de base, actualizar UTXO, historial, ubicaciones, cadena, undo, manifiesto y tip duradero.
4. Publicar la generación consistente para lectores.
5. Solo después evaluar poda.

Antes del commit puede quedar una cola no publicada: recuperar o truncar solo después de verificar referencias. Después del commit, todos los bytes referenciados deben existir y superar comprobaciones. Si no, modo seguro; nunca servir datos mezclados.

Registrar alturas y hashes de `core_tip`, `indexed_tip`, `durable_tip` y `served_tip`, además de altura de poda observada y límite de undo. Una altura sola no identifica una rama. No introducir inicialmente archivado adelantado independiente ni podar basándose solo en archivo persistido.

Reorg: localizar ancestro común, revertir descendiendo con undo, aplicar nueva rama ascendiendo, publicar y notificar. Conservar datos necesarios mientras haya lectores o rollback pendiente; no retener transacciones de lectura de base durante consultas de red largas.

**[PENDIENTE]** Ventana exacta de undo. Fuera de ella: modo seguro y reconstrucción controlada desde datos disponibles; no prometer reversión instantánea de profundidad arbitraria. Evitar inicialmente GC/reescritura del archivo histórico; medir crecimiento por bloques huérfanos.

## 6. Poda y disco

- Core con `prune=1`, sin poda automática, y `txindex=0`. Un solo controlador de poda; cambios externos pueden romper garantías y deben diagnosticarse. No modificar directamente archivos internos de Core.
- Denegar poda ante huecos, fallos de integridad, reorg o ausencia de generación durable verificada. Mantener margen bajo el tip duradero y respetar elegibilidad de Core; 288 bloques no es una garantía universal de recuperación.
- Core elimina archivos completos y conserva mínimos propios. Verificar retorno de `pruneblockchain`, `pruneheight` y disponibilidad real, no asumir eliminación exacta de la altura solicitada.
- Índice vacío frente a Core ya podado debe fallar claramente salvo restauración histórica confiable.
- No podar nodos con datos valiosos sin autorización y backups. Persistencia y recuperación deben pasar pruebas antes de habilitar poda.
- La poda manual no limita disco durante IBD: si Core adelanta al indexador, crece su almacenamiento. Alertar por espacio y lag; parar Electrum Lite no detiene Core.
- **[PENDIENTE]** Procedimiento autorizado para espacio crítico. No controlar automáticamente Core sin diseñarlo y probarlo explícitamente.

## 7. Electrum, mempool y seguridad

Baseline Electrum 1.4; fijar perfil exacto contra especificación y Sparrow. No anunciar versiones o métodos no probados.

Métodos previstos: `server.version`, `server.features`, `server.ping`, `server.peers.subscribe`; cabeceras y suscripción; saldo, historial, mempool, UTXO y suscripciones por scripthash; recuperación, difusión, posición y prueba Merkle de transacciones; estimatefee y relayfee. Verificar unsubscribe, verbose, errores, orden, alturas 0/-1, status y pruebas de cabeceras con checkpoint.

- JSON-RPC por nueva línea con límites de bytes, conexiones, concurrencia, tasa, complejidad JSON y timeouts. No devolver silenciosamente historiales incompletos ni agotar memoria con consultas grandes.
- Mempool transitoria separada del confirmado: snapshot y reconciliación periódica RPC; resolver carreras con cambios de tip, dependencias, RBF, expulsiones y confirmaciones. ZMQ no es requisito inicial.
- Localhost por defecto; acceso remoto con WireGuard o TLS explícito usando biblioteca madura como rustls. No exponer RPC/REST a Internet.
- Cookie RPC con permisos restringidos y sin secretos en repositorio. Sin telemetría ni anuncio público de pares por defecto. No registrar datos de cartera, consultas ni credenciales salvo debug explícito minimizado.
- Ejecutar sin root y con permisos mínimos. Documentar despliegue operacional después de validar la aplicación.

## 8. Pruebas y mediciones

**[DECIDIDO]** Signet y Testnet4 son las redes de integración y funcionamiento sostenido. Separar configuración, datos, cookies, puertos e identidad de red; rechazar mezclas. No ejecutar pruebas destructivas sobre nodos compartidos o valiosos. Mainnet queda fuera de las pruebas iniciales.

Regtest es apoyo local para fixtures deterministas, reorgs, fallos y poda controlada: esos escenarios no pueden provocarse reproduciblemente en redes compartidas.

Cobertura: génesis, coinbase, bloques vacíos, SegWit, Taproot, OP_RETURN, scripts no estándar, gastos intra-bloque, valores límite, deduplicación y fixtures BIP30; recuperación exacta y pruebas Merkle sin acceso histórico de Core; reorg dentro/fuera de ventana, mempool, reinicios y suscripciones; interrupciones antes/después de fsync y commit, checksums corruptos, cola incompleta, disco lleno y formato incompatible.

Probar backup/restore conjunto de base y archivo de una generación, rechazando snapshots mezclados. Para poda real, generar suficientes datos para los requisitos efectivos de Core, comprobar eliminación y consultar después. Conectar Sparrow no sustituye una suite independiente de protocolo.

### Experimento pequeño de almacenamiento

Comparar pocos backends transaccionales maduros con iguales datos, operaciones y durabilidad; elegir uno. Comparar archivo sin compresión, Zstandard y LZ4 por bloque. Zstandard es la opción inicial; cambiarla requiere evidencia y decisión documentada.

Medir Core + archivo + base, pico temporal y backup aparte; CPU, RSS/pico, E/S, ingestión y p50/p95/p99 de consultas aleatorias mientras se indexa, incluyendo descompresión y reconstrucción. Documentar hardware, versiones, FS y datos. No extrapolar ratios de compresión de otros corpus.

Signet/Testnet4 no predicen ahorro mainnet. Una estimación posterior necesitaría muestras offline representativas de distintas épocas de mainnet, sin desplegar allí ni habilitar poda.

Se retira la comparación obligatoria anterior LMDB/Fjall/segmentos propios. Formatos especiales, diccionarios e índices adicionales se estudiarán solo ante un cuello de botella medido.

## 9. Entregas y aceptación

| Fase | Entrega | Evidencia |
|---|---|---|
| A — Fundación | Paquete Rust, CLI, tipos, RPC por cookie, fixtures, CI | Build y tests reproducibles, red verificada |
| B — Archivo | Bloques completos comprimidos e índices en memoria | Ida y vuelta exacta, continuidad, Merkle y witness |
| C — Persistencia | Backend único, índices, undo y recuperación | Reinicios, fault injection, integridad y reorg |
| D — Poda | Controlador conservador y diagnóstico de disco | Histórico recuperable tras poda real controlada |
| E — Electrum | Consultas, mempool y suscripciones | Protocolo y Sparrow en Signet/Testnet4 |
| F — Operación | Backup/restore, límites, métricas, pruebas sostenidas | Informe reproducible y procedimientos de recuperación |

Estas fases sustituyen M0–M10. No saltar persistencia/recuperación para habilitar poda. Primera entrega funcional: CLI que archive bloques de Signet/Testnet4 y permita comprobar índices en memoria, sin poda. El soporte mainnet es obligatorio: selección explícita de red, comprobación contra Core, separación de datos y configuración y pruebas de sus parámetros. El despliegue y la poda real en mainnet se habilitarán solo después de validar persistencia, recuperación, interoperabilidad y capacidad de disco; no es necesario ejecutar pruebas destructivas en mainnet.

### Distribución e instalación

- Repositorio público: `https://github.com/AlbercoinDev/electrum-lite`.
- Mantener README con estado real del proyecto y pasos reproducibles. Mientras solo haya especificación, documentar cómo obtenerla y aclarar que no se puede instalar ni ejecutar un servidor todavía.
- Antes de declarar instalable una entrega, documentar sistema compatible, toolchain y versión Core probados, dependencias, compilación o binarios verificados, configuración por red, cookie RPC, rutas de datos, arranque, comprobación de salud y actualización/backup/recuperación.
- Incluir ejemplos separados de mainnet, Signet y Testnet4 sin credenciales. Los valores y comandos exactos se añadirán cuando existan y se hayan probado; no inventarlos ahora.

El paquete beta ya existe. Ejecutar `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked` y `cargo build --release --locked`. Las instrucciones reales y el harness de Core están en README y `scripts/core_smoke.py`.

La beta cubre la fundación y el archivo/indexación en memoria, no las fases C–F. Incluye selección mainnet/Signet/Testnet4/regtest; no equivale a sincronización completa ni servidor Electrum operativo. Los índices se reconstruyen desde el archivo en cada invocación y tienen límite de transacciones. El archivo sincronizado no representa todavía el commit coordinado de §5 y no autoriza poda. Formato inicial `ELBETA01`, segmentos hasta 64 MiB, Zstandard nivel 3, lecturas descomprimidas hasta 4 000 000 bytes y ventana máxima de 8 MiB: parámetros iniciales, no resultados de optimización.

## 10. Pendientes y riesgos

- Versiones exactas, licencia, backend, serialización, límites de segmentos/cachés, ventana undo y recuperación fuera de ella.
- Perfil Electrum y versiones de Sparrow, control de disco durante IBD y procedimiento ante espacio crítico.
- Compresión y consultas compiten por CPU; ahorro y funcionamiento en hardware antiguo siguen sin demostrar.
- Core y la app conservan estados UTXO separados; no compartir ni abrir directamente la base interna de Core.
- El histórico mainnet puede requerir cientos de GB; índices y duplicación reciente pueden anular el ahorro. No hay tamaño final ni hardware mínimo validados.

## 11. Fuentes a verificar antes de implementar

- Bitcoin Core 31.0 como referencia, no versión impuesta: https://github.com/bitcoin/bitcoin/blob/v31.0/src/rpc/blockchain.cpp
- Electrum; fijar versión, no aplicar cambios de `latest` sin revisión: https://electrum-protocol.readthedocs.io/en/latest/protocol-methods.html
- Rust Bitcoin: https://github.com/rust-bitcoin/rust-bitcoin
- Tokio: https://tokio.rs/
- Zstandard: https://github.com/facebook/zstd
- LZ4: https://github.com/lz4/lz4

## 12. Control de cambios

| Versión | Fecha | Cambio |
|---|---|---|
| 1.0.0-draft | 2026-10-28 | Propuesta inicial LMDB, segmentos propios y archivo TX; sin implementación. |
| 1.1.0-draft | 2026-10-28 | Revisión acordada: simplicidad, ejecutable único, bloques completos Zstandard, backend transaccional único por elegir, RPC inicial, Signet/Testnet4 y fases A–F. Retirados motor propio y benchmark A/B/C obligatorio. |
| 1.2.0-draft | 2026-10-28 | Mainnet como soporte obligatorio del producto; GitHub público e instrucciones de instalación verificables por entrega. Pruebas iniciales en Signet/Testnet4 y poda mainnet condicionada a validación. |
| 1.3.0-draft | 2026-10-28 | Beta 0.0.1-beta.1 parcial: CLI, RPC local por cookie, archivo de bloques Zstandard, índices en memoria, consultas offline, tests y CI. Persistencia de índices, undo, poda y servidor Electrum siguen pendientes. |
