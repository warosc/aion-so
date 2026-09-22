# ADR 0004: Política de memoria física — asignador de frames y memoria de boot services retenida

## Contexto

`ROADMAP.md` pide para Fase 2 un administrador de frames y memoria virtual.
El Incremento 5 construye el primero sobre el mapa de memoria que
`BootInfo` trae desde el Incremento 2 (`docs/adr/0002-fase2-exit-boot-services.md`).

Hasta ahora ningún ADR fijaba qué memoria física puede usar el kernel. La
única regla estaba implícita en `hal::memory_map::classify_memory_type`:
tras `ExitBootServices`, `BootServicesCode`/`BootServicesData` contaban como
`Usable`, igual que la memoria convencional, "como hace el stub EFI de
Linux". Esa regla no tenía consumidor mientras nada asignaba memoria.

Antes de darle uno se midió el mapa real de OVMF (QEMU `-m 256M`), con
instrumentación temporal que no se commiteó:

- la **pila** en la que corre el kernel (RSP `0xfe86f70` al entrar en
  `kmain`) está en una región `BootServicesData` de 128 KiB;
- **todas las tablas de páginas activas** (recorrido completo desde CR3
  `0xf801000`: 1 034 páginas) están en `BootServicesData`;
- la **página física 0** es `Conventional`;
- el búfer del mapa de memoria es `LOADER_DATA`;
- hay 104 descriptores, sin solapes ni desalineaciones: 52 905 páginas
  `Conventional` (206,7 MiB) y 10 998 de boot services (≈43 MiB).

Linux solo reutiliza la memoria de boot services **después** de pasar a su
propia pila y a sus propias tablas de páginas. Este kernel todavía corre
sobre las del firmware, así que la regla implícita permitía que el primer
consumidor que escribiera en un frame asignado (las tablas del Incremento 6
o el heap del Incremento 7) pisara la pila o una tabla de páginas viva.

`CLAUDE.md` exige ADR para cambios de layout de memoria. Esta decisión fija
qué memoria física pertenece al kernel y cambia la semántica de lo que
`boot` entrega en `BootInfo`, así que se registra aquí.

## Decisión

1. **Contrato del mapa en `BootInfo`.** `MemoryRegionKind` distingue tres
   clases: `Usable` (UEFI `Conventional`), `BootServices`
   (`BootServicesCode`/`BootServicesData`) y `Reserved` (todo lo demás).
   `classify_memory_type` deja de recibir `post_exit`: el clasificador
   informa de lo que dice el firmware, y la política la decide el kernel.
2. **Qué se entrega.** El asignador de frames solo entrega frames de
   regiones `Usable`. La memoria `BootServices` queda **retenida** mientras
   el kernel dependa de algo que el firmware dejó ahí: como mínimo, su
   pila y su jerarquía de tablas de páginas. Recuperarla exige que el
   kernel ya corra sobre pila y CR3 propios (previsto para Fase 3). Será
   una decisión explícita, registrada como actualización de este ADR, y no
   un cambio del clasificador.
3. **Reglas conservadoras**, todas inclinadas a retener: las regiones
   `Usable` se redondean hacia dentro a frames enteros; cualquier frame que
   toque una región no usable se retiene, redondeado hacia fuera, y la
   retención gana cualquier solape; la página 0 no se entrega nunca; la
   aritmética sobre el mapa es saturada, porque el mapa es un dato del
   firmware. Liberar un frame que el asignador no podía entregar es un
   error (`NotManaged`), nunca una forma de volver disponible memoria
   retenida.
4. **Asignador.** Un bitmap, con un bit por frame de 4 KiB desde la
   dirección física 0. Cubre 256 MiB (8 KiB de bitmap), el `-m 256M` de
   `cargo xtask`; la RAM usable por encima se ignora y se registra en el
   log. El bitmap vive en la pila de `kmain`, que nunca retorna y que está
   en memoria de boot services retenida, así que el bitmap nunca puede
   entregarse como frame.
5. **Contenido.** Los frames se entregan **sin poner a cero**: el asignador
   nunca lee ni escribe su contenido. Quien los use es responsable de
   inicializarlos.

## Alternativas consideradas

- **Mantener `BootServices` como usable y excluir solo lo que se sabe
  vivo** (la pila, las páginas alcanzables desde CR3): rechazada. Exige
  enumerar cada estructura del firmware que el kernel siga usando, y
  olvidar una es corrupción silenciosa. Retener la clase entera no tiene
  ese modo de fallo.
- **Clasificar `BootServices` como `Reserved`** (`post_exit = false`):
  rechazada. Se comporta igual hoy, pero borra la información que Fase 3
  necesitará para recuperar esa memoria.
- **Pasar ya a pila y tablas propias y recuperar la memoria ahora**:
  rechazada para este incremento. Es el alcance del Incremento 6 y de Fase
  3, y juntarlo aquí aumentaría el radio de impacto del incremento, justo
  lo que el proyecto evita con incrementos pequeños.
- **Asignador bump o buddy/slab**: el bump no puede liberar; buddy y slab
  no tienen todavía ningún consumidor de asignaciones multi-frame o
  sub-frame (ver el plan de Fase 2).
- **Bitmap en un `static` o colocado en una región usable**: exigiría
  `unsafe` o mapeo previo, y aún no hay ningún consumidor que necesite un
  asignador global `'static`. Se revisará cuando lo pidan Fase 3 (syscalls,
  fallos de página) o Fase 5 (dimensionar desde el mapa real).

## Consecuencias

- En QEMU quedan disponibles 52 901 frames (206 MiB). ≈43 MiB (≈17 % de la
  RAM) quedan retenidos hasta Fase 3.
- Cualquier consumidor del mapa en `BootInfo` debe tratar `BootServices`
  como no asignable.
- El Incremento 6 tiene que tomar las tablas nuevas de este asignador y
  ponerlas a cero, sin liberar nunca las tablas del firmware.
- En hardware real (Fase 5) la cobertura fija de 256 MiB debe pasar a
  dimensionarse desde el mapa.
- `kernel::memory::self_test` comprueba en cada arranque, sobre el mapa
  real, que el frame de la pila viva no es asignable. Si alguien vuelve a
  tratar `BootServices` como usable, el arranque se detiene con un pánico
  registrado en el log, en lugar de corromper memoria.
- Detalle de la medición y de la verificación: `docs/fase2-notes.md`,
  Incremento 5.
