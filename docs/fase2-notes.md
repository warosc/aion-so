# Notas de implementación — Fase 2

> **Nota (ADR 0003):** las secciones de los Incrementos 1 a 4 son anteriores
> a la migración de marca AION OS → HARLAN OS. Se conservan sin reescribir
> porque registran lo que se decidió y observó entonces, incluidas líneas de
> log literales como `AION: ticks=300`. `AION` / `aion-*` / `aion_*`
> equivalen hoy a `HARLAN` / `harlan-*` / `harlan_*`; tabla completa en
> `docs/adr/0003-brand-migration-harlan.md`.

## Cierre de Fase 2

**Criterio de salida de `ROADMAP.md`**: "prueba prolongada sin corrupción
de memoria ni panic inesperado". Se cumple con `cargo xtask soak-test`
(Incremento 8):

- 120 s en cada PR (CI);
- una corrida manual de 30 minutos, que `WORKFLOW.md` exige antes de
  promover `develop` a `main` (resultado en el Incremento 8).

La corrida larga mantiene el heap bajo estrés continuo, verificando el
contenido de cada bloque, mientras el temporizador interrumpe 100 veces por
segundo. Se exige que no haya pánico, excepción ni reinicio de CPU.

Lo que dejó la fase: GDT/IDT con manejadores de excepción, salida de Boot
Services, PIC/PIT a 100 Hz, teclado PS/2 por IRQ, consola sobre el
framebuffer GOP, asignador de frames, mapper con raíz propia en la mitad
alta, y heap con `alloc`. Decisiones registradas en los ADR 0002, 0004,
0005 y 0006.

### Límites que hereda la Fase 3 y siguientes

- **Memoria física**: cobertura fija de 256 MiB. La memoria de boot
  services (≈43 MiB) está retenida porque la pila y las tablas inferiores
  del firmware viven ahí. Los frames se entregan sin poner a cero. No hay
  zonas DMA. El asignador no es `'static` ni tiene candado. (ADR 0004)
- **Memoria virtual**: la página 0 está mapeada con escritura, así que una
  desreferencia nula no falla. La raíz del kernel está en `0x1000`
  (memoria baja; revisar al llegar SMP). No se parten páginas grandes ni se
  liberan tablas intermedias. El mapper vive en `kmain`. Recuperar la
  memoria de boot services y desmapear la página 0 requieren reconstruir
  el mapa de identidad (Fase 3). (ADR 0005)
- **Heap**: 4 MiB fijos, primer ajuste lineal, metadatos dentro del heap
  (acotados por comprobación de límites y `check()`). Ningún manejador de
  interrupción debe asignar memoria. Sin heap si `take_over` se rechaza.
  (ADR 0006)
- **Interrupciones**: PIC/PIT, no APIC; un solo núcleo. `IrqLock` provoca
  un pánico ante reentrada: con SMP tendrá que girar en espera. Solo
  algunos vectores de excepción tienen manejador explícito (el resto cae en
  `#DF`).
- **Consola y teclado**: US QWERTY y Shift. La latencia de hasta 10 ms por
  despertar perdido en `idle_once`. Sin cursor, scrollback ni color. Solo
  GOP RGB/BGR. El framebuffer se usa tras `ExitBootServices`, algo que la
  especificación UEFI no garantiza (revalidar en la Fase 5).
- **Verificación**: todo en QEMU/TCG con OVMF; nada en hardware físico ni
  con teclados USB (Fase 5). El modo soak sustituye a la shell, así que no
  ejercita el teclado.

## Incremento 10 — Guard pages en las pilas del kernel (endurecimiento)

Segundo de los tres incrementos de endurecimiento. Estrategia completa en
`docs/memory-safety.md`.

### ADR

No hay ADR nuevo. El ADR 0005 ya reservó la mitad alta para el kernel y
dejó el reparto de ranuras abierto; esta usa una ranura más (la 258) sin
cambiar ninguna decisión.

### El problema

Hasta ahora el kernel corría sobre la pila que le dejó el firmware: 128 KiB
de memoria de boot services con más datos del firmware justo debajo. Y la
pila del manejador de double fault era un estático de 16 KiB dentro de la
imagen, pegado a otros datos del kernel. En ambos casos, pasarse de la pila
no fallaba: **corrompía lo que hubiera al lado**, en silencio.

### Qué hace

- `kernel::memory::stacks`: mapea las pilas dentro de la ranura 258 de la
  PML4 (`KERNEL_STACKS_START`), en este orden: página sin mapear, pila del
  kernel (64 KiB), página sin mapear, pila de double fault (16 KiB), página
  sin mapear. Las pilas son escribibles y **no ejecutables**.
- `arch::stack::switch_to`: función *naked* que mueve RSP a la pila nueva y
  salta a un punto de entrada que nunca retorna. Prepara la pila como si se
  hubiera llegado con un `call` (32 bytes de shadow space de la convención
  de Microsoft x64 y una dirección de retorno falsa a 0, para que un
  retorno accidental falle en el acto).
- `arch::set_double_fault_stack`: apunta la entrada IST1 del TSS a la pila
  nueva. La CPU lee el TSS cuando ocurre la excepción, así que no hace
  falta recargar TR.
- `kmain` mapea ambas pilas, apunta el IST y se muda a la pila del kernel
  antes de la parte larga (banner, shell o soak). Si el mapeo falla, sigue
  en la pila del firmware y lo registra: no es fatal.
- Dos líneas de log nuevas hacen visible el resultado en cada arranque: la
  dirección de las pilas, y la dirección real de la pila en uso.

### Verificación ejecutada

- Host: 153 pruebas en verde. Las 3 nuevas de `stacks` usan un `PageMapper`
  falso y comprueban que las guard pages quedan sin mapear, que las pilas
  son escribibles y no ejecutables, que no comparten ningún marco y que
  quedarse sin marcos se reporta.
- Pruebas de mutación (a mano, revertidas): sin guard page debajo; pilas
  ejecutables; sin guard page entre las dos pilas; y una pila más corta de
  lo pedido. Las 4 detectadas.
- **Prueba negativa de extremo a extremo**: con una recursión infinita
  provocada a propósito (temporal, revertida), el kernel registra
  `#DF DOUBLE FAULT on the stack at 0xffff810000015f3f - halting` y se
  detiene. Esa dirección cae dentro de la pila de double fault recién
  mapeada (`0xffff810000012000`-`0xffff810000016000`), lo que demuestra
  **tres cosas a la vez**: la guard page detiene el desbordamiento, el IST
  apunta a la pila nueva, y el fallo es legible en vez de corrupción.
- QEMU: `stacks = kernel 64 KiB at 0xffff810000001000, double fault 16 KiB
  at 0xffff810000012000, guard pages around both` y `kernel running on the
  stack at 0xffff810000010f17` (dentro del rango de la pila nueva).
- `boot-test --repeat 10` 10/10; `--memory 1G --repeat 2` 2/2; soak de
  120 s PASS; `fmt-lint` limpio; builds release OK.
- QEMU interactivo: `help`, `version`, comando desconocido y `shutdown`
  correctos con la shell ya sobre la pila protegida.

### Riesgos y límites

- La pila del kernel es de 64 KiB fijos y no crece: si alguna vez hiciera
  falta más, el síntoma sería este double fault, que es el modo de fallo
  deseable.
- El arranque, hasta el cambio de pila, sigue ocurriendo sobre la pila del
  firmware, sin guardas. El siguiente incremento (tablas propias) es el que
  deja de depender de esa memoria.
- La pila de double fault tiene guardas, pero un desbordamiento *dentro*
  del manejador de double fault sería un triple fault (reinicio). El
  soak-test lo detectaría.
- `switch_to` asume la convención de Microsoft x64, como el resto del
  ensamblador de este crate (es la del objetivo UEFI).

## Incremento 9 — Los frames se entregan a cero (endurecimiento)

Primero de los tres incrementos de endurecimiento que pidió el usuario tras
cerrar la Fase 2, con la estrategia completa en `docs/memory-safety.md`.
Los otros dos: guard pages, y pila y tablas propias (que además permitirán
recuperar la memoria de boot services y dejar la página 0 sin mapear).

### ADR

No hay ADR nuevo: es una actualización del ADR 0004, que decía
explícitamente que los frames se entregaban sin poner a cero.

### Qué hace

- `PhysWindow`: el único sitio que convierte un frame físico en un puntero
  (`base + dirección física`). Hoy es el mapa de identidad (base 0), que la
  toma de control de la paginación verifica; en la Fase 3 solo cambiará la
  base. `frame_ptr` comprueba el desbordamiento.
- `ZeroedFrames<A>`: envuelve un asignador y pone a cero cada frame antes de
  entregarlo, de modo que **ningún consumidor puede olvidarlo**: ni una
  tabla nueva con entradas basura, ni un frame que conserve lo que escribió
  su dueño anterior. El asignador de bitmap sigue siendo puro y sin
  `unsafe`; todo el acceso a memoria de este camino vive en el envoltorio.
- `KernelFrames` (el alias que usa el kernel) expone además las operaciones
  del bitmap: liberar, contar y consultar.
- La autoprueba de arranque escribe un patrón en un frame, lo libera y
  exige que **vuelva a cero** al reutilizarse.
- `cargo xtask boot-test`/`soak-test` aceptan `--memory`, así que el
  arranque y el soak se prueban con varias cantidades de RAM (punto 10 de
  la estrategia).

### Verificación ejecutada

- Host: 150 pruebas en verde (4 nuevas de `ZeroedFrames`/`PhysWindow` y 1
  de los argumentos de memoria). Las nuevas usan páginas reales del proceso
  de prueba, así que ejercitan el mismo código `unsafe` que el kernel.
- Pruebas de mutación (a mano, revertidas): entregar el frame sin ponerlo a
  cero; poner a cero solo un byte; y que la ventana ignore la dirección del
  frame o su base. Las 4 detectadas (una de ellas tumbando el proceso de
  pruebas, que también es detección). El detector de mutaciones mira ahora
  el código de salida, no solo la cadena `test result: FAILED`: dos de
  estas mutaciones no fallan "limpiamente".
- QEMU con 256 MiB, 512 MiB y 1 GiB: arranca en los tres casos. Con 512 MiB
  quedan 56 939 marcos usables ignorados por encima de la cobertura y con
  1 GiB, 188 011: la limitación de cobertura fija se comporta como está
  documentada, sin pánico y registrada en el log.
- `boot-test --repeat 10` 10/10; `--memory 1G --repeat 3` 3/3; soak de
  120 s con 256 MiB y con 512 MiB: PASS.
- Log de arranque: `frame allocator self-test OK (frames arrive zeroed)` y
  `heap = 4096 KiB …, 1036 zeroed so far`.

### Riesgos y límites

- Poner a cero cuesta una escritura de 4 KiB por frame. En el arranque son
  ~1 036 frames (4 MiB), imperceptible; si algún día se asignan muchos
  frames por segundo, habrá que medirlo.
- La ventana sigue siendo el mapa de identidad del firmware: mientras siga
  vigente, el kernel depende de él para tocar memoria física.

## Incremento 8 — Soak test y cierre de la fase

### No se necesitó ADR

No cambia el layout de memoria, la ABI ni el boot path. La feature `soak`
solo cambia lo que `kmain` hace después de todas las autopruebas, y solo en
las compilaciones de prueba.

### Qué hace

- Feature `soak` (`harlan-boot` la reenvía a `harlan-kernel`): después de
  las autopruebas, `kmain` ejecuta `memory::heap::soak()` en lugar de la
  shell. Son rondas infinitas de `stress`, de 20 000 ciclos cada una y con
  una semilla distinta por ronda, con las interrupciones vivas. Tras cada
  ronda registra `HARLAN: soak round R: T heap cycles, 0 corruption`.
- `cargo xtask soak-test --duration-secs --min-ticks --min-heap-cycles`:
  compila con `soak` y deja correr QEMU (sin pantalla) toda la duración,
  con `-d guest_errors,cpu_reset -D target/soak-test-qemu.log`. Después,
  `analyze_soak` (función pura, con pruebas unitarias) exige:
  - que QEMU siga vivo todo el tiempo;
  - ninguna línea de pánico, excepción o NMI;
  - los tres marcadores de arranque exactamente una vez (detecta bucles de
    reinicio);
  - ticks estrictamente crecientes y por encima del mínimo;
  - rondas consecutivas y un total de ciclos por encima del mínimo;
  - **ningún reinicio de CPU después del arranque**.
- `build_commands` acepta una lista de features, y `--heap-stress` la usa.
- CI: nuevo job `soak-test`, de 120 s con al menos 10 000 ticks y 50 000
  ciclos.

### Hallazgos reales

- **Formato de `-d cpu_reset` medido en QEMU 11.1.0**, no supuesto: cada
  reinicio empieza con `CPU Reset (CPU 0)` seguido de un volcado de
  registros. Al crear la máquina salen 2; un `reboot` del invitado añade 1;
  un arranque normal no produce ninguna línea de `guest_errors`. Como la CI
  usa otra versión de QEMU (la de Ubuntu), no se fija ese "2": xtask cuenta
  los reinicios en el momento en que aparece el marcador del soak, y
  cualquier aumento posterior es un fallo. Sirve con cualquier versión.
- **Prueba negativa de extremo a extremo**: un triple fault provocado a
  propósito en la ronda 30 (IDT vacío más `int3`) **no deja rastro en el
  log del kernel**, porque ningún manejador puede ejecutarse. El soak-test
  falla igualmente por seis vías: marcadores ×3, ticks que retroceden,
  rondas no consecutivas y "2 CPU reset(s) after the kernel booted". Es
  justo el caso que justificaba la señal ortogonal de QEMU. La mutación se
  revirtió.

### Verificación ejecutada

- Host: `cargo xtask test` pasa 145 pruebas en verde (51 `arch`, 17
  `fbcon`, 15 `hal`, 49 `kernel`, 13 `xtask`). Nuevas: 8 de `xtask`
  (features, argumentos `-d`, y el analizador ante un soak sano, progreso
  insuficiente, pánico o excepción, segundo arranque, arranque ausente,
  ticks estancados o rondas saltadas, y reinicio tras el arranque).
- `cargo xtask fmt-lint` limpio; `clippy` de `harlan-boot` con
  `--features soak,heap-stress` sin avisos.
- `cargo xtask soak-test --duration-secs 120 --min-ticks 10000
  --min-heap-cycles 50000`: **PASS**, con 11 700 ticks, **4 580 000 ciclos
  de heap en 229 rondas**, 2 reinicios de CPU al crear la máquina y ninguno
  después.
- Prueba negativa del triple fault: **FAIL** como se esperaba (ver arriba).
- **Corrida manual de 30 minutos** (`--duration-secs 1800 --min-ticks
  150000 --min-heap-cycles 10000000`): **PASS**, con **179 400 ticks** (1 794 s
  a 100 Hz), **68 600 000 ciclos de heap en 3 430 rondas**, 0 corrupción, 0
  pánicos o excepciones, y ningún reinicio de CPU tras el arranque.
- Tras añadir la feature, el arranque normal se volvió a verificar:
  `boot-test --repeat 10` 10/10 y `--heap-stress` correcto.

## Incremento 7 — Heap del kernel

### ADR

`docs/adr/0006-fase2-kernel-heap.md`: ubicación (ranura 257 de la PML4,
`0xFFFF_8080_0000_0000`), tamaño (4 MiB fijos), algoritmo y candado.

### Qué hace

- `kernel/src/memory/heap/free_list.rs`, `FreeListHeap`: lista libre
  intrusiva ordenada por dirección, primer ajuste, división y fusión, con
  granularidad de 16 bytes (igual al tamaño de la cabecera de un hueco:
  cualquier sobrante es un hueco válido).
  - Los bloques no llevan cabecera: su tamaño sale del `Layout`.
  - Cada acceso a una cabecera comprueba los límites del heap.
  - La doble liberación y liberar memoria ajena provocan un pánico.
  - `check()` verifica todos los invariantes.
  - El puntero base conserva la *provenance* (`with_addr`).
- `kernel/src/sync.rs`, `IrqLock`: deshabilita las interrupciones mientras
  está tomado y restaura el estado anterior (las secciones anidadas no las
  reactivan antes de tiempo). Una reentrada provoca un pánico en vez de
  girar en espera. Es el primer consumidor de `hal::InterruptControl`,
  que esperaba uno desde el Incremento 1.
- `kernel/src/memory/heap.rs`:
  - `KernelHeap<I>` (`FreeListHeap` detrás de un `IrqLock`) implementa
    `GlobalAlloc`.
  - `static HEAP` es el `#[global_allocator]` (salvo en las pruebas de
    host).
  - `init` mapea 4 MiB página a página con frames nuevos (NX).
  - `stress` es una carga determinista (SplitMix64) de hasta 256 bloques
    vivos de 1 a 8192 bytes y alineaciones de 1 a 128 bytes (4096 una vez
    de cada 64). Rellena cada bloque con su propio byte y lo verifica antes
    de liberarlo, ejecuta `check()` cada 64 ciclos y al final exige volver a
    los bytes libres iniciales.
  - `self_test` comprueba que `Vec`, `Box` y `String` reciben memoria del
    rango del heap y después ejecuta `stress`.
- `STRESS_CYCLES`: 2 000 en cada arranque, 200 000 con la feature
  `heap-stress` (`harlan-boot` la reenvía a `harlan-kernel`).
  `cargo xtask boot-test --heap-stress` compila con ella; la CI la ejecuta
  una vez por PR.
- `kmain`: después de la autoprueba del mapper ejecuta `heap::init` y
  `heap::self_test`. Si no hay mapper (`take_over` rechazado), no hay heap
  y lo registra en el log.

### Desviaciones del plan original

1. El heap vive en la mitad alta (ADR 0005), no donde `virt == phys`: la
   paginación queda ejercitada de verdad.
2. El candado no gira en espera: la reentrada provoca un pánico, que es lo
   correcto en un solo núcleo con las interrupciones apagadas.
3. La prueba de estrés verifica el contenido de cada bloque (detecta
   solapes) y es el mismo código en el host y en QEMU. El plan pedía ciclos
   de asignar y liberar, sin especificar cómo detectar la corrupción.

### Verificación ejecutada

- Host: `cargo xtask test` pasa 137 pruebas en verde (51 `arch`, 17
  `fbcon`, 15 `hal`, 49 `kernel`, 5 `xtask`). Nuevas:
  - 14 de `FreeListHeap` sobre búferes reales, es decir, el mismo código
    `unsafe` que corre en el kernel: alineación, exhaustión, relleno por
    alineación, tamaño cero, peticiones imposibles, fusión en cualquier
    orden de liberación, doble liberación, memoria ajena, cabecera
    corrupta detectada por `check()`, puntero `next` corrupto que provoca
    un pánico en vez de escribir fuera, y 20 000 operaciones aleatorias con
    verificación de contenido;
  - 4 de `IrqLock`;
  - 4 de `KernelHeap`, entre ellas **la carga de arranque con 100 000
    ciclos sobre 1 MiB**;
  - 1 de la plomería `--heap-stress` en xtask;
  - 1 de la ranura del heap.
- Pruebas de mutación (a mano, revertidas): se introdujeron 10 bugs
  deliberados, uno cada vez: perder la cola al dividir un hueco; perder el
  relleno de alineación; no fusionar con el hueco siguiente; no fusionar con
  el anterior; sin detección de doble liberación; tamaños sin redondear a
  16; ignorar la alineación pedida; no descontar los bytes libres; el
  candado reactivando las interrupciones siempre; y el candado sin detectar
  la reentrada. Las pruebas detectaron los 10.
- `cargo xtask fmt-lint` limpio; builds release correctos, también con
  `--features heap-stress`.
- QEMU:
  - `heap = 4096 KiB at 0xffff808000000000; 51853 frame(s) left`. Cuadra al
    frame: 52 885 − 1 (raíz) − 3 (autoprueba del mapper) − 1 024 (páginas)
    − 4 (tablas del heap).
  - `heap stress complete, 2000 cycles, 0 corruption (… after: 1 hole(s),
    4194304 of 4194304 bytes free)`.
  - Con `--heap-stress`, 200 000 ciclos: pico de 139 032 bytes vivos y el
    mismo final, con un solo hueco y todo libre.
  - `boot-test --repeat 10`: 10/10; el marcador del estrés corto ×10:
    10/10; el estrés largo ×3: 3/3; `ticks=500` correcto.
- QEMU interactivo: `help` y `version` correctos, `shutdown` apaga QEMU y
  `reboot` produce un segundo arranque completo, con el heap reinicializado
  y verificado dos veces.

### Simplificaciones y riesgos documentados

- 4 MiB fijos, sin crecimiento: agotarlo provoca un pánico registrado.
- Primer ajuste con una lista lineal: O(huecos) por asignación. Basta para
  las cargas de Fase 2.
- Los metadatos van dentro del heap: un desbordamiento de un bloque puede
  pisar la cabecera del hueco vecino. Queda acotado: los accesos comprueban
  los límites y `check()` la detecta, así que se convierte en pánico y no
  en una escritura arbitraria. Aun así no se detecta hasta el siguiente
  acceso a ese hueco.
- No es reentrante desde interrupciones: ningún manejador debe asignar
  memoria (se detecta con un pánico).
- Sin heap cuando `take_over` se rechaza (por ejemplo, en una CPU sin NX):
  el kernel arranca igual porque nada más asigna todavía, pero cualquier
  código futuro que asigne en esa configuración terminará en pánico.

## Incremento 6 — Page mapper: el kernel toma la raíz de las tablas de páginas

### ADR

`docs/adr/0005-fase2-kernel-page-tables.md`: fija el layout virtual de
Fase 2 y que la tabla raíz pasa a ser del kernel.

### Hallazgo real: las tablas del firmware son de solo lectura

El plan preveía mapear "sobre la tabla de páginas única y activa heredada
de UEFI". Antes de escribir código se midió, con instrumentación temporal
que no se commiteó:

- `CR0 = 0x80010033`: **WP activo**, así que ring 0 respeta el "solo
  lectura".
- La PML4 (`0xf801000`), las PDPT y las PD están en páginas de 2 MiB
  **sin bit de escritura** (entrada `0xf8000e1`): es la autoprotección de
  tablas de EDK2. La primera escritura en la PML4 del firmware habría dado
  un #PF. El plan original no era ejecutable.
- `CR4 = 0x668`: 4 niveles (sin LA57), sin PCID ni PGE.
  `EFER = 0xd00`: NXE activo.
- PML4 con solo las ranuras 0 y 1: identidad de 0 a 1 TiB, 524 281 páginas
  de 2 MiB (4 en solo lectura: las de las tablas) y 7 PT de 4 KiB con
  permisos finos (código en solo lectura, datos con NX). No hay páginas de
  1 GiB. A partir de 1 TiB y en toda la mitad alta no hay nada.
- La página 0 está mapeada con escritura permitida (entrada `0xe3`): una
  desreferencia nula no falla.

Solución (ADR 0005): el kernel copia la PML4 en un frame propio y carga
CR3. Traducciones idénticas, tablas inferiores del firmware compartidas y
nunca escritas, y el mapper limitado a la mitad alta, donde todas las
tablas son del kernel.

### Qué hace

- `hal::frame::FrameAllocator` (trait): la paginación de `arch` saca frames
  del asignador del kernel sin depender del crate `kernel`.
  `BitmapFrameAllocator` lo implementa.
- `hal::paging`: `Page`, `PAGE_SIZE`, `PageFlags { writable, executable }`
  (NX salvo que se pida lo contrario; sin bit de usuario), `MapError`,
  `UnmapError` y el trait `PageMapper` (`map`/`unmap` son `unsafe` por el
  aliasing de frames; `translate` es segura).
- `arch/x86_64/src/paging.rs`:
  - `PageTables<A: TableAccess>`: la lógica de recorrido (`adopt`,
    `translate`, `map`, `unmap`, `new_table`), genérica sobre cómo se
    llega a la memoria de las tablas, igual que `fbcon` separa
    `TextConsole` de `Surface`.
  - `IdentityAccess`: el único acceso crudo, a través del mapa de
    identidad.
  - `KernelPageTable::take_over`: comprueba CR4 y EFER, verifica la
    identidad de la raíz del firmware y de la pila, adopta la raíz y
    escribe CR3.
  - `KERNEL_SPACE_START = 0xFFFF_8000_0000_0000`.
- `kernel::memory::paging_self_test`: mapea un frame nuevo en
  `KERNEL_SPACE_START`, comprueba `translate` y `AlreadyMapped`, **escribe
  por el mapeo nuevo y lee por la dirección de identidad del frame** (lo
  que prueba que la CPU recorre las tablas que construyó el kernel),
  desmapea, comprueba `NotMapped` y libera el frame.
- `kmain`: primero la autoprueba del asignador (no escribe ningún frame),
  luego `take_over` (verifica la identidad antes de la primera escritura) y
  por último la autoprueba de paginación. Un rechazo de `take_over` no es
  fatal.

### Desviaciones del plan original

1. CR3 cambia ya en Fase 2 (solo la raíz; las traducciones son idénticas):
   lo impone el hallazgo de arriba.
2. El mapper solo actúa en el espacio del kernel (mitad alta).
3. El recorrido de tablas **sí** se prueba en el host (el plan decía que
   no se podía), gracias a la separación `TableAccess`.
4. `PageFlags::executable` en vez de `no_execute`: NX por defecto.
5. No se parten páginas grandes, y `unmap` no libera tablas intermedias.

### Verificación ejecutada

- Host: `cargo xtask test` pasa 113 pruebas en verde (50 `arch`, 17
  `fbcon`, 15 `hal`, 27 `kernel`, 4 `xtask`). Nuevas: 17 del mapper y 2
  de `Page`. El mapper se prueba sobre memoria física simulada **en la que
  escribir una tabla del firmware o leer memoria que no es una tabla hace
  fallar la prueba**. Cubren:
  - índices y direcciones canónicas;
  - traducción por identidad, páginas de 1 GiB con el bit PAT, y
    direcciones sin mapear;
  - `adopt`: la copia y sus rechazos;
  - tablas nuevas puestas a cero, que se enlazan solo después de estar a
    cero del todo, y la hoja con NX;
  - reutilización de tablas y los flags de solo lectura con ejecución;
  - `AlreadyMapped` sin asignar marcos, y `OutsideKernelSpace` sin
    escribir;
  - quedarse sin frames deja solo tablas completas;
  - un frame no escribible se rechaza sin enlazarse;
  - `unmap` con invalidación de la TLB;
  - las páginas grandes no se parten.
- Pruebas de mutación (a mano, revertidas): se introdujeron 9 bugs
  deliberados, uno cada vez: tablas sin poner a cero; sin NX; entradas
  intermedias sin escritura; sin comprobar identidad y escritura de los
  frames de tabla; sin el límite del espacio del kernel; sin quitar el bit
  PAT; `adopt` ignorando ranuras ocupadas; bajar dentro de una página
  grande; y `unmap` sin `invlpg`. Las pruebas detectaron los 9.
- `cargo xtask fmt-lint` limpio; builds release correctos.
- QEMU: `paging = kernel root table at 0x1000, firmware identity map shared
  read-only` y `page mapper self-test OK`. `boot-test --repeat 10` dio
  10/10; `--marker "HARLAN: page mapper self-test OK" --repeat 10` dio
  10/10; `ticks=500` correcto.
- QEMU interactivo, con la raíz del kernel ya cargada: `help` y `version`
  correctos; **`shutdown` apaga QEMU** por sí solo; **`reboot` produce un
  segundo arranque completo** (la autoprueba de paginación sale OK dos
  veces). Los *runtime services* de UEFI funcionan bajo la raíz del kernel.
- Ruta de error real: con `-cpu qemu64,-nx`, el log registra `paging
  take-over refused (NoExecuteDisabled); staying on the firmware's page
  tables`, la shell arranca y los ticks siguen avanzando.

### Simplificaciones y riesgos documentados

- La página 0 sigue mapeada con escritura, así que una desreferencia nula
  no falla. Desmapearla exige escribir una tabla del firmware: queda para
  cuando el kernel reconstruya el mapa de identidad (Fase 3).
- La raíz del kernel quedó en `0x1000` (memoria baja). El arranque de otros
  núcleos (SMP) necesitará memoria por debajo de 1 MiB; habrá que
  revisarlo entonces.
- Las tres tablas intermedias que construye la autoprueba se quedan en
  memoria (3 frames).
- Si un frame de tabla resulta no escribible, ese frame se pierde: el trait
  `FrameAllocator` no tiene forma de devolverlo. Es un caso que las
  comprobaciones de `take_over` hacen inesperado.
- La seguridad de `IdentityAccess` depende de la disciplina de
  `PageTables`: solo tablas alcanzadas desde la raíz o frames nuevos ya
  verificados. Las pruebas de host lo imponen (una escritura en una tabla
  del firmware las hace fallar).
- El mapper vive en `kmain` (no es `'static`) y no tiene candado: un solo
  núcleo, y ninguna interrupción toca las tablas. Un manejador de fallos de
  página o las syscalls necesitarán acceso global (Fase 3).
- La PML4 original del firmware queda sin usar, en memoria de boot
  services retenida.

## Incremento 5 — Administrador de frames físicos (bitmap)

### ADR

`docs/adr/0004-fase2-physical-memory-policy.md`. La primera versión de este
incremento sostenía que no hacía falta ADR, porque `BootInfo` conserva su
forma y `MemoryRegionKind` es un tipo interno de `hal`. La revisión
cruzada (Codex) lo corrigió: el incremento fija qué memoria física es del
kernel (layout de memoria, que `CLAUDE.md` pone entre los cambios que
exigen ADR) y cambia la semántica del mapa que `boot` entrega en
`BootInfo`. El ADR 0004 registra la decisión y sus alternativas; estas
notas conservan la medición y la verificación.

### Hallazgo real: la pila y las tablas de páginas viven en memoria de boot services

Desde el Incremento 2, `hal::memory_map::classify_memory_type` trataba
`BootServicesCode`/`BootServicesData` como `Usable` tras
`ExitBootServices`, "como hace el stub EFI de Linux". Pero Linux solo
reutiliza esa memoria **después** de pasar a su propia pila y a sus propias
tablas de páginas, y este kernel todavía no ha hecho ninguna de las dos
cosas. Antes de construir el asignador se midió, con instrumentación
temporal (revertida; nunca se commiteó), sobre el mapa real de OVMF:

- **La pila en uso** (RSP `0xfe86f70` al entrar en `kmain`) está en una
  región `BootServicesData` de 32 páginas (128 KiB, `0xfe6b000`).
- **Todas las tablas de páginas activas** están en `BootServicesData`. Se
  recorrió la jerarquía completa desde CR3 (`0xf801000`): 1 034 páginas
  (1 PML4, 2 PDPT, 1 024 PD y 7 PT).
- **La página física 0** es `Conventional` (`0x0`-`0x87000`): un asignador
  ingenuo entregaría la dirección `0x0`.
- El búfer del propio mapa de memoria es `LOADER_DATA` (verificado en el
  código de `uefi` 0.40: es el tipo por defecto de `exit_boot_services`),
  así que ya quedaba reservado.
- 104 descriptores, ninguno solapado ni desalineado. Totales:
  `Conventional` 52 905 páginas (206,7 MiB), `BootServicesCode` 998,
  `BootServicesData` 10 000 (≈43 MiB entre ambas). El resto es reservado,
  runtime, ACPI o MMIO.

Consecuencia: con la clasificación anterior, el primer consumidor que
escribiera en un frame recién asignado (las tablas nuevas del Incremento 6
o el heap del Incremento 7) podía pisar la pila en ejecución o una tabla de
páginas viva. Hoy no se manifestaba solo porque nada asignaba memoria
todavía. Corrección: `MemoryRegionKind::BootServices` como variante propia,
el clasificador pierde el parámetro `post_exit` (que ya no significaba
nada) y el asignador solo gestiona `Usable`. Esos ≈43 MiB (≈17 % de la RAM
de QEMU) quedan retenidos hasta que el kernel tenga pila y tablas propias
(Fase 3, con un CR3 por espacio de direcciones). Recuperarlos será entonces
una decisión explícita, no un efecto de la clasificación.

### Qué hace

- `hal::frame`: `FRAME_SIZE` (4 KiB, igual que la página UEFI) y
  `PhysFrame`, una dirección física alineada. Vive en `hal` y no en
  `kernel` porque la paginación del Incremento 6 (en `arch/x86_64`, que no
  depende de `kernel`) la necesitará.
- `hal::memory_map`: variante `BootServices` y `total_pages(kind)`.
- `kernel::memory::frame_allocator::BitmapFrameAllocator`: un bit por
  frame desde la dirección física 0, sobre almacenamiento que le pasa el
  llamador. Las reglas se inclinan siempre hacia retener: solo regiones
  `Usable`, redondeadas hacia dentro; cualquier frame que toque una región
  no usable queda retenido, redondeado hacia fuera, y gana incluso si se
  solapa con una usable; la página 0 nunca se entrega; lo que queda fuera
  de la cobertura se ignora y se cuenta. La aritmética de las regiones es
  saturada, porque el mapa es un dato del firmware. `allocate` es next-fit
  a nivel de palabra de 64 bits. `deallocate` valida contra las mismas
  reglas (`NotManaged`) y detecta la doble liberación (`NotAllocated`).
  Sin `unsafe`: solo cambia bits de un slice y nunca toca el contenido de
  los frames.
- `kernel::memory::FRAME_BITMAP_WORDS = 1024`: 65 536 frames = 256 MiB, el
  `-m 256M` de `cargo xtask`.
- `kmain` construye el asignador con el bitmap en su propia pila (8 KiB de
  los 128 KiB; al entrar en `kmain` se usaban ≈16,5 KiB). `kmain` no
  retorna nunca, así que vive para siempre, y esa pila está en memoria de
  boot services, que el asignador retiene. Después registra las cifras y
  ejecuta `memory::self_test`: comprueba que el frame de la pila viva **no**
  es asignable (sobre el mapa real) y hace un ciclo de asignar, liberar y
  detectar la doble liberación.

### Desviaciones del plan original

1. El plan no preveía el problema de la memoria de boot services: es el
   hallazgo de arriba.
2. Página 0 retenida y liberación validada (`NotManaged`/`NotAllocated`):
   el plan solo pedía `free()`.
3. Palabras `u64` en vez de `&mut [u8]`: permite saltar palabras llenas y
   usar `trailing_zeros`.
4. Bitmap en la pila de `kmain` y no en un `static`: así no hace falta
   `unsafe`, y todavía no hay un consumidor que necesite acceso global.
5. `PhysFrame` entra ya en `hal`; el trait `FrameAllocator` que usará
   `arch` **no**: llegará con su primer consumidor, en el Incremento 6.

### Verificación ejecutada

- Host: `cargo xtask test` pasa 94 pruebas en verde (33 `arch`, 17
  `fbcon`, 13 `hal`, 27 `kernel`, 4 `xtask`). Nuevas: 16 del asignador
  (entre ellas un extracto del mapa real de OVMF, un modelo de referencia
  con 10 000 operaciones pseudoaleatorias deterministas, la coherencia
  frame a frame entre el bitmap y `manages()`, y regiones corruptas cerca
  de `u64::MAX`), 4 de `PhysFrame` y 1 de `total_pages`. Las pruebas del
  clasificador se actualizaron.
- Pruebas de mutación (a mano, revertidas): se introdujeron 5 bugs
  deliberados, uno cada vez: página 0 entregable; boot services tratada
  como usable; `manages()` ignorando las regiones retenidas; usable
  redondeada hacia fuera; y sin detección de doble liberación. Las pruebas
  detectaron los 5.
- `cargo xtask fmt-lint` limpio en host, freestanding y UEFI.
- QEMU: `memory map = 104 region(s), 52902 usable pages, 10998
  boot-services pages held back` y `frame allocator = 52901 free frame(s)
  (206 MiB) in the 256 MiB covered, 0 usable frame(s) beyond it ignored`.
  52 902 − 52 901 = la página 0, porque no hay solapes. `boot-test
  --repeat 10` (marcador de la shell) dio 10/10; `--marker "HARLAN: frame
  allocator self-test OK" --repeat 10` dio 10/10; y los ticks siguen
  avanzando (`ticks=500`).
- QEMU interactivo (`sendkey` + `screendump`, captura inspeccionada), con
  el bitmap ya en la pila de `kmain`: banner, `help`, `version` y un
  comando desconocido responden igual que antes.

### Simplificaciones y riesgos documentados

- Cobertura fija de 256 MiB: la RAM por encima se ignora y se registra.
  En hardware real (Fase 5) hay que dimensionarla desde el mapa, por
  ejemplo colocando el bitmap en una región usable.
- ≈43 MiB de boot services retenidos (ver el hallazgo).
- El asignador toma prestados el mapa y su almacenamiento, así que vive
  mientras viva `kmain`. Un asignador global (`'static`, con candado) hará
  falta cuando lo pidan un manejador de fallos de página o las syscalls
  (Fase 3).
- `manages()` recorre el mapa en cada liberación: O(regiones), unas 100
  hoy.
- Sin preferencia por memoria baja ni zonas DMA (<16 MiB, <4 GiB): no hay
  ningún consumidor que las pida.
- **Los frames se entregan sin poner a cero**: el asignador nunca toca su
  contenido. Quien los use (las tablas de páginas del Incremento 6) tiene
  que ponerlos a cero.
- La comprobación de la pila en `self_test` supone que la dirección
  virtual coincide con la física (el mapeo de identidad del firmware); el
  Incremento 6 afirma ese mapeo en tiempo de ejecución.
- Un solo núcleo y sin candado: el asignador se usa como `&mut` solo desde
  `kmain`, y ningún manejador de interrupción lo toca.

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
