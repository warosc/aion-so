# Notas de Fase 3 — Procesos y aislamiento

Lo más reciente arriba. Salida de la fase (ROADMAP.md): dos procesos
aislados se comunican sin compartir memoria no autorizada.

Decisiones de alcance tomadas al abrir la fase: primero la mudanza a la
mitad alta, binario plano incrustado para el primer programa de usuario, y
`syscall`/`sysret` en vez de `int 0x80`.

## Incremento 20 — Los runtime services se mudan a la mitad alta

`docs/adr/0016-fase3-set-virtual-address-map.md`. El cabo suelto que venía
arrastrándose desde el Incremento 17 queda cerrado: **la mitad baja está
completamente vacía**.

### Qué hace

- Una ventana para los runtime services en **PML4 261**: cada rango con
  `EFI_MEMORY_RUNTIME` se mapea en `KERNEL_RUNTIME_START + su dirección
  física`, con la caché que declaró y ejecutable solo si es código. Así,
  rellenar el mapa que UEFI pide es una suma.
- **La llamada la hace el cargador, la decisión la toma el kernel**:
  `boot` conserva el mapa de `ExitBootServices` y expone una función;
  `BootInfo` gana ese puntero. El kernel no aprende UEFI, el cargador no
  decide el layout.
- El orden que exige la especificación: mapear, llamar con los mapeos
  viejos todavía presentes, y **solo entonces** vaciar.
- Si el firmware se niega, no se vacía nada: se queda el mapa del
  Incremento 19 y se registra.
- `PageFlags` gana la política de caché, que hasta ahora solo conocían los
  rangos que sobrevivían abajo.

### Verificación ejecutada

- Host: 216 pruebas en verde.
- QEMU: `the firmware moved 6 descriptor(s) into kernel space at
  0xffff828000000000; 1862 page(s) mapped` y después `the lower half is
  empty: nothing of the firmware's is left there`.
- **La prueba que decide**: `shutdown` apaga y `reboot` rearranca llamando
  al firmware en sus direcciones nuevas. Si la mudanza estuviera mal, no
  habría término medio.
- **Prueba negativa**: leer `0xf5ed000`, donde vivía el código del
  firmware, da `#PF accessing 0xf5ed000, error_code=0x0`.
- `boot-test --repeat 10` 10/10, con 512 MiB 1/1, soak de 120 s PASS,
  `fmt-lint` limpio.
- Mutación: 5. Una detectada a la primera; **tres sobrevivieron** porque la
  función que decide permisos y caché no tenía prueba en host. Escrita con
  un mapper falso —y de paso cubre que un rango que no es del firmware no
  se mueva—, las 5 detectadas.

### Riesgos y límites

- `SetVirtualAddressMap` se llama **una vez por arranque** y no tiene
  vuelta atrás. Si un firmware la implementa a medias —relocaliza parte y
  devuelve error— no hay recuperación; por eso no se vacía nada hasta que
  devuelve éxito.
- Probado con OVMF. Otro firmware puede negarse, y entonces el kernel se
  queda con el mapa del Incremento 19: peor, pero vivo.
- El plan B si esto resulta frágil en hardware real sigue escrito en el
  ADR: un CR3 dedicado al firmware, o dejar de llamarlo y apagar por
  puerto.

## Incremento 19 — El mapa del firmware, entero y con sus atributos

`docs/adr/0015-fase3-runtime-memory-map.md`. Sale de la **revisión cruzada
de Codex** de los PRs #23 y #24: tres hallazgos suyos, dos de ellos altos,
y ninguno visible desde las pruebas que había.

### Lo que encontró Codex

1. **Conservábamos por tipo, no por atributo.** Lo que obliga a mantener un
   rango mapeado es `EFI_MEMORY_RUNTIME`, que también llevan descriptores de
   otros tipos. En QEMU se estaba descartando
   `0xFFC0_0000..0x1_0000_0000`: 4 MiB de flash del firmware, uncacheable.
   Y los atributos de caché no viajaban, así que un MMIO acabaría
   write-back, que corrompe lo que haya detrás.
2. **Un mapa truncado se trataba como completo**, y con él se decidía qué
   desmapear.
3. **La relocalización escribía y validaba a la vez**: un destino malo a
   mitad de tabla dejaba la imagen medio reubicada, y el camino de vuelta
   pretendía seguir desde la dirección vieja.
4. **`klog` bajo SMP** necesita quiescencia, no ordenaciones: un núcleo
   puede cargar el sumidero viejo mientras otro desmapea el código al que
   va a saltar. Con un núcleo basta; queda escrito en el contrato.

### Qué hace

- `hal::memory_map::classify_attributes` traduce el campo crudo del
  descriptor —`runtime` y la caché— al lado del `classify_memory_type` que
  ya existía: puro y probado en host, con el cargador pasando solo bits.
- La caché se elige por **lo más permisivo que el firmware ofrezca**,
  porque el campo lista capacidades: la RAM queda write-back y el MMIO,
  que solo anuncia uncacheable, queda uncacheable.
- Las tablas la reproducen con PWT/PCD. Write-combining exige reprogramar
  el PAT; hasta entonces se mapea uncacheable —más lento, nunca incorrecto.
- Se conserva **todo** rango con el bit runtime, y el arranque registra
  cada uno con su tamaño, tipo y caché.
- `MemoryMap` recuerda si se truncó, y entonces **no se vacía nada**.
- `pe::relocations` valida que cada dirección quepa en la imagen **antes**
  de entregar la primera: relocalizar es todo o nada.

### Verificación ejecutada

- Host: 215 pruebas en verde.
- QEMU: pasan de 5 a **6 rangos conservados** en 10 tablas, con el MMIO
  uncacheable que antes se perdía:
  `the firmware keeps 0xffc00000..0x100000000 (4096 KiB, data, Uncacheable)`.
- `boot-test --repeat 10` 10/10, soak 120 s PASS, `shutdown` apaga,
  `reboot` rearranca, ring 3 sigue entrando y saliendo.
- Mutación: 6, las 6 detectadas. Dos solo lo fueron **después** de mover la
  traducción de atributos de `boot` —que no tiene pruebas de host— a `hal`.
  Ese movimiento es la lección: lo que no se puede probar en host, no vive
  en el cargador.

### Riesgos y límites

- Write-combining sigue mapeándose uncacheable.
- La elección de caché es una heurística sobre una lista de capacidades; si
  algún firmware anuncia write-back en un rango que no lo tolera, esto lo
  creería.
- Y el de siempre, ahora con fecha: los runtime services viven en lo que va
  a ser espacio de usuario. Lo cierra `SetVirtualAddressMap` en el
  Incremento 20.

## Incremento 18 — Ring 3

`docs/adr/0014-fase3-syscall-abi-v0.md`. Por fin corre algo sin
privilegios, y el kernel deja de estar a su alcance.

### Qué hace

- **Descriptores de usuario** en la GDT, en el orden que `sysretq` exige
  (`SS` de `STAR[63:48] + 8`, `CS` de `+ 16`), y el TSS pasa a `0x28`.
- **`syscall`/`sysret`**: `EFER.SCE`, `STAR`, `LSTAR` y `FMASK` (que limpia
  `IF`, `DF` y `AC`). El stub entra con `swapgs`, cambia a una **tercera
  pila con guard pages** —`syscall` no cambia de pila sola— construye el
  marco de registros y llama al kernel.
- **`PageFlags::user`**: el bit de usuario se pone en la hoja **y en cada
  nivel del recorrido**, porque la CPU los ANDea. El mapper deja de
  rechazar la mitad baja cuando la bandera está puesta, y **sigue
  rechazando** mezclar los dos mundos: una página es del kernel o de un
  programa, y lo dice.
- **Un programa plano de 59 bytes** ensamblado a mano dentro de la imagen,
  copiado a una página propia: dice hola con `log(ptr, len)` y sale con
  `exit(7)`.
- **Todo puntero que llega de ring 3 se valida** contra la memoria que el
  programa tiene, antes de leer un byte.

### Lo que la máquina enseñó

**`TSS.rsp0` estaba sin poner.** Es la pila a la que salta la CPU cuando
toma una interrupción o una excepción **mientras corre ring 3**; sin ella,
el primer tick del temporizador después de entrar en modo usuario empuja
sobre la dirección 0 y la máquina triplefaultea sin decir nada. El primer
arranque con el programa corto funcionó **por suerte**: no llegó a caer un
tick. Lo destapó la prueba negativa, que sí provocaba una excepción.

### Verificación ejecutada

- Host: 210 pruebas en verde.
- QEMU, el registro de arranque:
  `entering ring 3 at 0x400000` → `from ring 3: HARLAN: hello from ring 3`
  → `the program exited with 7`, y el kernel sigue hasta el shell.
- **Prueba negativa del aislamiento**: un programa que lee una dirección
  del kernel produce
  `#PF accessing 0xffff800000000000, error_code=0x4, rip=0x400000`. El
  `0x4` es el bit de usuario: la CPU dice que quien lo intentó era ring 3.
  Y el kernel sobrevive para contarlo.
- `boot-test --repeat 10` 10/10, soak de 120 s PASS, `shutdown` apaga,
  `reboot` rearranca, `fmt-lint` limpio.
- Mutación: 6. Cinco a la primera; la sexta —tomar un puntero de ring 3 sin
  comprobarlo— **sobrevivió** porque el manejador no tenía prueba en host.
  Escrita, las 6 detectadas.

### Riesgos y límites

- **Un solo programa, sin espacio de direcciones propio**: comparte las
  tablas del kernel, y lo que lo protege es el bit de usuario, no un CR3
  aparte. El Incremento 19 le da uno.
- **La pila de syscalls es única**: vale mientras haya un proceso y las
  syscalls no se interrumpan (`FMASK` limpia `IF`). Con scheduler habrá que
  darle una por proceso.
- `exit` no vuelve a quien lanzó el programa: continúa el kernel en una
  función guardada, sobre la pila de syscalls. Es un cambio de contexto de
  juguete; el de verdad llega con el scheduler.
- El código del firmware sigue en la mitad baja, o sea en espacio de
  usuario: todavía sin conflicto, porque el programa vive en `0x40_0000` y
  el firmware mucho más arriba, pero sigue siendo el cabo suelto del ADR
  0013.

## Incremento 17 — La mitad baja deja de ser del kernel

`docs/adr/0013-fase3-physical-window.md`. El Incremento 16 sacó el código
del kernel de la mitad baja; quedaban la ventana física, el framebuffer y
el registro.

### Qué hace

- **Ventana onto la memoria física en PML4 260**: `[0, 4 GiB)` en páginas
  de 2 MiB, escribible y nunca ejecutable. Los marcos se ponen a cero por
  ahí y **las tablas de páginas se leen y escriben por ahí**, en vez de por
  su dirección física.
- **La consola sigue a su framebuffer** (`Console::framebuffer_moved`).
- **El registro deja de pasar por el crate `log`**: `hal::klog` es una
  fachada con un puntero a función que el kernel reapunta tras la mudanza.
- **La mitad baja se vacía**, salvo el código del firmware y —esto costó un
  fallo— **sus datos**, que `hal` distingue ahora como `RuntimeData`.

### Lo que la máquina enseñó, otra vez

Dos fallos que solo se vieron arrancando:

1. **El logger seguía apuntando abajo.** Escribí
   `let _ = log::set_logger_racy(...)` y el error quedó escondido: el
   crate rechaza un segundo logger, así que el del cargador siguió puesto y
   la primera línea tras vaciar la mitad baja fue un `#PF` leyendo `.data`
   en su dirección vieja. De ahí la fachada propia: un puntero a función se
   puede reapuntar, un `&'static dyn Log` no.
2. **`shutdown` falló con `#PF accessing 0xf5ec070`**: se conservó el
   código de los runtime services pero no sus datos, que hasta ahora
   estaban en el mismo saco que el resto de lo reservado.

### Verificación ejecutada

- Host: 201 pruebas en verde.
- Prueba negativa: leer `0x10_0000` da `#PF accessing 0x100000,
  error_code=0x0`. La mitad baja está de verdad desmapeada.
- QEMU: la ventana cuesta 5 tablas; abajo quedan 5 rangos del firmware en 7
  tablas. `boot-test --repeat 10` 10/10, con 512 MiB 1/1, soak de 120 s
  PASS, `shutdown` apaga, `reboot` rearranca, la pantalla sigue dibujando
  (captura con `version` y `help`).
- Mutación: 6. Cinco detectadas; la sexta —enlazar la ventana antes de
  construirla— ni siquiera compila, que es mejor garantía.

### Riesgos y límites

- **Los runtime services del firmware siguen en la mitad baja**, que será
  espacio de usuario. Con el shell dentro del kernel no hay conflicto; en
  cuanto haya procesos, `reboot` y `shutdown` tendrán que llamarse con el
  CR3 del kernel. Es el cabo suelto que hereda el incremento siguiente.
- La ventana cubre 4 GiB fijos, como el mapa de identidad que sustituye.
  Más RAM que eso seguiría sin ser alcanzable, igual que antes.

## Incremento 16 — El kernel corre desde la mitad alta

`docs/adr/0012-fase3-higher-half-kernel.md`. Hasta ahora el kernel se
ejecutaba donde el firmware dejó su imagen, en la mitad baja, por el mapa de
identidad. Fase 3 necesita esa mitad entera para los procesos.

### Qué hace

- `hal::pe` aprende a leer la **tabla de relocalizaciones** (`.reloc`):
  otra vez código puro y probado en host, que recorre la tabla completa
  antes de dar nada por bueno y rechaza los tipos de relocalización que no
  sabe aplicar en vez de saltárselos.
- `kernel::memory::higher_half` hace la mudanza en dos pasos y en este
  orden: **mapea el alias** de la imagen en PML4 259 —mismos marcos, mismos
  permisos que el ADR 0011: código ejecutable y de solo lectura, el resto no
  ejecutable— y después **aplica las relocalizaciones**. Al revés no
  funciona: en cuanto se relocaliza, todos los punteros absolutos de los
  datos nombran el alias, y si no estuviera mapeado el siguiente uso
  fallaría.
- `kmain` salta al gemelo de su propia función en el alias y sigue ahí.
- `arch::interrupts::reinstall_descriptors` vuelve a cargar GDT, TSS e IDT
  leyendo las direcciones que tienen ahora. Guardan direcciones absolutas
  escritas en tiempo de ejecución, que ninguna relocalización toca.

### Verificación ejecutada

- Host: 196 pruebas en verde (192 + 4 nuevas de la mudanza y las
  relocalizaciones).
- QEMU, lo que demuestra que funcionó: `kernel moved into kernel space:
  code at 0xffff8180000097e0 (was 0xddc67e0), 85 page(s) mapped, 61
  read-only, 731 address(es) relocated`. Con 512 MiB de RAM la imagen se
  carga en `0x1ddc67e0` y **el alias es el mismo**: el kernel ya no depende
  de dónde lo pongan.
- Las interrupciones siguen entrando: la autoprueba `int3` aparece dos veces
  en el registro (una por `init`, otra por `reinstall_descriptors`) y el
  temporizador sigue contando después del salto.
- Prueba negativa del camino delicado: desbordar la pila del kernel después
  de la mudanza sigue dando un double fault legible en la pila IST
  (`#DF DOUBLE FAULT on the stack at 0xffff810000015f3f`), lo que prueba que
  el TSS recargado apunta a donde debe.
- `boot-test --repeat 10` 10/10, soak de 120 s PASS, `shutdown` apaga,
  `reboot` rearranca, `fmt-lint` limpio.
- Mutación: 7. Cinco detectadas a la primera; dos sobrevivieron y
  destaparon pruebas flojas —una usaba un tamaño de bloque impar, que
  rechazaba otra condición, y otra no distinguía la guarda de tamaños
  impares— . Reforzadas, las 7 detectadas.

### Riesgos y límites

- **Lo escrito en tiempo de ejecución sigue apuntando abajo**: el puntero
  que `log::set_logger` guardó, y las vtables de los objetos `dyn` del
  contexto, se escribieron antes de la mudanza y nombran la imagen en la
  mitad baja. Funciona porque la mitad baja sigue mapeada; el Incremento 17,
  que la libera, tiene que rehacerlos.
- La mitad baja sigue mapeada entera, con el código del firmware ejecutable
  y escribible.
- La imagen ocupa ahora dos direcciones. Los marcos son los mismos y nunca
  fueron asignables, así que no hay memoria duplicada.
