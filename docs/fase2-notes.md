# Notas de implementación — Fase 2

> **Nota (ADR 0003):** las secciones de los Incrementos 1 a 4 son anteriores
> a la migración de marca AION OS → HARLAN OS. Se conservan sin reescribir
> porque registran lo que se decidió y observó entonces, incluidas líneas de
> log literales como `AION: ticks=300`. `AION` / `aion-*` / `aion_*`
> equivalen hoy a `HARLAN` / `harlan-*` / `harlan_*`; tabla completa en
> `docs/adr/0003-brand-migration-harlan.md`.

## Incremento 4 — Teclado PS/2 por IRQ, consola de framebuffer, shell restaurada

### No se necesitó un ADR nuevo

`BootInfo` no cambia y el boot path sigue siendo el de
`docs/adr/0002-fase2-exit-boot-services.md`. Sí se añadió un paso a ese
camino (consultar GOP *antes* de salir de Boot Services), así que el ADR
0002 recibe una sección de actualización en vez de un ADR aparte. El
framebuffer deliberadamente **no** entra en `BootInfo`: el kernel solo ve
`&mut dyn Console`. Si algún día un consumidor del kernel necesita los
píxeles directamente, eso sí sería un cambio de `BootInfo` y pediría ADR.

### Qué hace

- `hal::framebuffer::FramebufferInfo`: dato puro (base, ancho, alto,
  stride, tamaño de la región). Sin campo de formato de píxel: solo se
  dibuja blanco y negro, cuyo valor de 32 bits es idéntico en RGB y BGR.
- `drivers/fbcon` (`aion-fbcon`, primer crate bajo `drivers/`): la lógica
  de cursor (`TextConsole<S: Surface>`: salto de línea, ajuste al borde,
  scroll, retroceso que cruza un ajuste de línea) está separada de la única
  parte que toca memoria cruda (`FramebufferSurface`), de modo que la
  primera se prueba entera en host contra una superficie falsa. Fuente 8×8
  del crate `font8x8` (elegido con el usuario), dibujada a 2×: celdas de
  16 px, 80×50 caracteres en 1280×800, blanco sobre negro.
  `FramebufferSurface::new` valida el descriptor (tamaño, stride,
  alineación, desbordamiento aritmético) y devuelve `None` en vez de
  escribir fuera de límites.
- `arch/x86_64/src/keyboard.rs`: el manejador de IRQ1 (`on_irq`) solo lee
  bytes crudos del i8042 y los encola; `ScancodeQueue` (anillo SPSC de 64
  entradas sobre atómicos) los pasa al lado consumidor, donde `Decoder`
  (máquina de estados pura, scancode set 1, US QWERTY + Shift) produce
  `ConsoleKey`. `init_controller` deja el i8042 en un estado conocido.
- `pic::remap` ahora deja **todo** enmascarado; cada driver desenmascara su
  línea con `pic::unmask(irq)` una vez instalado su vector (`init_timer` →
  IRQ0, `init_keyboard` → IRQ1). `init_timer` ya no ejecuta `sti`: lo hace
  `kmain` una sola vez, con todos los dispositivos ya configurados.
- `boot/src/framebuffer.rs` consulta GOP antes de `ExitBootServices`;
  `boot/src/console_hw.rs` (`HardwareConsole`) une pantalla + teclado
  detrás de `Console`. `console_vga.rs` (el marcador de posición invisible
  del Incremento 2) se elimina.
- `kernel/src/shell.rs` y el trait `Console` **no cambian ni una línea**
  respecto a Fase 1 (`git diff` vacío): es la prueba de que la frontera HAL
  valía la pena. Solo `kmain` cambia (orden de inicialización).

### Desviaciones del plan original

1. **Cola de teclado**: el plan la protegía con `InterruptControl`. Se
   implementó como anillo SPSC de atómicos — sin `unsafe` y sin enmascarar
   interrupciones, más simple y correcto también con más de un núcleo.
   `InterruptControl` sigue esperando su primer consumidor real (el
   candado del heap, Incremento 7).
2. **Alcance de la decodificación**: el plan decía letras, dígitos, espacio,
   Enter y Backspace. Se cubrió todo el ASCII imprimible de un teclado US
   (+ Shift) porque la shell acepta `is_ascii_graphic()` y su propia prueba
   teclea `!`; con solo letras no habría quedado "restaurada". Sigue sin
   haber Caps Lock, Ctrl/Alt, teclado numérico ni layouts (todo eso →
   `Unknown`, que la shell ignora): no hay consumidor que los pida.
3. La decodificación corre en el lado consumidor, no en la ISR: el código
   que se ejecuta con interrupciones deshabilitadas se reduce a "leer un
   byte y encolarlo".

### Hallazgos reales

- **El i8042 ya venía configurado**: OVMF lo deja con byte de
  configuración `0x67` (IRQ1 habilitada, traducción a set 1 activa).
  `init_controller` escribe el mismo valor (es idempotente) en vez de
  depender de que el firmware lo haya dejado así, y el resultado se loguea
  (`i8042 config 0x67 -> 0x67`) para que un cambio en otro entorno sea
  visible.
- **`open_protocol_exclusive::<GraphicsOutput>` funciona en OVMF**, y el
  puntero al framebuffer (`0x80000000`, 1280×800, stride 1280, 4 096 000
  bytes) sigue siendo utilizable después de `ExitBootServices` (observado
  en pantalla, no asumido). La especificación UEFI no lo garantiza —el
  crate `uefi` lo advierte— pero es el supuesto que usan en la práctica
  todos los cargadores; queda anotado como riesgo para hardware real
  (Fase 5).
- **Un IRQ1 puede quedar "enganchado" mientras la línea está enmascarada**:
  el ACK del comando de habilitar escaneo (0xF4) levanta IRQ1 antes de que
  se desenmascare, y se entrega justo después con el búfer de salida ya
  vacío. Leer `0x60` sin mirar antes el registro de estado devolvería un
  byte viejo (una tecla fantasma), así que `on_irq` comprueba el bit de
  "salida llena".
- **Falsa alarma propia, descartada con datos**: en la primera captura
  parecían faltar el `_` de `x86_64` y el borde superior de la primera
  línea. Se leyeron los píxeles del volcado (`_` presente en las filas
  46-47, x 320-335; fila 0 del glifo `A` iluminada): eran artefactos del
  visor de imágenes, no defectos.

### Verificación ejecutada

- Host: `cargo xtask test` — 73 pruebas en verde (33 `arch`, 17 `fbcon`,
  8 `hal`, 11 `kernel`, 4 `xtask`). Nuevas: 16 de teclado (cola FIFO/llena/
  vuelta del índice, decodificación, Shift izquierdo/derecho
  independientes, prefijo `E0` incluido el "falso Shift", filas del teclado
  contrastadas con la disposición física US) y 17 de `fbcon` (cursor,
  ajuste, scroll, retroceso a través de un ajuste, dibujo de glifos en
  búfer real con stride mayor que el ancho, rechazo de descriptores
  mentirosos). `cargo xtask fmt-lint` limpio, incluidos los objetivos UEFI
  y freestanding.
- QEMU automatizado: `boot-test --repeat 10` (marcador de la shell) 10/10 y
  `--marker "AION: ticks=300" --repeat 10` 10/10.
- QEMU interactivo, con `sendkey` y `screendump` por el monitor y capturas
  inspeccionadas: `help`; mayúsculas y todos los símbolos US (`Hello,
  World! 0-9 [a] {b}`, la fila de dígitos, `!@#$%^&*()_+{}|:"<>?~`);
  retroceso (`abc`⌫⌫ → `a`); `clear`; `shutdown` desde el teclado termina
  QEMU; 30 comandos seguidos (60 filas en una pantalla de 50) → scroll
  correcto; línea de 91 caracteres que se ajusta al borde y 15 retrocesos
  que cruzan el ajuste, sin restos; `reboot` → segundo arranque con banner
  limpio y teclado funcionando.
- Ruta de error: QEMU con `-machine pc,i8042=off` (sin controlador) →
  `AION: PS/2 keyboard unavailable: ControllerTimeout`, el kernel sigue
  (shell lista, ticks sostenidos hasta 2200+), sin cuelgue.
- **No verificado**: hardware físico, teclados USB (Fase 5).

### Simplificaciones y riesgos documentados

- Solo US QWERTY y Shift. Sin repetición configurable, Caps Lock, Ctrl/Alt
  ni teclas extendidas.
- Ventana de "despertar perdido" en `idle_once`: si un IRQ llega entre que
  `read_key()` devuelve `None` y el `hlt`, la tecla espera al siguiente
  tick (≤10 ms con el PIT a 100 Hz). Latencia acotada, no cuelgue.
- Cola de 64 scancodes: si se llena, se descarta el más nuevo y se loguea.
  En ese caso extremo se podría perder un "soltar Shift" y dejarlo
  pegado hasta el siguiente Shift.
- Sin cursor visible, sin scrollback, sin color. Cada scroll copia ~3,9 MB
  con `ptr::copy` (no volátil; el framebuffer es solo-escritura para el
  programa): aceptable en QEMU, a revisar con hardware real, donde leer un
  framebuffer write-combining es lento.
- Solo formatos GOP `Rgb`/`Bgr`. Con `BltOnly`/`Bitmask` (o un descriptor
  inconsistente) el kernel arranca **sin pantalla** —entrada y debugcon
  siguen funcionando— y lo loguea.
- Comentario obsoleto conocido en `kernel/src/shell.rs` (dice que el prompt
  sale por la consola UEFI): se deja tal cual para mantener el archivo
  idéntico a Fase 1.

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
  PIT y por fin ejecuta `sti`. *(Actualizado en el Incremento 4: `sti` pasó
  a `kmain`, y `remap()` deja todo enmascarado mientras que `init_timer`
  desenmascara IRQ0 con `pic::unmask`.)*
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
