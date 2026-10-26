# ADR 0023: Los registros de un dispositivo, y qué virtio hablamos

## Contexto

El kernel ya sabe qué hay en el bus (ADR 0022) y ahí está el disco. Para
hablar con él hacen falta dos cosas que hoy no existen: **alcanzar sus
registros**, que están en direcciones físicas que no son RAM, y **decidir
qué versión del protocolo virtio hablar**, porque el dispositivo que QEMU
presenta habla dos.

El disco aparece como `1af4:1001`: un dispositivo *transicional*. Entiende
virtio legacy —registros en un BAR de puertos de E/S— y virtio 1.0
—registros en memoria, encontrados a través de capacidades PCI—. Elegir es
obligatorio; no elegir significa que el driver funcione por accidente por
el camino que resulte estar activo.

## Decisión

### Qué virtio

1. **Virtio 1.0, el moderno, y solo ese.** Legacy está obsoleto desde
   2014, usa direcciones de cola de 32 bits que no sobrevivirían a una
   máquina con memoria alta, y habría que tirarlo entero.
2. **El dispositivo se configura como moderno-solo** (`disable-legacy=on`),
   así que pasa a anunciarse como `1af4:1042` y deja de tener BAR de E/S.
   No es cosmético: mientras exista el camino legacy, un driver con un
   error puede funcionar por él y nadie se enteraría de que el camino
   moderno está mal. Quitarlo hace que la prueba diga la verdad.
3. **La negociación se hace como manda la especificación**: reinicio
   escribiendo 0 en el estado, `ACKNOWLEDGE`, `DRIVER`, leer lo que el
   dispositivo ofrece, escribir lo que el driver acepta —con
   `VIRTIO_F_VERSION_1` obligatoriamente—, `FEATURES_OK`, y **volver a
   leer el estado** para comprobar que el dispositivo no lo retiró. Un
   dispositivo que rechaza la negociación lo dice ahí, y el kernel se
   queda sin disco en vez de escribir en registros que no acordó.
4. **`DRIVER_OK` no se pone todavía.** La especificación permite quedarse
   entre `FEATURES_OK` y `DRIVER_OK`: el dispositivo está negociado y sin
   colas. Es un punto de parada con nombre, y es el final de este
   incremento.

### Cómo se alcanzan los registros

5. **Las capacidades PCI se recorren desde el puntero de `0x34`**, y las
   de virtio son las de proveedor (`0x09`) con un `cfg_type` que dice qué
   estructura describen. Cada una da un BAR, un desplazamiento y un
   tamaño: eso es todo lo que hace falta para saber dónde está la
   configuración común.
6. **Un BAR se decodifica, no se adivina.** Memoria o E/S según el bit 0;
   64 bits cuando el tipo lo dice, y entonces ocupa **dos** entradas y la
   siguiente no es un BAR sino la mitad alta de este. El BAR moderno de
   virtio en QEMU es de 64 bits, así que esto no es un caso teórico.
7. **Los registros de dispositivo tienen su propia región**, la séptima
   del espacio del kernel (`KERNEL_DEVICES_START`, ranura 262 del PML4).
   Un dispositivo en `p` se lee en `KERNEL_DEVICES_START + p`, igual que
   la ventana física y los runtime services: una suma, sin tabla que
   mantener.
8. **Mapeados **no cacheables** y no ejecutables.** Un registro leído de
   una caché es un registro que no se leyó, y uno escrito con retraso es
   una orden que el dispositivo no ha recibido. `PageFlags` ya sabe
   decirlo desde el ADR 0016; aquí es obligatorio, no una precaución.
9. **Solo lo que una capacidad describe se mapea**, redondeado a páginas,
   y nada más del BAR. Lo que el dispositivo no ha dicho que use no tiene
   por qué estar alcanzable.
10. **Cada lectura y escritura de un registro es volátil y del tamaño que
    la especificación dice.** Un registro de 16 bits leído en dos mitades,
    o una escritura que el compilador reordena, son errores que no dan
    señal: el dispositivo simplemente hace otra cosa.

## Alternativas consideradas

- **Virtio legacy por el BAR de E/S**: la mitad de código, ninguna decisión
  de mapa de memoria, y hay que tirarlo. Fue el mismo razonamiento que
  llevó a `syscall` en vez de `int 0x80` (ADR 0014): lo que se va a
  sustituir no se escribe.
- **Dejar el dispositivo transicional** y hablar moderno de todas formas:
  funciona, y deja vivo un camino por el que un error puede pasar
  desapercibido.
- **Alcanzar los registros por la ventana física** (ADR 0013) en vez de
  una región propia: la ventana está mapeada cacheable, porque describe
  RAM, y los registros de un dispositivo no pueden estarlo. Mezclar las
  dos políticas en una región sería un mapa que dice una cosa y significa
  dos.
- **Mapear el BAR entero**: más simple y alcanza memoria de dispositivo que
  nadie dijo que existiera.
- **ECAM para leer las capacidades**: no hace falta; están en los primeros
  256 bytes, que los puertos alcanzan (ADR 0022).

## Consecuencias

- El espacio del kernel gana una región, la séptima. Es un cambio de
  layout: `docs/memory-safety.md` y las notas lo registran.
- El kernel **escribe** en un dispositivo por primera vez. Enumerar era
  leer (ADR 0022); negociar no lo es, y desde aquí un error del kernel
  puede hacer que el hardware haga algo.
- Las interrupciones del dispositivo siguen sin usarse: no hay MSI-X ni
  manejador de la línea INTx. El siguiente incremento sondea la cola, y
  eso queda dicho como límite ahí.
- La decodificación de BAR y el recorrido de capacidades son puros y viven
  en `hal`, probados contra una máquina que no existe, como el recorrido
  del bus.
