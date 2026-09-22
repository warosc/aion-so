# Estrategia de seguridad de memoria

Qué defiende al kernel de la corrupción de memoria, dónde está aplicado y
qué falta. Se revisa al cerrar cada incremento que toque memoria.

| # | Medida | Dónde | Estado |
| --- | --- | --- | --- |
| 1 | Rust seguro por defecto; `unsafe` pequeño, localizado y con invariantes escritas | todo el árbol; `kernel` casi no tiene `unsafe` fuera de `memory` | ✅ |
| 2 | Propiedad explícita de cada frame, con transiciones válidas | `kernel::memory::frame_allocator` (libre / no libre + `manages()` derivado del mapa) | ⚠️ parcial |
| 3 | Retención conservadora ante mapas dudosos | `frame_allocator` (lo reservado gana los solapes, redondeo hacia fuera, aritmética saturada, página 0); ADR 0004 | ✅ |
| 4 | Guard pages alrededor de las pilas | — | ❌ pendiente |
| 5 | Validación centralizada de rangos | `PhysFrame`/`Page::from_start_address`, `PhysWindow::frame_ptr`, `FreeListHeap::hole_ptr`, `manages()`, direcciones canónicas | ⚠️ parcial |
| 6 | Separar físico y virtual con tipos | `hal::frame::PhysFrame`, `hal::paging::Page` | ⚠️ falta `PhysAddr`/`VirtAddr` |
| 7 | Frames a cero antes de reutilizarlos | `kernel::memory::zeroed_frames::ZeroedFrames` (todo el kernel los recibe así) | ✅ |
| 8 | Liberación comprobada | `frame_allocator::deallocate` (`NotManaged`, `NotAllocated`), `FreeListHeap::deallocate` (doble liberación, memoria ajena) | ✅ |
| 9 | Pruebas de propiedades contra un modelo | `frame_allocator` (10 000 operaciones), `FreeListHeap` (20 000), `heap::stress` (100 000 en host) | ✅ |
| 10 | Estrés en QEMU, con distintas cantidades de RAM | `cargo xtask soak-test`, `boot-test --heap-stress --memory` | ✅ |
| 11 | Concurrencia controlada y contextos documentados | `kernel::sync::IrqLock` (pánico ante reentrada); "ningún manejador de interrupción asigna memoria" | ⚠️ un solo núcleo |
| 12 | Fallos visibles, nunca éxito fingido | pánicos con dirección y operación; `check()` del heap; autopruebas de arranque | ✅ |

## Lo que falta, por orden

1. **Guard pages** (punto 4): pila del kernel y pila de double fault
   propias, mapeadas con páginas sin mapear alrededor. Hoy el kernel corre
   sobre la pila de 128 KiB del firmware y la pila de `#DF` es un estático
   dentro de la imagen: un desbordamiento corrompería lo que tenga al lado
   en vez de fallar.
2. **Pila y tablas propias** y, con ello, recuperar la memoria de boot
   services y dejar la página 0 sin mapear (hoy una desreferencia nula no
   falla).
3. **Estado por frame** (punto 2) y tipos `PhysAddr`/`VirtAddr` (punto 6).
4. **Concurrencia** (punto 11): `IrqLock` tendrá que girar en espera cuando
   haya varios núcleos, y habrá que decidir si se permite asignar desde
   manejadores de interrupción.

## Cómo se comprueba que esto funciona de verdad

- **Pruebas de mutación**: en cada incremento de memoria se introducen
  bugs deliberados, uno a uno, y se exige que las pruebas los detecten. Han
  sido 24 hasta ahora (asignador de frames, mapper, heap y candado), todos
  detectados. Quedan registrados en `docs/fase2-notes.md`.
- **Pruebas negativas de extremo a extremo**: un triple fault provocado a
  propósito, una CPU sin NX, un mapa sin controlador de teclado.
- **Soak**: 30 minutos con 68,6 millones de operaciones de heap
  verificadas, sin corrupción, pánico ni reinicio.
