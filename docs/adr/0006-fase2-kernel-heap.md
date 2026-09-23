# ADR 0006: Heap del kernel — ubicación, algoritmo y candado

## Contexto

`ROADMAP.md` pide para Fase 2 un heap del kernel con pruebas. Es el primer
uso de `alloc` en el proyecto y el primer `#[global_allocator]`. Se apoya en
el asignador de frames (ADR 0004) y en el mapper del espacio del kernel
(ADR 0005), que ya dejaron fijadas dos restricciones:

- solo se entregan frames de memoria `Usable`, y llegan sin poner a cero;
- solo se puede mapear en el espacio del kernel (mitad alta, ranuras
  256-511 de la PML4), cuyas tablas son todas del kernel.

Decidir dónde vive el heap, cuánto mide y cómo se protege es layout de
memoria, así que lleva ADR según `CLAUDE.md`.

## Decisión

1. **Ubicación**: `0xFFFF_8080_0000_0000`
   (`harlan_arch_x86_64::paging::KERNEL_HEAP_START`), la ranura 257 de la
   PML4, **una ranura propia**. La 256 queda para mapeos puntuales como la
   página de la autoprueba del mapper. Cuando en Fase 3 cada proceso tenga
   su PML4, compartir el heap consistirá en copiar una entrada.
2. **Tamaño**: 4 MiB fijos. Se mapean al arrancar, página a página, con
   frames nuevos y permisos `writable`, no `executable` (NX). No se
   desmapean ni crecen. Si faltan frames o tablas a mitad de camino, el
   heap se queda con las páginas ya mapeadas y lo registra en el log.
3. **Algoritmo**: lista libre intrusiva ordenada por dirección, con primer
   ajuste, división al asignar y fusión al liberar (`FreeListHeap`).
   - La granularidad es de 16 bytes, igual al tamaño de la cabecera de un
     hueco. Así, cualquier sobrante delante o detrás de una asignación es un
     hueco válido y ningún byte queda sin contabilizar.
   - Los bloques no llevan cabecera: su tamaño se recalcula del `Layout`
     que exige el contrato de `GlobalAlloc`.
4. **Defensas**:
   - Cada acceso a una cabecera comprueba que cae dentro del heap: una
     cabecera corrupta provoca un pánico en vez de una escritura en otra
     parte.
   - Liberar memoria ajena, mal alineada o que se solapa con un hueco (una
     doble liberación) provoca un pánico.
   - `check()` verifica todos los invariantes de la lista.
5. **Candado**: `IrqLock`, el primer consumidor de `hal::InterruptControl`.
   Mientras está tomado, las interrupciones siguen deshabilitadas; al
   soltarlo se restaura el estado anterior, así que las secciones anidadas
   no las reactivan antes de tiempo. En un solo núcleo, encontrar el candado
   tomado solo puede ser reentrada (un manejador de interrupción que asigna,
   o una asignación dentro del propio asignador). Eso **provoca un pánico**
   en lugar de girar en espera: con las interrupciones apagadas, girar
   colgaría la máquina sin dejar rastro. Con SMP pasará a girar.
6. **Agotamiento**: `alloc` devuelve nulo y el `handle_alloc_error` por
   defecto de `no_std` provoca un pánico, que el manejador de pánico
   registra. Sin heap (si `take_over` o el mapeo fallaron), la primera
   asignación entra en ese mismo pánico. Hoy nada asigna fuera de la
   inicialización y la autoprueba del heap.
7. **Global allocator**: el `static HEAP` es el `#[global_allocator]`
   excepto en las pruebas de host, que conservan el asignador del sistema.
   Las pruebas ejercitan el mismo `FreeListHeap` y el mismo `IrqLock` sobre
   búferes propios.

## Alternativas consideradas

- **Bump allocator**: no puede liberar, así que no aguanta una carga
  sostenida.
- **Bitmap de 16 bytes con metadatos fuera de banda**: tiene la ventaja de
  que un desbordamiento de un bloque no puede corromper la contabilidad.
  Rechazado por ahora, porque cada asignación tendría que buscar tramos
  libres en todo el heap. Se reconsiderará si alguna vez un desbordamiento
  corrompe cabeceras. Las comprobaciones de límites y `check()` ya
  convierten esa corrupción en un pánico.
- **Buddy o slab**: no hay todavía una carga que los justifique.
- **Crate `linked_list_allocator`**: rechazado para mantener el heap
  auditable en el árbol e integrado con `IrqLock`, la detección de doble
  liberación y `check()`.
- **Candado de espera activa** (spinlock) sin tocar las interrupciones: un
  manejador que asignara con el candado tomado dejaría la máquina en
  interbloqueo.

## Consecuencias

- `alloc` (`Vec`, `Box`, `String`…) está disponible en el kernel una vez
  inicializado el heap. La autoprueba de arranque comprueba que su memoria
  sale del rango del heap.
- En QEMU el heap consume 1 028 frames (1 024 páginas y 4 tablas).
- La prueba de estrés es la misma en el host y en QEMU. En cada arranque
  corren 2 000 ciclos; con `cargo xtask boot-test --heap-stress` (también
  en la CI) corren 200 000. Cada bloque vivo se rellena con su propio byte
  y se verifica antes de liberarlo, de modo que cualquier solape entre
  bloques aparece. Al final se exige volver a los bytes libres iniciales.
- El heap no es reentrante desde interrupciones: ningún manejador debe
  asignar memoria.
- Detalle de la verificación: `docs/fase2-notes.md`, Incremento 7.
