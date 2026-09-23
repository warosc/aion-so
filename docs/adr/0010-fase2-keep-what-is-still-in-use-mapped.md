# ADR 0010: El kernel no reconstruye el mapa si perdería algo que sigue usando

## Contexto

El ADR 0007 dio al kernel su propio mapa de identidad y el ADR 0008 lo dejó
no ejecutable salvo los rangos de código que todavía se alcanzan por él. El
mapa cubre los primeros 4 GiB (`DEFAULT_IDENTITY_LIMIT`).

La revisión cruzada de Codex encontró que nada comprobaba que lo que sigue en
uso **quepa** en ese mapa. Dos agujeros, uno por cada lado:

- **Un rango ejecutable fuera del límite** se descartaba en silencio. El
  kernel construía el mapa, lo cargaba en CR3 y fallaba en la siguiente
  instrucción del código que acababa de declarar que ejecuta. Lo mismo con un
  `page_count` de la tabla del firmware que se desborda al multiplicar.
- **El framebuffer** se quedaba sin mapear si el firmware lo colocaba por
  encima del límite. La consola de hardware sigue escribiendo por su alias de
  identidad, así que el siguiente carácter habría provocado un fallo de
  página. Nadie se lo estaba pasando al kernel.

Cerrarlo añade un campo a `BootInfo`, que es el contrato de arranque entre
`boot` y `kernel`, y el ADR 0008 ya trató añadir un campo como algo que lleva
ADR. De ahí este.

## Decisión

1. **`boot` pasa el rango del framebuffer** en `BootInfo::framebuffer`, junto
   al de la imagen del kernel y por el mismo motivo: solo el firmware sabe
   dónde está, y solo antes de `ExitBootServices`. Si no hay framebuffer, el
   campo es `None` y no restringe nada.
2. **`rebuild_identity` valida los rangos ejecutables** que recibe: longitud
   cero, desbordamiento o cualquier parte por encima del límite se rechazan
   con `RequiredRangeOutsideIdentityMap`, antes de escribir una sola tabla.
3. **El kernel comprueba el framebuffer** antes de reconstruir, y un
   `page_count` que se desborda en una región `RuntimeCode` cancela la
   reconstrucción.
4. **Ante cualquiera de esas condiciones, el kernel se queda con las tablas
   del firmware** y lo registra como error. Es la misma política que cuando no
   conoce ningún rango ejecutable (ADR 0008, punto 4): un mapa al que le falta
   algo que está en uso es peor que un mapa sin endurecer.

## Alternativas consideradas

- **Subir el límite hasta cubrir el framebuffer**: mantendría W^X y la página
  nula en máquinas con el framebuffer alto, pero mapearía además todos los
  huecos de MMIO intermedios y costaría tablas proporcionales al salto. Queda
  como opción si aparece hardware así.
- **Mapear el framebuffer en espacio de kernel** y dejar de usar su alias de
  identidad: es la solución de verdad, y lo natural cuando la mitad baja pase
  a ser espacio de usuario (Fase 3). Entonces este campo dejará de restringir
  la reconstrucción y pasará a decir qué mapear.
- **Confiar en que el firmware lo ponga bajo** (lo que había): en QEMU está en
  `0x8000_0000` y funciona; en hardware con mucha RAM no es una garantía.
- **Recortar el rango al límite** en vez de rechazar: dejaría media imagen
  ejecutable o media pantalla mapeada. Un fallo a medias es peor que no
  reconstruir.

## Consecuencias

- `BootInfo` gana un campo (`framebuffer: Option<PhysRange>`); el contrato de
  arranque queda como lo dejaron los ADR 0007 y 0008, más este dato.
- En QEMU no cambia nada: el framebuffer está en `0x8000_0000`, dentro de los
  4 GiB, y el mapa se reconstruye igual (10 tablas, 4 GiB, página nula sin
  mapear).
- Una máquina con el framebuffer por encima del límite arranca con las tablas
  del firmware: sin W^X y con la página nula mapeada, pero **funcionando y
  diciéndolo**, en vez de fallar en el primer carácter.
- El coste es una comprobación por rango antes de reconstruir; ninguna en el
  camino caliente.
