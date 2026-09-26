# ADR 0024: Cómo el kernel y un dispositivo se pasan memoria

## Contexto

El disco está negociado (ADR 0023) y no hay por dónde pedirle nada. Una
petición de virtio no se escribe en un registro: se deja en memoria que el
dispositivo lee **por sí mismo**, en una estructura compartida —la cola—, y
el registro solo sirve para avisar de que hay algo nuevo.

Eso invierte quién manda sobre la memoria. Hasta ahora toda la memoria del
kernel la leía y escribía el kernel. Aquí el kernel le da una dirección
física a un dispositivo y el dispositivo escribe ahí, sin pasar por las
tablas de páginas, sin comprobación de límites y sin nada que lo detenga
si la dirección está mal.

## Decisión

### La memoria que el dispositivo toca

1. **Los marcos de DMA salen del asignador, con un propósito propio**
   (`FramePurpose::Dma`). Es la propiedad por marco del ADR 0004 aplicada
   a lo más peligroso que hay: un marco que el dispositivo puede escribir
   no debe poder acabar siendo una tabla de páginas o una pila. El
   propósito es lo que hace que confundirlos sea un error y no un desastre
   silencioso.
2. **El kernel le da direcciones físicas y las lee por su ventana** (ADR
   0013). Las dos vistas de la misma memoria son deliberadas: el
   dispositivo no camina tablas de páginas, así que lo que se le dice es
   físico; el kernel no puede leer físico directamente, así que lo lee en
   `ventana + dirección`.
3. **No hay IOMMU.** Lo que se le diga al dispositivo, lo escribe. No hay
   segunda comprobación, ni forma de limitarlo desde aquí. La corrección
   descansa entera en que las direcciones vienen del asignador y se
   traducen en un solo sitio; queda dicho porque es la propiedad más
   frágil de todo el subsistema.
4. **Nada de DMA en memoria que se mueva o se reutilice mientras el
   dispositivo la tiene.** Una petición en vuelo es memoria prestada: el
   kernel no la devuelve hasta que el dispositivo ha dicho que terminó.
5. **Las tres partes de la cola y los tres trozos de una petición caben en
   un marco**, con la cola dimensionada a cuatro descriptores. Sin
   asignación por petición, y todo lo que el dispositivo toca en un sitio
   que se puede enseñar entero en el registro.

### La cola

6. **Cola partida (*split*), no empaquetada.** Es la que toda
   implementación entiende, incluida la que no anuncie `RING_PACKED`. La
   empaquetada es mejor y es una optimización.
7. **La cola se encoge a cuatro descriptores.** El dispositivo ofrece 256;
   una petición de bloque usa tres —cabecera, datos, estado—. Cuatro es la
   potencia de dos más pequeña que sirve, y **se vuelve a leer el registro
   después de escribirlo** para comprobar que el dispositivo aceptó el
   tamaño, porque un dispositivo que lo ignorara dejaría al kernel con
   anillos de un tamaño que no es el que el dispositivo cree.
8. **Los índices son ventanas, no contadores.** `avail.idx` y `used.idx`
   crecen para siempre y dan la vuelta a los 16 bits; la entrada es
   `idx % tamaño`. Tratarlos como contadores funciona durante las primeras
   65 536 peticiones.
9. **Se sondea, no se interrumpe.** Después de avisar, el kernel lee
   `used.idx` hasta que cambia, **con un límite**: un dispositivo que no
   contesta hace que el kernel lo diga y siga, no que el arranque se
   quede ahí. Las interrupciones de dispositivo exigen MSI-X o la línea
   INTx, y eso es un incremento propio.
10. **Una petición a la vez.** Con una sola en vuelo no hace falta llevar
    la cuenta de qué descriptor es de quién, y ese es el estado que hará
    falta el día que haya varias.
11. **Ordenar las escrituras es parte del protocolo.** El descriptor tiene
    que estar escrito antes de que su índice aparezca en el anillo
    disponible, y el índice antes del aviso; el dispositivo puede estar
    mirando. Las escrituras son volátiles y llevan barreras de compilador
    entre los pasos.

## Alternativas consideradas

- **DMA en memoria estática del kernel**, en su `.bss`: no hace falta el
  asignador y pone memoria escribible por un dispositivo dentro de la
  imagen, al lado del código y de las tablas. Un error de dirección dejaría
  de ser un error en un marco suelto.
- **Cola empaquetada**: menos memoria y menos accesos, y exige negociar
  `RING_PACKED`, que un dispositivo puede no ofrecer. Con una petición cada
  varios segundos no se nota.
- **Usar los 256 descriptores** que el dispositivo ofrece: tres marcos de
  anillos para usar tres descriptores.
- **Interrupciones desde el principio**: es lo correcto para trabajo real y
  mete el manejo de peticiones en el camino de interrupción antes de que
  una sola petición haya funcionado. Primero que funcione y se vea.
- **Esperar para siempre** a que el dispositivo conteste: es lo que hace un
  driver que da por hecho que el hardware funciona. Un límite convierte un
  cuelgue en una línea de registro.

## Consecuencias

- El asignador gana un propósito, y el registro del arranque dirá cuántos
  marcos hay en DMA.
- El kernel pasa a tener memoria que otro puede escribir. Es una clase de
  error nueva: no hay `unsafe` que la marque, porque desde el punto de
  vista de Rust el kernel solo escribió un número en un registro.
- El disco pasa a estar en `DRIVER_OK`, que es lo que dice que un driver
  está listo. A partir de aquí el dispositivo actúa por su cuenta.
- El tamaño de la cola y el número de peticiones en vuelo son ambos uno de
  esos números que habrá que subir; los dos están en un sitio.
