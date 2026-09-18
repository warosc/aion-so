# Notas de implementación — Fase 2

## Incremento 3 — Remapeo de PIC + temporizador PIT

### No se necesitó ADR

No cambia el boot path ni `BootInfo`: es configuración de hardware (PIC,
PIT) y una entrada nueva en el IDT.

### Qué hace

- `arch/x86_64/src/pic.rs`: `remap()` ejecuta la secuencia ICW1-4
  (IRQ0-7 → vectores 0x20-0x27, IRQ8-15 → 0x28-0x2F, fuera del rango de
  excepciones 0-31) y deja desenmascarado únicamente IRQ0; `send_eoi()`.
- `arch/x86_64/src/pit.rs`: canal 0, modo 3, **100 Hz** (divisor 11931).
- `arch/x86_64/src/interrupts.rs`: vector `0x20` con manejador real
  (`timer_stub` → `common_trampoline`) que incrementa un `AtomicU64`
  (`Relaxed`, único escritor), envía EOI y registra `AION: ticks=N` cada
  100 ticks. `init_timer()` instala el vector, remapea el PIC, programa el
  PIT y por fin ejecuta `sti`.
- `hal::TickCounter` (solo lectura); `Cpu` lo implementa.

Se eligió PIC/PIT y no APIC porque APIC requiere descubrimiento de
hardware (ACPI/MADT o MSR+MMIO), territorio de Fase 4/5.

### Hallazgo retroactivo: los Incrementos 1 y 2 dejaban la shell colgada

Este incremento cierra un problema real que las pruebas automatizadas de
los dos anteriores no podían ver. `interrupts::init()` (Incremento 1)
ejecuta `cli` y nunca vuelve a ejecutar `sti`; el Incremento 2 además
enmascara ambos PIC. Con IF=0 y todo enmascarado, el `hlt` de
`idle_once()` (que la shell ejecuta en cuanto `read_key()` devuelve `None`,
o sea siempre) solo despierta con una NMI o un reset: la shell imprimía su
prompt y quedaba parada para siempre. `cargo xtask boot-test` no lo
detectaba porque termina QEMU en cuanto ve el marcador de la shell, que se
loguea *antes* de entrar al bucle. En uso interactivo (`cargo xtask run`)
habría sido visible de inmediato.

El estado corregido se verificó empíricamente: con el timer vivo, los
ticks avanzan de forma sostenida y regular (`ticks=100` … `ticks=500`,
≈1 s por cada 100) mientras la shell está inactiva en `hlt` — es decir,
`hlt` vuelve a despertar 100 veces por segundo.

### Verificación ejecutada

- Host: 1 test nuevo (divisor del PIT para 100 Hz); `cargo xtask test`
  ahora suma 37 pruebas en verde (14 `arch`, 8 `hal`, 11 `kernel`, 4
  `xtask`).
- QEMU automatizado: `cargo xtask boot-test --marker "AION: ticks=100"`
  y `--marker "AION: ticks=500"` (el timer dispara *y* se sostiene);
  `boot-test --repeat 10` con el marcador de la shell: 10/10.
- Sin lógica pura que probar en host en el remapeo/programación (es
  secuenciación de hardware): no se inventaron pruebas falsas.

### Simplificaciones documentadas

- Solo IRQ0 está desenmascarado; el resto de líneas del PIC (incluida la
  cascada hacia el esclavo) sigue enmascarado hasta el Incremento 4
  (teclado, IRQ1).
- No se insertan esperas `io_wait` entre escrituras al PIC: innecesarias
  en el hardware emulado que es el objetivo de esta fase.

## Incremento 2 — `ExitBootServices` + PIC enmascarado + consola VGA

### ADR

Ver `docs/adr/0002-fase2-exit-boot-services.md` — este incremento sí
dispara el ADR que `docs/adr/0001-fase0-boot-path.md` predijo.

### Hallazgo real: OVMF usa GOP, no modo texto VGA legado

El diseño original de este incremento (basado en el plan) asumía que
escribir directamente al buffer de texto VGA en `0xB8000` bastaría para
tener una consola visible, como en un arranque BIOS clásico. **Verificado
con una captura de pantalla real** (vía `screendump` del monitor de QEMU,
convertida de PPM a PNG e inspeccionada visualmente): en este entorno
real (OVMF + QEMU), el display está controlado por un framebuffer lineal
GOP en modo gráfico (1280×800 observado), no por el modo de texto VGA
clásico. Escribir a `0xB8000` no corrompe nada (la memoria es válida y de
nuestra propiedad) pero tampoco aparece en pantalla.

Decisión (confirmada con el usuario): `boot/src/console_vga.rs` se queda
como un backend correcto pero con salida visible no confirmada — mantiene
la frontera del trait `Console` y la lógica de fila/columna/scroll, útil
como base, pero el renderizado de texto real y visible sobre el
framebuffer GOP (con fuente de mapa de bits) se difiere al Incremento 4,
empaquetado junto con completar `Console` para el teclado PS/2 — así el
trabajo de "consola visible" se hace una sola vez, en el momento en que
además hay entrada de teclado real para ejercitarla interactivamente.
Ninguna verificación automatizada de este incremento (`cargo xtask
boot-test`) depende de esto — todas pasan por debugcon, un canal
completamente distinto (ver Incremento 1 más abajo sobre por qué estos
canales son independientes).

### Verificación ejecutada

- Host: 8 tests nuevos en `hal::memory_map` (clasificación de tipos de
  memoria UEFI, manejo de capacidad agotada sin panic, suma de páginas
  usables).
- QEMU automatizado: `cargo xtask boot-test --marker
  AION-PHASE2-POST-EXIT-OK` confirma la transición en sí; `cargo xtask
  boot-test --repeat 10` (marcador por defecto, el de la shell) — 10/10
  arranques exitosos, con el mapa de memoria real reportado (104
  regiones, ~63911 páginas usables ≈ 249 MB de 256 MB configurados).
- Visual, una vez, documentado aquí: captura de pantalla real
  (`screendump`) inspeccionada para descubrir el hallazgo de GOP arriba —
  el mismo tipo de verificación honesta que exige `AGENTS.md`/`CLAUDE.md`,
  que llevó a encontrar una brecha real en vez de asumir que "compiló y
  pasó boot-test" era suficiente.

## Incremento 1 — GDT + TSS/IST + IDT + manejadores de excepción

### No se necesitó ADR

Este incremento no llama a `ExitBootServices`, no carga una imagen de kernel
separada y no cambia la forma de `BootInfo`. Todo ocurre dentro del mismo
único binario `.efi` de Fase 0/1, todavía dentro de UEFI Boot Services. No
se dispara ninguna condición de `ARCHITECTURE.md` que exija ADR. La
transición real (`ExitBootServices`) es del Incremento 2.

### Sin ABI `x86-interrupt`: trampolines naked escritas a mano

`rust-toolchain.toml` fija Rust estable. `extern "x86-interrupt"` sigue
bajo feature-gate nightly. Los manejadores usan `#[unsafe(naked)]` +
`core::arch::naked_asm!` (estable desde ~1.88), con macros
(`stub_no_error_code!`/`stub_with_error_code!`) que generan el prólogo
específico de cada vector (con o sin código de error puesto por hardware)
antes de saltar a un `common_trampoline` compartido.

### Alcance deliberadamente reducido de manejadores explícitos

Solo `#DE`(0), `NMI`(2), `#BP`(3), `#DF`(8), `#GP`(13) y `#PF`(14) tienen
entradas IDT reales. El resto de 0-31 queda ausente: nada en este
incremento ejecuta `int n` hacia ellos, y si algún bug futuro los alcanza,
usar una puerta ausente causa una falla en cascada (`#GP`/`#NP`) que
termina en el manejador de `#DF`, respaldado por su propia pila IST —
una red de seguridad deliberada, no un hueco. Los vectores 32-255 (rango
de interrupciones de hardware) sí tienen un catch-all real e instalado
(`spurious_interrupt_stub`) — ver más abajo por qué esto resultó ser
necesario de verdad, no solo defensivo.

### Hallazgos reales durante el bring-up (los bugs que costó encontrar)

Arrancar GDT/IDT/excepciones por primera vez expuso una cadena de bugs
genuinos, cada uno enmascarando al siguiente. Se documentan en el orden en
que se encontraron porque cada uno es una lección reusable para cualquiera
que agregue manejadores de interrupción nuevos en fases futuras:

1. **`options(nostack)` mintiéndole al compilador.** El bloque de asm de
   `gdt::init()` que recarga los segmentos (secuencia `retfq`) hace `push`
   dos veces — pero estaba marcado `options(nostack)`. Esa opción le dice
   al compilador "este bloque no toca la pila", permitiéndole seguir
   confiando en su "red zone" (128 bytes bajo RSP, válidos para funciones
   hoja según la ABI) para variables locales propias. El `push` real
   pisaba esa zona. Corrección: quitar `nostack` de ese bloque específico.

2. **El manejador de interrupción también necesita proteger la red zone
   ajena.** `common_trampoline` se ejecuta sobre la misma pila del código
   interrumpido, sin cambio de pila (IST=0). Si ese código era una función
   hoja usando su red zone, los primeros `push` del trampolín la
   pisarían. Corrección: `sub rsp, 128` como primera instrucción,
   liberado con `add rsp, 128` al final.

3. **El struct de Rust debe reflejar ese hueco.** Agregar el `sub rsp,
   128` sin actualizar `InterruptStackFrame` significaba que `vector`,
   `rip`, etc. se leían 128 bytes desalineados de donde realmente estaban.
   Corrección: campo de relleno explícito `_red_zone_guard: [u64; 16]`
   entre `rax` y `vector`, verificado con `core::mem::offset_of!` en
   `const` asserts — no confiar en la aritmética mental, verificarla en
   tiempo de compilación.

4. **El timer interno de UEFI/OVMF puede llegar después de `cli`.**
   Verificado con el trace `-d int` de QEMU: una interrupción de hardware
   real (vector `0x20` observado) llegó *después* de ejecutar `cli`,
   aterrizando en una entrada IDT ausente (tipo de gate inválido → `#GP`
   en cascada). `cli` bloquea la *siguiente* admisión de interrupciones;
   no cancela retroactivamente una que el CPU ya empezó a entregar un
   ciclo antes. Por eso el catch-all de 32-255 (`spurious_interrupt_stub`,
   silencioso, sin EOI) no es decorativo: sin él, este incremento falla de
   forma intermitente y difícil de reproducir.

5. **`x86_64-unknown-uefi` usa la convención de llamada de Microsoft x64,
   no SysV.** El primer bloqueo real: `common_trampoline` pasaba el
   puntero al frame en `RDI` (SysV), pero `rust_interrupt_handler`
   —compilado para el target UEFI— espera su primer argumento en `RCX`
   (convención Microsoft x64, la misma que usa el propio firmware UEFI).
   Además esa convención exige 32 bytes de "shadow space" reservados por
   quien llama, antes de la propia alineación de 16 bytes. Sin esto,
   `frame` llegaba como puntero nulo. Esta es la clase de bug que
   **reaparecerá** en cualquier incremento futuro que llame desde asm
   naked a una función Rust normal en este target — no es específico de
   este trampolín.

6. **Usar un registro *caller-saved* para sobrevivir una llamada.** Tras
   corregir el punto 5, seguía apareciendo un *double fault* justo después
   de que el manejador de `#BP` terminara con éxito. Causa: el valor de
   RSP a restaurar se guardaba en `RAX` a través del `call` — pero RAX es
   volátil (caller-saved) en ambas convenciones (SysV y Microsoft x64), así
   que la función llamada podía pisarlo libremente, y lo hacía. RSP se
   restauraba con basura, colapsando la pila (`#GP`→`#PF`→`#DF` en
   cascada, verificado con `-d int`: `SP` caía a valores cercanos a cero).
   Corrección: usar `RBX` (callee-saved en ambas convenciones) para ese
   valor.

Cada uno de estos seis puntos se verificó empíricamente con el trace
`-d int` de QEMU o con aserciones `offset_of!` en tiempo de compilación —
ninguno se "arregló" solo por razonamiento sin observar el comportamiento
real. Ver `AGENTS.md`/`CLAUDE.md`: nunca declarar algo corregido sin
haberlo observado.

### Verificación ejecutada

- Host: 13 tests de codificación GDT/IDT/TSS (`arch/x86_64`), agregados a
  `cargo xtask test` y a los jobs de CI (antes solo cubrían
  `aion-hal`/`aion-kernel`/`xtask`).
- QEMU automatizado: `cargo xtask boot-test --repeat 10` — 10/10 arranques
  exitosos, cada uno con el self-test de `int3` (`#BP`) pasando y logueando
  `"breakpoint handler OK"` antes de llegar al marcador de la shell de
  Fase 1 (sin cambios en `kernel::shell`).
- Manual, una vez, documentado aquí (revertido antes de commitear, mismo
  patrón que la verificación de teclado de Fase 1): se forzó un `#DE` real
  (división entre cero) y un `#GP` real (carga de un selector inválido en
  `GS`), cada uno por separado —ambos son fatales— confirmando que cada
  uno loguea correctamente (mensaje, código de error cuando aplica, RIP
  legítimo) y detiene el sistema de forma segura en vez de corromper
  memoria o colgarse silenciosamente.

### Simplificaciones documentadas de este incremento

- No se remapea el PIC todavía (Incremento 3) — por eso `cli` se ejecuta
  antes de tocar el IDT, y por eso el catch-all de 32-255 es la mitigación
  real mientras tanto.
- `NMI` tiene un manejador no fatal (log + retorno) como mitigación barata
  adicional, dado que las NMI no se enmascaran con `cli`.
- El `#DF` es incondicionalmente fatal — no se intenta preservar registros
  ni reanudar ejecución, siguiendo la práctica estándar de kernels.
