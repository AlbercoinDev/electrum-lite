# Poda con pares P2P conectados: investigación

Estado: **no implementado**. La poda automática actual sigue desactivando P2P
de Core y exige que no haya otros escritores de cadena. Esto reduce disponibilidad
de red y puede requerir reconectar los pares, pero no debe eliminarse esa barrera
sin sustituirla por una garantía equivalente.

## Lo observado

En Bitcoin Core 32.0, [`pruneblockchain`](https://bitcoincore.org/en/doc/32.0.0/rpc/blockchain/pruneblockchain/)
recibe una altura o fecha; no recibe el hash de punta esperado. Su documentación
advierte que la eliminación local es irreversible. En el [código de Core 32.0](https://github.com/bitcoin/bitcoin/blob/v32.0/src/rpc/blockchain.cpp),
esa RPC adquiere `cs_main` mientras determina la altura y llama a la poda. Sin
embargo, el `getbestblockhash` anterior de Lite es otra RPC: su comprobación no
comparte ese bloqueo con la llamada de poda. Una extensión o reorganización puede
ocurrir entre ambas. Las [garantías RPC de Core](https://github.com/bitcoin/bitcoin/blob/v32.0/doc/JSON-RPC-interface.md#rpc-consistency-guarantees)
describen consistencia por llamada, no una transacción que abarque las dos.

El margen de 288 bloques reduce ciertos riesgos, pero no constituye una prueba
contra reorganizaciones más profundas. Detectar el cambio después de la RPC no
revierte archivos ya eliminados. También debe considerarse el mempool y la
existencia de otros escritores locales; mantener la sesión P2P no los congela.

## Opciones pendientes de validar

1. **RPC condicional en Core:** una operación que compare dentro del mismo
   bloqueo la punta esperada y las precondiciones y solo entonces pode. Es una
   posible dirección, no una API existente identificada en Core 32.0. Requeriría
   cambios en Core o soporte upstream; un Core modificado no es el binario oficial
   firmado que instala actualmente el proyecto.
2. **Conservar antes de podar los bloques necesarios para recuperar una rama:**
   requeriría diseñar y verificar durabilidad, witness, reorganizaciones profundas,
   recuperación, espacio máximo y eliminación segura. No demuestra por sí sola
   que pueda mantenerse un disco acotado ni evita tener que definir una política
   cuando las fuentes históricas fallen.
3. **Mantener la política actual:** continuar con Core sin red durante el tramo
   irreversible, dejando Electrum activo para consultas permitidas. No elimina la
   reconexión y no garantiza que todas las consultas o difusiones estén disponibles.

Antes de cambiar la política se necesitan pruebas en nodos desechables con
extensiones y reorganizaciones antes, durante y después de la poda; cambios de
mempool; fallos RPC y de disco; caída entre intención y confirmación; y recuperación
de bloques sin fuentes externas. Las pruebas actuales no validan esas opciones.
La poda de mainnet permanece bloqueada.
