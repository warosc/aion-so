# Notas de Fase 4 — Almacenamiento y shell

Lo más reciente arriba. Salida de la fase (ROADMAP.md): **crear, leer y
persistir un archivo entre reinicios** — cumplida en el Incremento 32, con
diez arranques seguidos sobre el mismo disco contando cuántos van.

Decisiones de alcance tomadas al abrir la fase: **virtio-blk** como primer
driver de almacenamiento, **FAT32 empezando por solo lectura**, y **ELF64
estático** como formato ejecutable —el ADR 0014 dejó el binario plano
incrustado explícitamente como provisional "hasta que haya filesystem"—.

## Incremento 32 — Escribir, y que lo escrito siga ahí

`docs/adr/0027-fase4-fat32-write.md`. **La salida de Fase 4**: *crear, leer
y persistir un archivo entre reinicios.*

Leer mal da una respuesta equivocada y se nota al momento. Escribir mal
deja un disco roto que se descubre después, y con él se pierde lo que
hubiera dentro.

### Qué hace

- **Crear, extender y sobrescribir un fichero en el directorio raíz.** Ni
  borrar, ni truncar, ni subdirectorios: lo que la salida de fase necesita.
- **El orden, que es la decisión entera**: primero los datos, después la
  cadena, y el directorio **al final, en una sola escritura de un sector**.
  Cada paso deja el volumen en un estado que otro lector entiende. Escribir
  en clusters que ninguna cadena nombra no cambia nada; escribir la cadena
  deja, en el peor caso, clusters ocupados que no son de nadie —una cadena
  perdida, que `fsck` sabe nombrar—; y la entrada del directorio es lo que
  hace aparecer el fichero. Al revés, un corte deja un directorio que
  apunta a clusters que todavía no son suyos: un disco que miente.
- **Las dos tablas se escriben las dos.** Un volumen cuyas tablas no
  coinciden es el que todo lo demás llama dañado.
- **Si no hay clusters libres suficientes, no se escribe nada.** Se reserva
  antes de tocar un byte: un fichero a medias porque el disco se llenó es
  peor que un fichero que no está.
- **Lo que un fichero no llena se pone a cero**, no se deja como estaba.
  Devolver lo que era de otro es como un disco filtra.
- **Los clusters del fichero viejo se liberan después** de que el
  directorio diga el tamaño nuevo, por la misma razón que el orden de
  arriba.
- **La imagen deja de reescribirse en cada arranque.** Pasa a ser una
  salida de `build`: si se rehiciera antes de cada arranque, borraría justo
  lo que hay que demostrar.

### Verificación ejecutada: la salida de fase

Diez arranques seguidos sobre **el mismo disco**:

```
this is boot 10; BOOTS.TXT said 9 and now says 10, in 2 byte(s) from cluster 22
```

El fichero lo creó el primer arranque y lo leyó el segundo, con nada entre
ellos salvo el disco. El número solo crece porque lo escrito sobrevivió.

- **7-Zip, sobre la imagen después de que el kernel escribiera en ella**:
  cinco ficheros, `Everything is Ok`, y `BOOTS.TXT` contiene el número que
  el registro dice. Es la regla del ADR 0025 punto 5 aplicada al escritor:
  lo que este kernel escribe lo lee otro.
- **`fsck.vfat` en CI pasa a comprobar dos imágenes**: la que `xtask`
  dispone, y —en el trabajo de arranque, después de once arranques— la que
  el kernel ha estado escribiendo. Ahí es donde un escritor se gana el
  sueldo.
- Host: 330 pruebas (318 + 12), `fmt-lint` limpio.
- `boot-test --repeat 10` 10/10, soak de 120 s PASS.
- Mutación: 17, las 17 detectadas — **después de arreglar seis pruebas**.

### Seis mutaciones que sobrevivieron, y lo que decían

La primera vuelta detectó 11 de 17. Los seis supervivientes no eran código
malo: eran pruebas mías que solo recorrían el camino feliz.

1. **Un nombre en minúsculas se guardaba tal cual**: ninguna prueba
   escribía uno.
2. **Los cuatro bits reservados de una entrada se machacaban**: ninguna
   prueba los tenía puestos.
3. **Una entrada fuera del volumen se escribía igual.** Esta era distinta:
   la comprobación era **inalcanzable** desde dentro del módulo, porque
   todos los llamantes acotan su cluster antes. En vez de quitarla, la
   operación pasó a ser pública: así la guarda está donde se puede confiar
   en ella y donde una prueba la alcanza.
4. **Un fichero a medias cuando el disco se llena**: ninguna prueba llenaba
   el disco. Ahora una marca todas las entradas de las dos tablas a mano y
   comprueba que el directorio no se toca.
5. **Lo que un fichero no llena se quedaba como estaba**: hay que poner
   basura en un cluster libre y comprobar que desaparece.
6. **Una entrada reescrita conservaba campos de la vieja**: las fechas, que
   este kernel no usa, seguían diciendo algo de un fichero que ya no
   estaba.

Dos de esas pruebas las escribí mal a la primera, y el fallo enseñó cosas
del código: la primera entrada del raíz es la **etiqueta del volumen**, no
un fichero; y escribir **reserva antes de liberar**, así que un fichero
reescrito no cae en el cluster del que sustituye.

### Riesgos y límites

- **Sin journal y sin barreras.** Un corte entre dos sectores deja lo que
  el orden permite y nada peor: en el peor caso, clusters ocupados que no
  son de nadie. Este kernel no promete atomicidad; promete que el peor caso
  es recuperable.
- **Primer hueco, buscando desde el principio de la tabla**, sin mapa de
  bits. Es lento y no tiene estado que mantener de acuerdo con el disco.
- **Solo el directorio raíz, solo 8.3, y el fichero entero de una vez.**
  Escribir a trozos exige saber dónde se quedó, y eso es un descriptor de
  fichero.
- **El kernel puede ahora dejar un disco peor de lo que lo encontró.** Es
  la primera vez, y es la razón de que el orden sea una decisión y no un
  detalle.

## Incremento 31 — Los programas de usuario pasan a ser programas

`docs/adr/0026-fase4-elf-user-programs.md`. El ADR 0014 eligió un binario
plano incrustado como primer programa de usuario y dijo cuándo se
revisaría: *"El formato de verdad se decide cuando haya filesystem (Fase
4)."* Ya lo hay.

Los programas incrustados habían llegado a su límite: ocho, escritos a mano
en hexadecimal dentro de `kernel/src/user.rs`, con los desplazamientos
calculados a mano y fijados por pruebas que comprueban byte a byte que el
comentario dice la verdad. Eso fue lo correcto para demostrar un salto a
ring 3; no es forma de escribir una shell.

### Qué hace

- **Un crate de usuario de verdad**, `user/hello`, compilado para
  `x86_64-unknown-none`. No es parte del kernel y no se enlaza con él: se
  compila aparte, acaba en el disco, y el kernel no sabe de él más que su
  nombre.
- **ELF64 estático.** El toolchain produce un PIE por defecto, que
  necesitaría reubicarse al cargar; se le pide explícitamente
  `relocation-model=static`, `-no-pie` y una base de imagen, y entonces
  sale un `ET_EXEC` con su entrada donde el kernel la espera.
- **Los permisos salen de los segmentos**, no de una convención. Lo que el
  arranque enseña —`r--` para los datos de solo lectura y `r-x` para el
  código— lo dice el fichero, y un segmento que pidiera escritura **y**
  ejecución se rechaza: el ADR 0008 vale también para un programa de
  usuario.
- **El fichero se valida entero antes de mapear nada**: la cabecera, la
  tabla de cabeceras de programa, y de cada segmento que quepa en el
  fichero, que no encoja, que no cruce a la mitad alta y que no pida W y X
  a la vez. Un ELF es un fichero del disco, o sea dato de fuera.
- **`p_memsz` mayor que `p_filesz` se pone a cero**: es el `.bss`, y como
  los marcos llegan a cero del asignador, basta con no escribir encima.
- **Un `Process` deja de tener dos rangos fijos** —código y pila— y pasa a
  tener uno por segmento más la pila, con los marcos que haya tomado
  anotados con su propósito. Las dos formas de arrancar un proceso
  comparten esa contabilidad.
- **Los programas escritos a mano siguen ahí**, los que se portan mal a
  propósito (ADR 0020): un compilador no produce un programa que escribe
  en su propio código.

### Verificación ejecutada

- QEMU, que es lo que lo demuestra:

  ```
    HELLO.ELF — 6360 byte(s), from cluster 3
  HELLO.ELF is 6360 byte(s), read off the disk
  it is an ELF with 2 loadable segment(s), entry 0x401240, reaching 0x401285
    segment at 0x400000, 492 byte(s) of file and 492 of memory, r--
    segment at 0x4011f0, 149 byte(s) of file and 149 of memory, r-x
  a process from an ELF: space at 0x5d9000, 2 segment(s), entry 0x401240,
  a stack at 0x500000
  process 8 runs in slot 8
  from ring 3: HARLAN: hello from a program that came off the disk
  from ring 3: HARLAN: compiled by the toolchain, loaded as ELF
  the process in slot 8 exited with 0
  ```

- **La contabilidad sigue cuadrando** con nueve procesos:
  `91 frame(s) back from the processes that ended; the allocator has the
  62740 it started with`.
- **Pruebas negativas**, rompiendo los bytes del ELF en `xtask` *después*
  de que el toolchain haya producido uno bueno —así se prueba el cargador
  y no el compilador—:
  - poniéndole el bit de escritura al segmento ejecutable:
    `SegmentWritableAndExecutable { at: 4198896 }`, que es `0x4011f0`;
  - moviendo un segmento a la mitad alta: `SegmentOutsideUserSpace`;
  - rompiendo el número mágico: `NotElf`.
  Las tres llegan al shell y los otros ocho procesos siguen su camino.
- Host: 318 pruebas (309 + 9), `fmt-lint` limpio —con el crate nuevo
  añadido a clippy, en su propio target—.
- `boot-test --repeat 10` 10/10, soak de 120 s PASS.
- Mutación: 23, las 23 detectadas. Dos sobrevivieron primero y eran huecos
  reales: ninguna prueba tenía una tabla de cabeceras fuera del fichero, y
  ninguna tenía un fichero **sin** segmentos cargables —sin esa, quitar la
  comprobación daba otro error y parecía bien—.

### Riesgos y límites

- **Estático y nada más**: sin enlazado dinámico, sin intérprete, sin
  reubicaciones.
- **Dos segmentos en una misma página se rechazan** con nombre, en vez de
  adivinar qué permisos debería tener. El enlazador separa los segmentos
  por páginas, así que no ocurre; si ocurriera, el kernel lo diría.
- El programa se lee entero a un buffer del heap antes de analizarlo. Un
  programa grande querrá leerse por segmentos, que es la misma cadena
  recorrida desde un punto.
- `p_vaddr` decide dónde va un segmento, así que el mapa de un proceso ya
  no lo fija el kernel. La pila sí.

## Incremento 30 — Un fichero, leído por su nombre

`docs/adr/0025-fase4-fat32-read-only.md`, que ya estaba decidido. Esto es
el resto del camino: de la tabla de asignación y el directorio raíz a los
bytes de un fichero.

### Qué hace

- **La tabla**, con las cuatro cosas que una entrada puede decir: libre,
  reservada, sigue en otro cluster, medio defectuoso, o fin de cadena.
  Solo los 28 bits bajos son el número de cluster —los cuatro altos están
  reservados y hay que enmascararlos—, y **todo lo que va de `0x0FFFFFF8`
  arriba termina una cadena**, no solo el valor que escribe un formateador;
  un lector que comparase por igualdad se saldría del disco al leer uno
  escrito por otra herramienta.
- **El directorio**, con lo que no es un fichero: la entrada que termina el
  directorio, las borradas, los fragmentos de nombre largo y la etiqueta
  del volumen. El nombre 8.3 recupera su punto, que la entrada no guarda, y
  se compara sin distinguir mayúsculas porque nadie los escribe como se
  almacenan.
- **`Volume` sobre un rasgo `Sectors`**: un disco en el kernel, una imagen
  en una prueba. El lector no sabe que habla con virtio y el disco no sabe
  que guarda un sistema de ficheros.
- **Un fichero se lee hasta donde dice su longitud**, no hasta donde acaba
  su último cluster: lo que hay después del final de un fichero dentro de
  su cluster es lo que hubiera antes.
- **Una cadena rota es un error con nombre**, no un fichero corto: si la
  cadena acaba antes que el fichero, el directorio y la tabla no están de
  acuerdo y eso se dice. Y una cadena más larga que el volumen se
  abandona, porque se apunta a sí misma.

### La prueba que vale

`xtask` escribe la imagen y `hal` la lee; ninguna de las dos cosas sirve
sin la otra, así que la prueba de que se entienden vive donde están las
dos. `xtask` pasa a depender de `harlan-hal` **solo en `dev-dependencies`**
y monta en memoria la imagen que acaba de formatear.

El camino entero —sector de arranque, directorio raíz, tabla, bytes— se
recorre así en una prueba de host, contra los mismos bytes que recibe
QEMU. Lo que antes solo podía fallar en un arranque ahora falla en medio
segundo.

### Verificación ejecutada

- QEMU, que es lo que lo demuestra:

  ```
    HELLO.TXT — 27 byte(s), from cluster 3
    LONG.BIN — 2049 byte(s), from cluster 4
    EMPTY.BIN — 0 byte(s), from cluster 0
  3 thing(s) in the root directory
  HELLO.TXT is 27 byte(s) and reads "HARLAN reads its own disk.", which is
  what is in it
  LONG.BIN is 2049 byte(s) across 5 cluster(s), every one of them what it
  should be
  ```

  Tres cosas en el directorio y no cuatro: la etiqueta del volumen no es un
  fichero. `LONG.BIN` son cuatro clusters y un byte, así que un lector que
  parase en el límite de un cluster, o que leyera hasta el final del
  último, no daría esos 2049 bytes correctos.
- **Prueba negativa**: liberando el segundo cluster de la cadena de
  `LONG.BIN` en la imagen, el kernel dice
  `could not be read (BrokenChain { cluster: 5, entry: Free })` —y sigue
  hasta el shell—. Mira lo que dice cada entrada en vez de limitarse a
  seguirla.
- Host: 309 pruebas (299 + 10), `fmt-lint` limpio.
- `boot-test --repeat 10` 10/10, soak de 120 s PASS.
- Mutación: 19, las 19 detectadas. Dos sobrevivieron primero:
  - **Que una cadena rota se leyera como un fichero completo.** Ninguna
    prueba de host rompía una cadena; solo lo hacía la sonda de arranque.
    Añadida una que corrompe la imagen y comprueba el error exacto.
  - **Quitar el atajo del fichero vacío no cambiaba nada**, porque el bucle
    ya no se ejecuta cuando la longitud es cero. Era una rama que ninguna
    prueba podía tomar, así que se ha ido y su razón está donde está el
    bucle.

### Riesgos y límites

- **Solo el directorio raíz**: no se baja a subdirectorios. La estructura
  es la misma —un directorio es una cadena como cualquier fichero— y lo que
  falta es el camino, no el mecanismo.
- **Sin nombres largos**, 8.3 y nada más.
- **Solo lectura**: nada escribe en el disco.
- Un fichero se lee entero en un buffer que el llamante da. Para un fichero
  grande hará falta leerlo a trozos, que es la misma cadena recorrida desde
  un punto.

## Incremento 29 — El disco con un sistema de ficheros de verdad

`docs/adr/0025-fase4-fat32-read-only.md`. El kernel lee sectores; un sector
no es un fichero. Este incremento decide el formato y, sobre todo, **contra
qué se comprueba que lo leemos bien**.

Esa segunda pregunta es la importante. Un analizador escrito y probado por
la misma persona que escribió la imagen que lee pasa todas sus pruebas
aunque los dos compartan el mismo malentendido. Eso no es verificación, es
un eco.

### Qué hace

- **FAT32, empezando por solo lectura**, sin particiones y con nombres 8.3
  —las entradas de nombre largo son un añadido que se puede ignorar para
  leer un disco entero—.
- **La imagen la construye `xtask`**, no el sintetizador `fat:` de QEMU,
  que al pedirle FAT32 avisa: *"FAT32 has not been tested"*.
- **Y la comprueban otros**: `fsck.vfat` en CI, y 7-Zip en local. Son
  implementaciones escritas por otra gente que saben qué es una imagen
  FAT32 válida.
- **El BPB se valida, no se cree.** Es dato que viene de fuera del kernel y
  cada número se usa para calcular una dirección: tamaño de sector,
  sectores por cluster —cero divide por cero—, número y tamaño de las
  tablas, cluster raíz, y que las tablas quepan en el volumen. La
  aritmética que podría desbordar se hace en 64 bits.
- **El disco crece a 64 MiB.** No es gusto: FAT32 exige más de 65 525
  clusters y por debajo la especificación dice que el volumen es FAT16
  diga lo que diga su BPB. Un disco pequeño con un BPB que dice FAT32 es
  exactamente la imagen que un lector descuidado acepta y `fsck` rechaza.
- **Los marcadores en crudo del Incremento 28 desaparecen**: el sector 0 es
  ahora el BPB. Lo que demostraban —que el número de sector llega al
  dispositivo— lo demuestra la copia de seguridad que FAT32 guarda en el
  sector 6, y de paso comprueba que la imagen la tiene donde debe.

### Dos fallos que encontraron las pruebas, y uno que encontró otro

1. **El tamaño de la tabla oscilaba.** El número de clusters depende del
   tamaño de la tabla y el tamaño de la tabla depende del número de
   clusters. Perseguir eso como punto fijo **no converge**: alterna entre
   dos tamaños, cada uno una entrada corto de lo que el otro implica, y se
   queda en el que toque cuando se acaba el bucle. Lo destapó una prueba
   escrita antes del código —"la tabla tiene que caber para los clusters
   que describe"— y se arregló preguntándolo como un sí o un no: una tabla
   de `n` sectores sirve o no sirve, y como crecer solo lo hace más fácil,
   el menor que sirve se busca partiendo el rango por la mitad.
2. **La prueba de clusters grandes estaba mal, no el código.** Con ocho
   sectores por cluster el volumen de 64 MiB ya no llega al mínimo de
   FAT32, y el analizador lo rechazó. Tenía razón.
3. **Y el que importa: 7-Zip se negó a leer `EMPTY.BIN`.** Mi formateador
   le daba un cluster a un fichero vacío. La especificación dice que un
   fichero de longitud cero tiene primer cluster **0**: un cluster
   asignado que no pertenece a nadie es lo que un comprobador llama cadena
   perdida. Y mi propia prueba afirmaba lo contrario —"y un cluster de
   todas formas"— porque el formateador y la prueba tenían el mismo
   malentendido escrito dentro. Eso es exactamente lo que el punto 5 del
   ADR 0025 existe para atrapar, y lo atrapó a la primera.

### Verificación ejecutada

- **7-Zip**, en local, sobre la imagen: `Type = FAT`, `File System = FAT32`,
  `Label = HARLAN`, `Cluster Size = 512`, los tres ficheros con sus
  tamaños (27, 2049, 0) y, al extraerlos, `Everything is Ok` y el contenido
  correcto byte a byte.
- **`fsck.vfat -n -V`** en CI, en su propio trabajo, sobre la imagen que
  `cargo xtask build` produce.
- QEMU, que es lo que demuestra que el kernel lo lee:

  ```
  the disk holds FAT32: 131072 sector(s) of 512 byte(s), 129022 cluster(s)
  of 1 sector(s), 2 table(s) of 1009 sector(s) from sector 32, root at
  cluster 2 (sector Some(2050)), data from sector 2050
  sector 6 holds the same boot sector as sector 0, byte for byte
  ```

  Los números coinciden uno a uno con los que `xtask` dijo al formatear
  —131072 sectores, 129022 clusters, 1009 sectores por tabla, raíz en el
  cluster 2—, que es la comparación que importa: el que escribe y el que
  lee coinciden, y un tercero dice que la imagen es válida.
- **Pruebas negativas**: con un `sectors_per_cluster` de cero el kernel
  dice `does not hold a FAT32 volume (SectorsPerCluster { found: 0 })` en
  vez de dividir por cero; con el tamaño de tabla de 16 bits puesto,
  `NotFat32 { sixteen_bit_fat_size: 256 }`. Las dos llegan al shell.
- Host: 299 pruebas (285 + 14), `fmt-lint` limpio.
- `boot-test --repeat 10` 10/10, soak de 120 s PASS.
- Mutación: 25, las 25 detectadas.

### Riesgos y límites

- **Solo lectura**, y nada lee todavía un fichero: este incremento llega al
  BPB. La cadena de clusters y el directorio raíz son el siguiente.
- **Sin nombres largos**: 8.3 y nada más.
- **Sin particiones**: el volumen empieza en el sector 0.
- CI gana una dependencia, `dosfstools`, y un paso que puede fallar por
  algo que no es el kernel. Es el precio de tener una segunda opinión.
- La imagen se reescribe en cada `build`. Es deliberado —una entrada que
  se mueve porque una ejecución anterior escribió en ella hace que una
  prueba que pasa no signifique nada— y querrá revisarse cuando el kernel
  escriba en el disco.

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
