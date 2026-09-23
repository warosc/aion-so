# ADR 0011: W^X dentro de la imagen del kernel

## Contexto

El ADR 0008 dejó la mitad baja no ejecutable salvo los rangos que contienen
código, y anotó lo que quedaba abierto: **la imagen del kernel entera seguía
siendo escribible y ejecutable**, porque solo se conocía su rango completo
(`LoadedImage`), no lo que hay dentro. Sus datos —`.rdata`, `.data`, el
`.reloc`— se podían ejecutar, y su código se podía sobrescribir.

Saber qué parte es código exige leer la tabla de secciones del PE que el
firmware ya ha cargado y reubicado. Es el último punto abierto del programa
de endurecimiento (`docs/memory-safety.md`).

Añade un campo a `BootInfo` y cambia permisos del mapa de memoria, así que
lleva ADR.

## Decisión

1. **`hal::pe` lee la tabla de secciones**: código puro, sin `alloc`, sin
   arquitectura y sin firmware, que dado el contenido de una imagen PE32+
   devuelve las secciones marcadas `IMAGE_SCN_MEM_EXECUTE`. No se fía de la
   imagen: comprueba cada desplazamiento contra el tamaño antes de leerlo, de
   modo que una cabecera corrupta es un error, nunca una lectura fuera de
   rango. Se prueba en host con imágenes sintéticas.
2. **`boot` la usa sobre sí mismo** y pasa el resultado en
   `BootInfo::kernel_code`. Si las cabeceras no se pueden leer, el campo es
   `None`, la imagen entera queda ejecutable y escribible —como antes de este
   ADR— y se registra por qué.
3. **Solo el código queda ejecutable**: el resto de la imagen pasa a
   no ejecutable, como cualquier otro dato del mapa.
4. **Una página que solo contiene código se mapea de solo lectura.** Esa es
   la otra mitad de W^X: si se ejecuta, no se escribe. Una página que mezcla
   código y datos —posible si alguna sección no está alineada a 4 KiB— sigue
   siendo escribible, porque hay datos dentro.
5. **Cada rango ejecutable dice si hay que dejarlo escribible**
   (`hal::paging::ExecutableRange`). El código del kernel, no. El de los
   runtime services de UEFI, **sí**: OVMF escribe dentro de su propio código,
   y con ese rango en solo lectura `shutdown` falla con
   `#PF accessing 0xf6e6104, error_code=0x3, rip=0xf6e5388` (medido, no
   supuesto).

## Alternativas consideradas

- **Leer las secciones desde el kernel** en vez del cargador: el kernel
  tendría que conocer el formato PE de su propia imagen, cuando `boot` ya es
  quien habla con el firmware y ya captura el rango. La parte reutilizable
  —el análisis— vive en `hal` de todos modos.
- **Marcar de solo lectura también el código del firmware**: probado, falla
  (ver arriba). Es el mismo tipo de hallazgo que el ADR 0008 registró al
  intentar marcarlo no ejecutable.
- **Marcar de solo lectura la imagen entera salvo `.data`**: exigiría además
  distinguir las secciones escribibles, y `.data` ya queda escribible por ser
  lo que no es código. La regla "ejecutable ⇒ no escribible" es más simple y
  cubre lo que importa.
- **Confiar en que nadie escriba en `.text`**: es lo que había. No detecta
  nada, y un error de puntero que aterrice ahí corrompe código en silencio.

## Consecuencias

- En QEMU: de 320 KiB de imagen, 228 KiB son código (una sección `.text`).
  El mapa reconstruido tiene 314 páginas ejecutables, **57 de ellas de solo
  lectura** (el código del kernel); las ~23 páginas de datos de la imagen
  pasan a no ejecutables, y el código del firmware sigue ejecutable y
  escribible por necesidad.
- Verificado por la vía negativa: escribir en el código del propio kernel
  produce `#PF accessing <dirección de kernel_main>, error_code=0x3`
  (página presente, fallo de escritura). Sin este cambio la escritura se
  completaba en silencio.
- `BootInfo` gana un campo (`kernel_code`); el contrato de arranque queda
  como lo dejaron los ADR 0007, 0008 y 0010, más este dato.
- Un kernel que algún día quiera parchear su propio código —trampolines,
  parcheo en caliente— tendrá que mapear esa página aparte y a propósito, en
  vez de escribir sin más. Es justamente lo que se quería.
- Queda fuera: las secciones de solo lectura (`.rdata`) siguen siendo
  escribibles; separarlas exigiría leer también los permisos de cada sección
  y no cambia la propiedad W^X.
