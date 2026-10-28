# Electrum Lite

Indexador Bitcoin y servidor Electrum propuesto en Rust, orientado a equipos con recursos limitados y conectado a Bitcoin Core con poda manual.

**Estado: diseño, sin implementación.** Todavía no hay aplicación compilable, binarios, instalador ni benchmarks. No se puede instalar ni ejecutar un servidor Electrum Lite en este momento.

## Diseño previsto

- Un ejecutable Rust y una base de datos transaccional.
- Archivo local de bloques completos comprimidos con Zstandard, incluido witness.
- Consulta histórica autónoma después de que Core pode sus bloques.
- Bitcoin Core valida consenso; Electrum Lite indexa y sirve consultas compatibles con Electrum para carteras como Sparrow.
- Simplicidad, corrección y durabilidad antes que optimizaciones propias. No hay ahorro de disco o requisitos mínimos de hardware demostrados.

## Redes

**Mainnet, Signet y Testnet4 son requisitos del producto; aún no están implementados.** Las pruebas de integración iniciales se harán en Signet y Testnet4, con regtest como apoyo para fallos y reorganizaciones controlados.

Mainnet tendrá selección explícita de red, configuración y datos aislados y verificación de identidad contra Core. No habilitar poda real ni desplegar en mainnet antes de validar recuperación, integridad y capacidad de disco.

## Obtener el proyecto

Se necesita Git para descargar la documentación:

```sh
git clone https://github.com/AlbercoinDev/electrum-lite.git
cd electrum-lite
```

Lee [`ELECTRUM_LITE_ENGINE_v1.0.md`](ELECTRUM_LITE_ENGINE_v1.0.md), cuya revisión vigente figura dentro del documento. [`AGENTS.md`](AGENTS.md) resume las restricciones para agentes de desarrollo.

## Instalación y ejecución

**Pendientes de implementación.** No hay todavía `Cargo.toml`, comandos de aplicación ni configuración ejecutable. No ejecutes instrucciones de poda de otras herramientas suponiendo que el histórico ya está protegido por este proyecto.

Antes de publicar una entrega instalable, este README incluirá:

1. Sistemas compatibles y versiones probadas de Rust y Bitcoin Core.
2. Dependencias y pasos de compilación o instalación de binarios verificados.
3. Configuración separada para mainnet, Signet y Testnet4, autenticación por cookie y rutas de datos.
4. Comandos de arranque y comprobación de sincronización y salud.
5. Procedimientos seguros de actualización, backup, restauración y poda.

## Desarrollo

El primer objetivo es una CLI que archive bloques de Signet/Testnet4 y compruebe índices en memoria, sin poda. Después: persistencia y recuperación, poda controlada, protocolo Electrum y operación sostenida. El soporte mainnet sigue siendo obligatorio.

Las dependencias, toolchain, backend transaccional y licencia están pendientes de selección. No hay todavía una suite de pruebas ejecutable.

## Licencia

Pendiente de elección. Que el repositorio sea público no implica una licencia de código abierto ni una autorización general de reutilización.
