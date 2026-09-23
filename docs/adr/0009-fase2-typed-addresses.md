# ADR 0009: Las direcciones físicas y virtuales llevan tipos distintos

## Contexto

Hasta el Incremento 13 toda dirección viajaba como `u64`. Los tipos que
envuelven memoria —`PhysFrame`, `PhysRange`, `Page`— se construían desde
`u64` y devolvían `u64`, y `PageMapper::translate` tenía la firma
`u64 -> Option<u64>`: entraba una dirección virtual y salía una física, sin
nada que las distinguiera.

Es el error clásico de un kernel: escribir a través de una dirección física
creyendo que está mapeada, o pasar una virtual donde se espera un marco. No
falla al compilar ni al arrancar; falla más tarde, escribiendo en algún sitio
que no es, o no falla nunca en QEMU y sí en hardware real. Las dos son
números de 64 bits, así que solo un tipo puede separarlas.

Este cambio recorre la frontera de la HAL —cambia firmas que `boot`, `kernel`
y `arch` comparten—, así que lleva ADR, aunque no toca ABI, protocolo de
arranque ni layout de memoria.

## Decisión

1. **`hal::addr` define `PhysAddr` y `VirtAddr`**: dos envoltorios de `u64`
   sin conversión entre sí. Ofrecen `new`, `as_u64`, `is_aligned_to`,
   `align_down`, `checked_add`, `saturating_sub`, `+ u64` y los formateos
   `Debug`/`Display`/`LowerHex`, para que los registros impriman lo mismo que
   antes.
2. **Los tipos de memoria se apoyan en ellos**: `PhysFrame` y `PhysRange`
   sobre `PhysAddr`; `Page` sobre `VirtAddr`; `PageMapper::translate` pasa a
   `VirtAddr -> Option<PhysAddr>`; y quedan tipados
   `MemoryRegion::start_phys_addr`, `FramebufferInfo::base_addr`,
   `Stack::{bottom, top}` y `KERNEL_{SPACE,HEAP,STACKS}_START`.
3. **`VirtAddr::as_ptr::<T>()` es la única conversión de entero a puntero**
   del árbol. Crear el puntero es seguro; desreferenciarlo sigue siendo
   `unsafe` y vive en las pocas funciones que saben que la página está
   mapeada.
4. **Cruzar de físico a virtual es explícito y comentado.** Solo ocurre
   donde el mapa de identidad del firmware lo justifica: `PhysWindow`
   (`base + dirección física`), la lectura de comprobación del mapper y el
   framebuffer que reporta el GOP.
5. **El interior del walker de `arch::paging` sigue en `u64`.** Ahí las
   direcciones se enmascaran y desplazan bit a bit contra el formato de las
   entradas; los tipos entran y salen en sus bordes. Por eso
   `KERNEL_SPACE_BASE` (crudo, privado) convive con `KERNEL_SPACE_START`
   (tipado, público).

## Alternativas consideradas

- **Seguir con `u64` y confiar en los nombres**: es lo que había. No costaba
  nada y no detecta nada.
- **Tipar también el interior del walker**: cada paso intermedio de la
  búsqueda de tablas es un enmascarado de bits, no una dirección con
  significado; envolverlos solo añadiría `as_u64()` sin ganar comprobación.
- **Usar `x86_64::{PhysAddr, VirtAddr}` del crate homónimo**: traería una
  dependencia con su propio modelo (validación de canonicidad, panics) a la
  HAL, que debe ser agnóstica de arquitectura.
- **Validar en el constructor** (canonicidad, límites): `new` no valida a
  propósito. La validación ya está donde se puede decidir de verdad
  (`from_start_address`, `manages`, `translate`, `is_canonical`), y un
  constructor que entra en pánico haría inservibles los datos del firmware.

## Consecuencias

- Confundir físico y virtual ya no compila. Comprobado: pasar
  `frame.start_address()` donde se espera una `VirtAddr` da
  `no method named as_ptr found for struct PhysAddr`.
- No cambia comportamiento: el registro de arranque es idéntico al del
  Incremento 13, con las mismas direcciones y el mismo desglose de marcos.
  10/10 arranques, soak de 120 s en verde, `shutdown` y `reboot` intactos.
- El código gana llamadas a `as_u64()` en los bordes (ABI de asm, TSS,
  aritmética del walker). Están contadas y comentadas; son el sitio donde
  habría que mirar si alguna vez reaparece una confusión.
- Fase 3 hereda la separación: cuando la mitad baja pase a ser espacio de
  usuario, la ventana física (`PhysWindow`) cambiará de base y el compilador
  señalará todo lo que dependa de que hoy coincidan.
