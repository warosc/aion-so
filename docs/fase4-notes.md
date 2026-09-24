# Notas de Fase 4 — Almacenamiento y shell

Lo más reciente arriba. Salida de la fase (ROADMAP.md): arrancar, leer y
escribir ficheros desde una shell de usuario.

Decisiones de alcance tomadas al abrir la fase: **virtio-blk** como primer
driver de almacenamiento, **FAT32 empezando por solo lectura**, y **ELF64
estático** como formato ejecutable —el ADR 0014 dejó el binario plano
incrustado explícitamente como provisional "hasta que haya filesystem"—.

## Incremento 28 — Un sector, leído de verdad

`docs/adr/0024-fase4-dma-and-the-queue.md`. El disco estaba negociado y sin
nada por donde pedirle. Una petición de virtio no se escribe en un
registro: se deja en memoria que el dispositivo lee **por sí mismo**, y el
registro solo sirve para avisar de que hay algo nuevo.

Eso invierte quién manda sobre la memoria. Hasta ahora toda la del kernel
la leía y escribía el kernel. Aquí el kernel le da una dirección física a
un dispositivo y el dispositivo escribe ahí: sin pasar por las tablas de
páginas, sin comprobación de límites, y sin nada que lo detenga si la
dirección está mal.

### Qué hace

- **Los marcos de DMA salen del asignador con un propósito propio**
  (`FramePurpose::Dma`). Es la propiedad por marco del ADR 0004 aplicada a
  lo más peligroso que hay: un marco que el hardware puede escribir no
  debe poder acabar siendo una tabla de páginas o una pila.
- **Dos vistas de la misma memoria, deliberadamente**: al dispositivo se le
  dan direcciones **físicas**, porque no camina tablas; el kernel la lee en
  `ventana + dirección física`.
- **Cola partida de cuatro descriptores.** El dispositivo ofrece 256 y una
  petición usa tres —cabecera, datos, estado—; cuatro es la potencia de dos
  más pequeña que sirve, y todo cabe en un marco. Se **vuelve a leer** el
  registro del tamaño después de escribirlo, porque un dispositivo que lo
  ignorara dejaría al kernel con anillos de otra forma que la que el
  dispositivo cree.
- **Los índices son ventanas, no contadores**: crecen para siempre y dan la
  vuelta a los 16 bits, y la entrada es `idx % tamaño`. Tratarlos como
  contadores funciona durante las primeras 65 536 peticiones.
- **Tres descriptores porque los permisos son tres**: la cabecera la lee el
  dispositivo, los datos y el byte de estado los escribe. Un dispositivo
  que pudiera escribir la cabecera podría cambiar lo que se le pidió.
- **El byte de estado se precarga a `0xFF`**, un valor que el dispositivo
  nunca escribe, para que "funcionó" no pueda leerse de memoria que ya
  estaba a cero.
- **Se sondea con límite.** Un dispositivo que no contesta es una línea en
  el registro, no un arranque que se queda ahí.
- **El orden de las escrituras es parte del protocolo**: el descriptor
  antes de su índice en el anillo, el índice antes del aviso, con barreras
  entre los pasos, porque el dispositivo puede estar mirando.

### Verificación ejecutada

- QEMU, que es lo que lo demuestra:

  ```
  sector 0 of the disk reads "HARLAN-DISK-0", which is what is there
  sector 8 of the disk reads "HARLAN-SECTOR-8", which is what is there
  ```

  **Dos sectores, no uno.** Leer solo el 0 no distingue un driver que pide
  el sector 0 de uno cuyo número de sector nunca llega al dispositivo: los
  dos dan los mismos bytes. `xtask` escribe un segundo marcador en el
  sector 8 y el kernel comprueba los dos contra lo que debería haber.
  (Esto lo descubrí porque la primera versión del cambio en `xtask` falló
  en silencio: el sector 8 leyó ceros, que ya probaba que el número
  llegaba, pero la comprobación era accidental en vez de positiva.)
- **Prueba negativa**: pidiendo el sector 100 000 de un disco de 16 384,
  `sector 100000 could not be read (Failed { status: 1 })` —error de E/S—
  y el arranque llega al shell. El byte de estado se lee de verdad: el
  dispositivo lo escribió sobre el `0xFF` precargado.
- Host: 285 pruebas (275 + 10: el trazado de la cola y lo que no cabe, un
  descriptor campo por campo, publicar y recoger contra un anillo en
  memoria ordinaria donde la prueba hace el papel del dispositivo, el
  timbre y su multiplicador, y la cabecera de una petición).
- `boot-test --repeat 10` 10/10, soak de 120 s PASS, `fmt-lint` limpio.
- Mutación: 23, las 23 detectadas. Dos hicieron falta arreglos de verdad:
  una comprobación del trazado que otra tapaba —la del anillo usado solo
  se puede disparar con un marco más pequeño que el que el trazado supone—
  y otra, la del anillo disponible, que **no se puede disparar nunca** con
  estos desplazamientos. Esa segunda dejó de ser una rama en tiempo de
  ejecución y pasó a ser una aserción del compilador: es un hecho sobre
  tres constantes, así que si una se mueve, el build se para en vez de
  solapar dos anillos en silencio.

### Riesgos y límites

- **Sin IOMMU.** Lo que se le diga al dispositivo, lo escribe. No hay
  segunda comprobación ni forma de limitarlo desde aquí. La corrección
  descansa entera en que las direcciones vienen del asignador y se traducen
  en un solo sitio. Es la propiedad más frágil del subsistema y no hay
  `unsafe` que la marque: desde el punto de vista de Rust el kernel solo
  escribió un número en un registro.
- **Sin interrupciones**: se sondea. Hace falta MSI-X o INTx, y eso es un
  incremento propio.
- **Una petición a la vez**, y cuatro descriptores. Los dos números están
  en un sitio y los dos habrá que subirlos.
- Nada escribe en el disco todavía. `TYPE_OUT` existe y no se usa.

## Incremento 27 — Los registros del disco, y qué virtio hablamos

`docs/adr/0023-fase4-device-registers.md`. El disco está encontrado
(Incremento 26). Para hablar con él faltaban dos cosas: **alcanzar sus
registros**, que están en direcciones físicas que no son RAM, y **decidir
qué versión del protocolo hablar**, porque el dispositivo que QEMU
presentaba habla dos.

### Qué hace

- **Virtio 1.0 y solo ese.** Legacy guarda sus registros detrás de puertos
  de E/S y sus direcciones de cola en 32 bits; está obsoleto desde 2014 y
  habría que tirarlo entero. Es el mismo razonamiento que llevó a
  `syscall` en vez de `int 0x80`: lo que se va a sustituir no se escribe.
- **El dispositivo se configura moderno-solo** (`disable-legacy=on`), así
  que pasa a anunciarse como `1af4:1042` y pierde su BAR de puertos. No es
  cosmético: mientras el camino legacy exista, un driver con un error puede
  funcionar por él y la prueba no diría nada.
- **Las capacidades PCI se recorren** desde el puntero de `0x34` —solo si
  el registro de estado dice que hay lista— y las de virtio dan un BAR, un
  desplazamiento y un tamaño. Una lista que se apunta a sí misma se
  abandona tras 48 entradas en vez de colgar el arranque.
- **Un BAR se decodifica, no se adivina**: memoria o puertos según el bit
  0, y de 64 bits cuando el tipo lo dice —y entonces ocupa **dos** de las
  seis entradas, así que la siguiente no es un BAR sino su mitad alta—.
- **Los registros tienen su propia región**, la séptima del espacio del
  kernel (PML4 262), mapeada **no cacheable** y no ejecutable. Un registro
  leído de una caché es un registro que no se leyó. La ventana física de al
  lado describe RAM y es cacheable; mezclar las dos políticas en una región
  sería un mapa que dice una cosa y significa dos.
- **Solo se mapea lo que una capacidad describe**, redondeado a páginas.
- **El saludo, en el orden que manda la especificación**: reinicio,
  `ACKNOWLEDGE`, `DRIVER`, leer lo ofrecido, escribir lo aceptado —con
  `VERSION_1` obligatoriamente—, `FEATURES_OK`, y **volver a leer el
  estado**, porque el dispositivo retira ese bit cuando no acepta lo
  elegido. Un driver que sigue adelante sin comprobarlo acaba hablándole a
  algo que dejó de escuchar. Si algo falla, el kernel escribe `FAILED` y lo
  dice, en vez de dejar el dispositivo a medio negociar.
- **Se para en `FEATURES_OK`**: negociado y sin colas. `DRIVER_OK` es lo
  que dice que un driver está listo para enviar peticiones, y todavía no
  hay por dónde enviarlas.

### Verificación ejecutada

- QEMU, la cadena entera de la capacidad al registro:

  ```
  a virtio disk at pci 00:03.0 (1af4:1042)
    bar 1: memory at 0x81010000
    bar 4: memory at 0xc000000000, 64-bit, prefetchable
  the disk at pci 00:03.0 is negotiated: registers from bar 4 at
  0xffff83c000000000, offers 0x10130006e54, agreed 0x100000000,
  1 queue(s), queue 0 holds 256 descriptor(s) and is notified at 0
  ```

  El dispositivo ya es `1af4:1042` y **no tiene BAR de puertos**, que es la
  prueba de que legacy está de verdad apagado. El BAR 4 es de 64 bits, así
  que ese camino del decodificador lo recorre la máquina de verdad. La
  dirección mapeada es `KERNEL_DEVICES_START + 0xc000000000`. Y lo leído
  son valores reales: bit 32 puesto en lo ofrecido (`VERSION_1`) y
  `0x100000000` exactamente en lo aceptado.
- **Prueba negativa**: quitando `disable-legacy=on`, el dispositivo vuelve
  a ser `1af4:1001`, aparece `bar 0: ports at 0xc000`, y el driver dice
  `the disk could not be started (Transitional)` **y el arranque llega al
  shell**. Un driver que se niega no es un kernel que se cae.
- Host: 275 pruebas (258 + 17: decodificación de BAR incluida la de 64
  bits y la de puertos, el recorrido de capacidades con una lista que
  contiene capacidades ajenas y otra que se apunta a sí misma, el
  enmascarado de los bits reservados del puntero, y cada registro de la
  configuración común contra un dispositivo hecho de memoria ordinaria).
- `boot-test --repeat 10` 10/10, soak de 120 s PASS, `fmt-lint` limpio.
- Mutación: 24, las 24 detectadas. Una sobrevivió primero —quitar el
  enmascarado de los dos bits reservados del puntero de capacidad— porque
  ninguna prueba usaba un puntero con basura ahí. Añadida la prueba, la
  regla queda escrita donde se comprueba.

### Riesgos y límites

- **Sin interrupciones del dispositivo**: no hay MSI-X ni manejador de la
  línea INTx. El siguiente incremento sondea la cola, y eso se dirá allí.
- El kernel **escribe** en un dispositivo por primera vez. Enumerar era
  leer; negociar no lo es.
- No se acepta ninguna bandera más que `VERSION_1`: cada una de las demás
  cambia cómo es una petición, y todavía no hay peticiones.
- El mapeo de registros acepta una página ya mapeada como caso normal: dos
  estructuras de un dispositivo suelen compartir página. Eso significa que
  no detectaría un solape con otra cosa que ya estuviera ahí, y la región
  es solo de dispositivos.

## Incremento 26 — El kernel pregunta qué hay

`docs/adr/0022-fase4-pci-enumeration.md`. Todo lo que el kernel tocaba
hasta ahora estaba en una dirección que alguien fijó hace cuarenta años:
el PIC en `0x20`, el PIT en `0x40`, el teclado en `0x60`. Un disco no.

### Qué hace

- **El espacio de configuración se lee por los puertos `0xCF8`/`0xCFC`**,
  no por ECAM: ECAM necesita la tabla MCFG de ACPI, que necesita un
  analizador de ACPI, que es un subsistema entero. Los puertos alcanzan
  los 256 buses y los primeros 256 bytes de cada función, que es donde
  viven las capacidades de virtio.
- **El recorrido es exhaustivo y sin recursión**, y solo pregunta por las
  funciones 1 a 7 cuando la función 0 dice que el dispositivo es
  multifunción: uno que no lo es puede contestar por las ocho, y el mismo
  disco aparecería ocho veces.
- **El kernel no configura nada mientras mira.** No asigna BAR, no
  habilita bus mastering, no dimensiona nada —eso exige escribir en el
  registro—. El firmware ya lo hizo. Enumerar es leer.
- **Todo menos `in` y `out` vive en `hal`**, sobre un rasgo `ConfigSpace`
  de una sola operación. La misma forma que `PageTables` sobre
  `TableAccess`: el recorrido entero se prueba contra una máquina que no
  existe.
- **`xtask` le da un disco a la máquina**: una imagen cruda enganchada
  como `virtio-blk-pci`. Nada arranca desde él; el firmware sigue
  arrancando desde la ESP.

### Verificación ejecutada

- QEMU, que es lo que lo demuestra —la máquina entera, encontrada:

  ```
  pci 00:00.0 8086:1237 host bridge (class 06.00)
  pci 00:01.0 8086:7000 ISA bridge (class 06.01)
  pci 00:01.1 8086:7010 IDE storage (class 01.01)
  pci 00:01.3 8086:7113 bridge (class 06.80)
  pci 00:02.0 1234:1111 display (class 03.00)
  pci 00:03.0 1af4:1001 SCSI storage (class 01.00)
  a virtio disk at pci 00:03.0 (1af4:1001), first BAR 0xc001
  ```

  Es exactamente lo que `-machine pc` emula, con nuestro disco en `00:03.0`
  y ni una función repetida. El puente PIIX3 en `00:01.0` **sí** es
  multifunción, así que el camino de las funciones 1 a 7 lo recorre
  también un arranque de verdad, no solo las pruebas.
- Host: 258 pruebas (246 + 12: la dirección de configuración campo por
  campo, la cabecera, la tabla de dispositivos, y el recorrido contra una
  máquina falsa que registra qué se le preguntó).
- **Coste**: ninguno medible. Diez arranques entre 5,24 s y 5,48 s, dentro
  del margen de antes del escaneo (5,68–6,69 s en el incremento previo, en
  la misma máquina). Los ~8200 accesos a puerto no se notan.
- `boot-test --repeat 10` 10/10, soak de 120 s PASS, `fmt-lint` limpio.
- Mutación: 15, las 15 detectadas. Todas alcanzables desde host porque el
  recorrido está en `hal`: desplazar el bus un bit, leer la clase donde
  está la subclase, buscar el bit de multifunción en el sitio equivocado,
  recorrer un solo bus, preguntar por las ocho funciones siempre.

### Lo que hay que saber para el siguiente incremento

- **El dispositivo es transicional**: QEMU presenta `1af4:1001`, no
  `1af4:1042`. Es decir, habla virtio legacy por un BAR de E/S **y**
  virtio 1.0 por capacidades PCI. Cuál de los dos usar es la decisión del
  ADR del driver.
- **El FAT32 sintético de QEMU no sirve de referencia.** Enganchar un
  directorio con `fat:32:rw:` funciona, pero QEMU avisa: *"FAT32 has not
  been tested. You are welcome to do so!"*. Verificar un lector contra una
  implementación no probada no verifica nada. El incremento del filesystem
  tendrá que construir la imagen y comprobarla con algo independiente
  —`fsck.vfat` en CI, que es Linux— en vez de confiar en el sintetizador.

### Riesgos y límites

- La configuración extendida de PCIe (de `0x100` en adelante) es
  inalcanzable por este camino. Nada de lo que este kernel usa la
  necesita todavía.
- 32 funciones como máximo. Pasado eso el kernel dice cuántas perdió.
- No se sigue ningún puente: los 256 buses se visitan a pelo. Encuentra lo
  mismo en esta máquina y es menos código.
