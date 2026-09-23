# ADR 0015: El mapa del firmware se conserva entero y con sus atributos

## Contexto

El ADR 0013 vació la mitad baja y conservó "lo que el firmware necesita":
los tipos UEFI 5 (`RuntimeServicesCode`) y 6 (`RuntimeServicesData`). La
revisión cruzada de Codex de ese incremento señaló que eso **no es lo que
UEFI pide**:

- Lo que obliga a mantener un rango mapeado no es su tipo, es el atributo
  **`EFI_MEMORY_RUNTIME`**, que también llevan descriptores de otros tipos
  —memoria mapeada de dispositivos con la que hablan los runtime services,
  por ejemplo (UEFI 2.10, "Memory Map").
- Los **atributos de caché** del descriptor hay que respetarlos. El
  paginador solo sabía de escritura y ejecución, así que un rango de MMIO
  acabaría mapeado write-back, que corrompe lo que haya detrás.
- Un mapa **truncado** (más descriptores que capacidad) se trataba como
  completo, y con él se decidía qué desmapear.

En QEMU la diferencia es concreta: el rango `0xFFC0_0000..0x1_0000_0000`
—4 MiB de flash del firmware, uncacheable, con el bit RUNTIME— no es de
tipo 5 ni 6, así que se estaba descartando.

Cambia el contrato de arranque (`MemoryRegion` gana atributos, `MemoryMap`
gana completitud), así que lleva ADR.

## Decisión

1. **El mapa transporta los atributos**: `RegionAttributes { runtime,
   cache }`, traducidos del campo crudo del descriptor por
   `hal::memory_map::classify_attributes`, al lado del `classify_memory_type`
   que ya existía. Puro y probado en host; el cargador solo pasa los bits.
2. **La política de caché se elige por lo más permisivo que el firmware
   ofrezca**: el campo lista lo que el rango *soporta*, y varios bits
   pueden estar puestos a la vez. Write-back si está, y si no
   write-through, write-combining o uncacheable. Así la RAM queda
   write-back y el MMIO —que solo anuncia uncacheable— queda donde tiene
   que quedar.
3. **Se conserva todo descriptor con `EFI_MEMORY_RUNTIME`**, sea del tipo
   que sea: ejecutable si es `RuntimeCode`, datos en cualquier otro caso, y
   siempre con su caché.
4. **Las tablas reproducen la caché** con PWT y PCD. Write-combining exige
   reprogramar el PAT, que este kernel no hace: hasta entonces se mapea
   uncacheable —más lento, nunca incorrecto— y está dicho donde se decide.
5. **Un mapa truncado impide vaciar la mitad baja.** `MemoryMap` recuerda
   que un descriptor no cupo, y el kernel se queda con el mapa que tiene:
   el rango que se perdió podría ser justo el que el firmware necesita.

## Alternativas consideradas

- **Seguir filtrando por tipo**: es lo que había. Funciona en QEMU por
  suerte, no por diseño.
- **Mapear todo lo runtime como uncacheable** para no tener que
  transportar la caché: correcto y lento, y deja el código del firmware sin
  caché en el camino de `reboot`.
- **Ampliar la capacidad del mapa hasta que nunca se trunque**: no hay
  número que lo garantice, y el modo de fallo silencioso seguiría ahí.
- **Reprogramar el PAT** para tener write-combining de verdad: útil el día
  que el framebuffer lo pida; hoy no hay consumidor.

## Consecuencias

- En QEMU pasan de 5 a **6 rangos conservados**, 10 tablas, y uno de ellos
  es MMIO uncacheable que antes se perdía.
- El arranque registra cada rango que se conserva, con su tamaño, si es
  código o datos y su caché: si un día falta uno, se ve.
- `MemoryRegion` gana un campo; el contrato de arranque queda como lo
  dejaron los ADR 0007, 0008, 0010, 0011 y 0013, más este dato.
- Sigue faltando lo de verdad: los runtime services viven en la mitad baja,
  que va a ser espacio de usuario. Eso lo cierra `SetVirtualAddressMap` en
  el incremento siguiente, decidido así tras esta misma revisión.
