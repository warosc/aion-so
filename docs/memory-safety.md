# Estrategia de seguridad de memoria

Qué defiende al kernel de la corrupción de memoria, dónde está aplicado y
qué falta. Se revisa al cerrar cada incremento que toque memoria.

| # | Medida | Dónde | Estado |
| --- | --- | --- | --- |
| 1 | Rust seguro por defecto; `unsafe` pequeño, localizado y con invariantes escritas | todo el árbol; `kernel` casi no tiene `unsafe` fuera de `memory`; el análisis del PE (`hal::pe`) es seguro entero | ✅ |
| 2 | Propiedad explícita de cada frame, con transiciones válidas | `frame_allocator`: libre / retenido / en uso con `FramePurpose` (boot, tablas, heap, pilas, kernel); `deallocate_as` falla si no coincide | ✅ |
| 3 | Retención conservadora ante mapas dudosos | `frame_allocator` (lo reservado gana los solapes, redondeo hacia fuera, aritmética saturada, página 0); ADR 0004 y 0007 | ✅ |
| 4 | Guard pages alrededor de las pilas | `kernel::memory::stacks`, `arch::stack::switch_to`, IST1 del TSS | ✅ |
| 5 | Validación centralizada de rangos | `PhysFrame`/`Page::from_start_address`, `PhysWindow::frame_ptr`, `FreeListHeap::hole_ptr`, `manages()`, direcciones canónicas | ⚠️ parcial |
| 6 | Separar físico y virtual con tipos | `hal::addr::PhysAddr`/`VirtAddr`, y sobre ellos `PhysFrame`, `PhysRange`, `Page`, `PageMapper::translate`, `Stack`, `KERNEL_*_START` | ✅ |
| 7 | Frames a cero antes de reutilizarlos | `kernel::memory::zeroed_frames::ZeroedFrames` (todo el kernel los recibe así); el arranque comprueba contra las tablas vivas que la ventana física alcanza cada marco antes de escribir uno | ✅ |
| 8 | Liberación comprobada | `frame_allocator::deallocate_as` (`NotManaged`, `NotAllocated`, `WrongPurpose`; el kernel no tiene otra forma de devolver un marco), `FreeListHeap::deallocate` (doble liberación, memoria ajena) | ✅ |
| 9 | Pruebas de propiedades contra un modelo | `frame_allocator` (10 000 operaciones), `FreeListHeap` (20 000), `heap::stress` (100 000 en host) | ✅ |
| 10 | Estrés en QEMU, con distintas cantidades de RAM | `cargo xtask soak-test`, `boot-test --heap-stress --memory` | ✅ |
| 11 | Concurrencia controlada y contextos documentados | `kernel::sync::IrqLock` (pánico ante reentrada) y la tabla de contextos de más abajo | ⚠️ un solo núcleo |
| 12 | Fallos visibles, nunca éxito fingido | pánicos con dirección y operación; `check()` del heap; autopruebas de arranque | ✅ |

El kernel ya no depende de la memoria del firmware: tiene tablas, pila,
mapa de memoria, bitmap y consola propios, y la página 0 está sin mapear
(ADR 0007).

## Contextos de ejecución y concurrencia

El kernel corre en un solo núcleo. Hay dos contextos:

- **Contexto de kernel**: `kmain` y, tras el cambio de pila,
  `kernel_main_on_stack` y el shell. Puede asignar, mapear y liberar.
- **Contexto de interrupción**: `rust_interrupt_handler` y lo que llama.
  Entra por una puerta de interrupción, así que `IF` está a 0 y no es
  reentrante (salvo NMI, que solo registra un aviso y vuelve).

La regla que sostiene todo lo demás: **ningún manejador de interrupción
asigna memoria ni toca las tablas de páginas**. Hoy se cumple por
construcción —el temporizador incrementa un `AtomicU64` y el teclado escribe
en un anillo de atómicos; ninguno de los dos llama al heap, al asignador de
marcos ni al mapper—, y debe seguir cumpliéndose:

| Componente | Quién puede llamarlo |
| --- | --- |
| `BitmapFrameAllocator`, `ZeroedFrames`, `PageTables`, `stacks` | solo contexto de kernel |
| `KernelHeap` (`alloc`, `Box`, `Vec`, formateo que asigne) | solo contexto de kernel |
| `TICK_COUNT`, anillo del teclado (`AtomicU8`/`AtomicUsize`) | ambos |
| `log::` sobre debugcon y framebuffer | ambos; no asigna |

`IrqLock` protege el heap: tomarlo desactiva las interrupciones, de modo que
en un solo núcleo no hay contención posible salvo por reentrada, y la
reentrada **entra en pánico en vez de girar** (un giro con las interrupciones
desactivadas colgaría la máquina en silencio). El candado evita la
corrupción si una interrupción alcanza el heap mientras ya está bloqueado,
pero **no impone por sí solo** la regla "ningún manejador asigna": una
asignación desde un manejador con el heap libre se completaría. Esa
prohibición se sostiene hoy revisando los manejadores, y hay que conservarla
a mano.

Con varios núcleos (Fase 3 o más adelante) esto cambia: `IrqLock` tendrá que
girar además de desactivar interrupciones, el asignador de marcos necesitará
su propio candado y habrá que decidir explícitamente si se permite asignar
desde un manejador.

## Lo que falta, por orden

1. **Varios núcleos** (punto 11): mientras haya uno solo, `IrqLock` basta;
   ver la sección anterior para lo que habrá que cambiar.
2. **Validación de rangos** (punto 5): la validación existe en cada frontera
   (`from_start_address`, `PhysWindow::frame_ptr`, `hole_ptr`, `manages`),
   pero no en un único sitio; queda como estaba hasta que haya user space y
   un punto natural donde centralizarla.
3. **Permisos por sección dentro de la imagen**: desde el ADR 0011 el código
   del kernel es de solo lectura y el resto de la imagen no es ejecutable,
   pero `.rdata` sigue siendo escribible. Separarlo exige leer los permisos
   de cada sección y no cambia la propiedad W^X.

El código de los runtime services del firmware queda fuera de todo esto: es
ejecutable y escribible porque OVMF escribe dentro de él (ADR 0011). No se
puede arreglar desde aquí.

## Cómo se comprueba que esto funciona de verdad

- **Pruebas de mutación**: en cada incremento de memoria se introducen
  bugs deliberados, uno a uno, y se exige que las pruebas los detecten. Han
  sido 69 hasta ahora (asignador de frames, mapper, heap, candado, marcos a
  cero, guard pages, mapa de identidad, recuperación de memoria, permisos de
  ejecución, propósito por marco, tipos de dirección, los arreglos de la
  revisión de Codex y W^X dentro de la imagen), todas detectadas. Dos de ellas destaparon huecos
  reales de pruebas —uno en `reclaim_boot_services`, otro en el soak— y una
  tercera destapó una condición redundante; se arreglaron antes de cerrar su
  incremento. Desde el Incremento 14 hay además
  una comprobación que no necesita pruebas: confundir una dirección física
  con una virtual ya no compila. Quedan registradas en
  `docs/fase2-notes.md`.
- **Pruebas negativas de extremo a extremo**: ejecutar desde un marco de
  datos (`#PF ... error_code=0x11`), una desreferencia de puntero nulo
  (`#PF accessing 0x0`), escribir en el código del propio kernel
  (`#PF ... error_code=0x3`), un desbordamiento de pila (que la guard page
  convierte en un double fault legible), un triple fault, una CPU sin NX,
  un mapa sin controlador de teclado.
- **Soak**: 30 minutos con 68,6 millones de operaciones de heap
  verificadas, sin corrupción, pánico ni reinicio. Desde la revisión de Codex
  también falla si el progreso se detiene en los últimos diez segundos, no
  solo si el total se queda corto.
- **Revisión cruzada**: cada incremento lo revisa el otro agente. La revisión
  de la pila 8-14 encontró ocho defectos que las pruebas no veían, incluido un
  UB de escritura desalineada en el TSS (ver `docs/fase2-notes.md`).
