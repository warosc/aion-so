# ADR 0005: El kernel toma la raíz de las tablas de páginas; espacio del kernel en la mitad alta

## Contexto

`ROADMAP.md` pide memoria virtual en Fase 2. El Incremento 6 da al kernel
una API para mapear, desmapear y traducir páginas, que el heap del
Incremento 7 usará para reservar su memoria. El plan de Fase 2 preveía
hacerlo **sobre la tabla de páginas activa heredada de UEFI**, sin cambiar
CR3 (lo dejaba para el aislamiento de Fase 3).

Antes de escribir código se midió el estado real de la paginación en
OVMF/QEMU, con instrumentación temporal que no se commiteó:

- **CR0.WP = 1**, así que el kernel (ring 0) tampoco puede escribir en
  páginas mapeadas como de solo lectura.
- **Las tablas del firmware están mapeadas en solo lectura**: la PML4
  (`0xf801000`), las PDPT y las PD están en páginas de 2 MiB sin el bit de
  escritura. Es la autoprotección de tablas de EDK2. Escribir una entrada
  nueva en la PML4 del firmware habría provocado un #PF en la primera
  escritura: el plan original no era ejecutable.
- **Mapa de identidad de 0 a 1 TiB** (ranuras 0 y 1 de la PML4), en páginas
  de 2 MiB (524 281), más 7 PT de 4 KiB con permisos finos (código en solo
  lectura, datos con NX). A partir de 1 TiB no hay nada mapeado, y la mitad
  alta está vacía.
- Paginación de 4 niveles (CR4.LA57 = 0), sin PCID, con **EFER.NXE = 1**.
- La página 0 está mapeada con escritura permitida: una desreferencia nula
  no falla.

Esto fija el layout de memoria virtual y quién es dueño de CR3, así que
necesita ADR según `CLAUDE.md`.

## Decisión

1. **El kernel es dueño de la tabla raíz.** `KernelPageTable::take_over`
   copia las 512 entradas de la PML4 del firmware en un frame propio (de
   memoria `Usable`, ver ADR 0004) y lo carga en CR3, conservando los bits
   PWT/PCD. Todas las traducciones quedan idénticas. Las tablas de niveles
   inferiores del firmware se **comparten en solo lectura** y el kernel
   **nunca escribe ninguna tabla del firmware**.
2. **Layout virtual de Fase 2:**

   | Rango | Ranuras PML4 | Contenido |
   | --- | --- | --- |
   | `0x0000_0000_0000_0000`-`0x0000_00FF_FFFF_FFFF` | 0-1 | Mapa de identidad del firmware (1 TiB), tablas del firmware |
   | `0x0000_0100_0000_0000`-`0x0000_7FFF_FFFF_FFFF` | 2-255 | Vacío; reservado para el espacio de usuario (Fase 3) |
   | `0xFFFF_8000_0000_0000`-`0xFFFF_FFFF_FFFF_FFFF` | 256-511 | **Espacio del kernel**: todas sus tablas son del kernel |

   El mapper solo mapea y desmapea en el espacio del kernel. Fuera de él
   devuelve `OutsideKernelSpace` sin escribir nada. `take_over` se niega
   (`KernelSpaceInUse`) si el firmware ya ocupa esa mitad.
3. **Requisitos, comprobados en tiempo de ejecución** antes de escribir
   nada: paginación de 4 niveles, sin PCID, EFER.NXE activo, y mapa de
   identidad verificado para la raíz del firmware y para la pila. Cada
   frame nuevo que vaya a ser tabla debe traducirse a sí mismo y ser
   escribible, o se rechaza (`TableFrameNotWritable`).
4. **Tablas nuevas**: se ponen a cero por completo **antes** de enlazarlas
   (el recorrido de la CPU nunca ve una tabla a medio construir). Las
   entradas intermedias son `PRESENT | WRITABLE`, y los permisos los decide
   la hoja: sin bit de usuario nunca, y con NX salvo que se pida
   `executable`.
5. **Límites deliberados**: no se parten páginas grandes
   (`HugePageInTheWay`); `unmap` no libera las tablas intermedias vacías; y
   tras cada escritura de una hoja se ejecuta `invlpg`.
6. **Si `take_over` se niega**, no es fatal: el kernel sigue en las tablas
   del firmware, sin mapper, y lo registra en el log.

## Alternativas consideradas

- **Escribir en las tablas del firmware** (el plan original): imposible sin
  más, porque son de solo lectura con CR0.WP = 1 (medido).
- **Desactivar CR0.WP mientras se escriben tablas**: rechazada. Apaga una
  protección global para todo el kernel durante esa ventana, la convierte
  en estado que hay que restaurar en todas las rutas (incluidas las de
  error y las interrupciones) y sigue escribiendo memoria del firmware que
  el ADR 0004 considera en uso.
- **Reconstruir desde cero todas las tablas** (identidad incluida) y
  abandonar las del firmware: rechazada por ahora. Es el paso que
  permitiría recuperar la memoria de boot services, pero es mucho más
  grande: copiar o regenerar 1 TiB de mapa con los permisos finos del
  firmware. Encaja en Fase 3, junto al primer espacio de direcciones por
  proceso.
- **Espacio del kernel en la mitad baja** (por ejemplo, la ranura 2):
  rechazada. Chocaría con el espacio de usuario de Fase 3, que ocupará la
  mitad baja de cada proceso mientras el kernel se comparte en la alta.

## Consecuencias

- Desviación del plan de Fase 2: CR3 cambia ya en Fase 2, aunque solo
  cambia la raíz y todas las traducciones quedan idénticas. El aislamiento
  entre procesos sigue siendo de Fase 3.
- El ADR 0004 **sigue vigente**: la raíz es del kernel, pero las tablas
  inferiores del mapa de identidad (y la pila) siguen viviendo en memoria
  de boot services.
- Los *runtime services* de UEFI (`reboot`, `shutdown`) se ejecutan con la
  raíz del kernel. Funcionan porque el mapa de identidad se conserva
  (verificado en QEMU).
- El heap del Incremento 7 vivirá en el espacio del kernel.
- Queda anotado como riesgo que la página 0 está mapeada con escritura:
  desmapearla exige escribir una tabla del firmware y encaja cuando el
  kernel reconstruya el mapa de identidad (Fase 3).
- Detalle de la medición y de la verificación: `docs/fase2-notes.md`,
  Incremento 6.
