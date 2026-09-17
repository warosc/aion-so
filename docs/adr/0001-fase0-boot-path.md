# ADR 0001: Boot path de Fase 0 — app UEFI propia con `uefi-rs`

## Contexto

Fase 0 (`ROADMAP.md`) exige que "un comando construya y arranque la
imagen" en QEMU. Había tres enfoques razonables para el arranque UEFI:

1. App UEFI propia con el crate `uefi` (`uefi-rs`), target
   `x86_64-unknown-uefi`.
2. El crate `bootloader` (rust-osdev), que resuelve UEFI/BIOS y entrega un
   `BootInfo` ya construido (framebuffer, mapa de memoria).
3. Un bootloader externo no-Rust (Limine o GRUB con Multiboot2), con el
   kernel como ELF freestanding separado.

## Decisión

Se eligió la opción 1: una aplicación UEFI propia escrita con `uefi-rs`.
El kernel de Fase 0 (`aion-kernel`) se enlaza como librería Rust dentro del
binario UEFI (`aion-boot`). Esta versión **no** llama a `ExitBootServices`,
**no** carga una imagen de kernel separada y **no** toca el Graphics
Output Protocol (GOP): solo inicializa el logger de `uefi-rs` y llama a
`aion_kernel::kmain()`, que reporta el marcador de arranque y detiene el
núcleo con `hlt` en un bucle infinito.

## Alternativas consideradas

- **Crate `bootloader`**: menos código propio y un `BootInfo` gratis, pero
  el ABI de arranque (forma de `BootInfo`, contrato del punto de entrada)
  lo controla el mantenedor de esa crate, no AION — riesgo de choque con
  la regla de `ARCHITECTURE.md` de que ningún cambio de boot path ocurra
  sin ADR: un cambio *upstream* forzaría la mano de AION sin que fuera una
  decisión propia.
- **Limine / GRUB**: protocolo de arranque maduro, pero requiere un
  toolchain no-Rust que `SETUP.md` no menciona, y binarios externos que
  chocan con la regla de `WORKFLOW.md` de no versionar binarios pesados.

## Consecuencias

- Todo el boot path queda dentro del toolchain Rust/Cargo ya exigido por
  `SETUP.md`, sin dependencias externas que versionar.
- El código dependiente de arquitectura permanece confinado a `arch/x86_64/`
  y `hal/`, según `ARCHITECTURE.md`.
- Cuando Fase 1 reemplace este arranque por un loader real (imagen de
  kernel separada, `BootInfo` real con mapa de memoria y framebuffer,
  `ExitBootServices`), eso constituye una "sustitución del boot path" y
  requiere un ADR formal, tal como exige `ARCHITECTURE.md`.
