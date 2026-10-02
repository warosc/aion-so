# ADR 0026: Los programas de usuario pasan a ser programas

## Contexto

El ADR 0014, punto 10, eligió un **binario plano incrustado** en la imagen
del kernel como primer programa de usuario, y dijo por qué: no hacía falta
un analizador, ni reubicaciones, ni un formato que mantener para demostrar
que el aislamiento funciona. Y dijo también cuándo se revisaría: *"El
formato de verdad se decide cuando haya filesystem (Fase 4)."*

Ya hay filesystem (ADR 0025), y los programas incrustados han llegado a su
límite. Son ocho ahora, escritos a mano en hexadecimal dentro de
`kernel/src/user.rs`, con sus desplazamientos calculados a mano y fijados
por pruebas que comprueban byte a byte que el comentario dice la verdad.
Eso fue lo correcto para demostrar un salto a ring 3; no es forma de
escribir una shell.

## Decisión

1. **ELF64 estático**, que es lo que el toolchain de Rust ya produce sin
   trucos. Un programa de usuario pasa a ser un crate normal del workspace
   compilado para `x86_64-unknown-none`, no una tabla de bytes.
2. **Estático y nada más**: sin enlazado dinámico, sin intérprete, sin
   reubicaciones. Un `PT_LOAD` que dice dónde va y ahí va. El día que haya
   bibliotecas compartidas será otro ADR, y lo que haya que leer entonces
   ya estará medio escrito.
3. **Los permisos salen de los segmentos**, no de una convención. Cada
   `PT_LOAD` trae sus bits de lectura, escritura y ejecución, y el kernel
   los traduce a las banderas de página. Eso hace que W^X dentro de un
   programa de usuario sea una propiedad del fichero y no del cargador
   —y un segmento que pida escritura **y** ejecución se rechaza, porque el
   ADR 0008 vale también aquí—.
4. **El fichero se valida entero antes de mapear nada.** La cabecera, cada
   cabecera de programa, y que cada segmento quepa en el fichero y en la
   mitad baja. Un ELF es un fichero del disco: dato de fuera. Un
   desplazamiento mal leído es una lectura fuera del fichero y un tamaño
   mal leído es un mapeo encima de otra cosa.
5. **`p_memsz` puede ser mayor que `p_filesz`**, y la diferencia se pone a
   cero: es el `.bss`. Un cargador que lo ignore deja al programa con
   basura donde esperaba ceros, que es un fallo que aparece lejos de aquí.
6. **El analizador vive en `hal` y es puro**: de los bytes de un fichero a
   una lista de segmentos con sus permisos. Se prueba en host contra ELF
   construidos a mano, incluidos los rotos, y contra el que el toolchain
   produce de verdad.
7. **El programa se lee del disco**, por su nombre, con el lector del ADR
   0025. Dejan de existir los programas incrustados salvo los que
   demuestran algo que un ELF no puede demostrar —los que se portan mal a
   propósito (ADR 0020)—, que se quedan donde están y dicen por qué.
8. **Un ELF que no se puede cargar no arranca un proceso**, y el kernel
   dice cuál de las comprobaciones falló. No hay carga parcial: un proceso
   a medio mapear es peor que ninguno.

## Alternativas consideradas

- **Seguir con binarios planos, con una cabecera propia**: se escribe en
  un día, y hay que tirarlo el día que se quiera enlazar de verdad. Es el
  mismo razonamiento que llevó a `syscall` en vez de `int 0x80` y a virtio
  moderno en vez del heredado.
- **ELF con reubicaciones (PIE)**: hace falta para cargar una biblioteca
  compartida o para aleatorizar la disposición, y las dos cosas están
  lejos. El analizador que se escribe aquí es el mismo al que se le añade
  eso después.
- **Un formato propio, más simple que ELF**: todo el código sería nuestro,
  y habría que escribir también el enlazador, o enseñarle a `rustc` a
  producirlo.
- **Leer el ELF desde la imagen del kernel** en vez del disco: evita
  depender del filesystem y pierde justo lo que este incremento demuestra,
  que es que un programa viene de un fichero.

## Consecuencias

- El workspace gana un crate de usuario. No es parte del kernel y no se
  enlaza con él: se compila aparte, acaba en el disco, y el kernel no sabe
  de él más que su nombre.
- `kernel/src/user.rs` adelgaza: los programas que demostraban el
  aislamiento siguen ahí, escritos a mano, porque un compilador no produce
  un programa que escribe en su propio código.
- El kernel pasa a leer un fichero del disco **antes** de arrancar un
  proceso, así que el camino de arranque depende del driver de disco y del
  filesystem. Si el disco falla, no hay programas de usuario — y eso tiene
  que verse en el registro, no quedarse en un cuelgue.
- `p_vaddr` decide dónde va un segmento, así que el mapa de memoria de un
  proceso deja de estar fijado por el kernel (ADR 0014: código en
  `0x400000`, pila en `0x500000`) y pasa a decirlo el fichero. La pila
  sigue siendo del kernel.
