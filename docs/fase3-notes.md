# Notas de Fase 3 — Procesos y aislamiento

Lo más reciente arriba. Salida de la fase (ROADMAP.md): dos procesos
aislados se comunican sin compartir memoria no autorizada.

Decisiones de alcance tomadas al abrir la fase: primero la mudanza a la
mitad alta, binario plano incrustado para el primer programa de usuario, y
`syscall`/`sysret` en vez de `int 0x80`.

## Incremento 23 — IPC mínimo: un mensaje cruza de un espacio a otro

`docs/adr/0019-fase3-ipc-v0.md`. Hay cuatro procesos a la vez; dos se
turnan diciendo quiénes son, y los otros dos se pasan un mensaje.

### Qué hace

- **El kernel copia, nadie comparte.** Cada proceso tiene un buzón de un
  mensaje y 64 bytes en memoria del kernel, dentro de su ranura del
  planificador. Ninguna página de un proceso aparece nunca en las tablas de
  otro: los bytes cruzan porque el kernel los copia, y por ningún otro
  camino.
- **Cada copia ocurre con el CR3 de su dueño activo**: `send` copia de la
  memoria del remitente al buzón mientras corre el remitente, y `recv` del
  buzón a la memoria del receptor mientras corre el receptor. El kernel no
  necesita leer un espacio que no sea el activo, y es la primera vez que
  **escribe** en memoria de usuario: el rango se comprueba entero antes del
  primer byte.
- **`recv` bloquea y `send` no.** Un receptor sin mensaje pasa a `Blocked`,
  deja de recibir turnos y vuelve, dentro de la misma syscall, cuando
  alguien le escribe. Un remitente que encuentra el buzón ocupado recibe
  `-4` y decide; el programa de la demostración cede la CPU y reintenta.
- **Nadie espera para siempre.** `recv` mira antes de bloquear si queda
  alguien que pueda correr, y si no queda devuelve `-6`. Y si el kernel
  recupera la CPU con alguien todavía bloqueado, lo dice y lo da por
  muerto, para que su memoria vuelva con la del resto.
- **El receptor sabe quién le escribió**: `recv` devuelve la longitud en
  `RAX` y la ranura del remitente en `RDX`.
- **"Quién corre" tiene una sola respuesta.** El manejador de syscalls
  guardaba su propia copia del proceso actual, escrita al entrar en ring 3
  y nunca al cambiar de proceso: desde el segundo cambio comprobaba los
  punteros de uno contra la memoria de otro. Funcionaba solo porque los dos
  procesos tenían los mismos rangos. Ahora se la pide al planificador, que
  es quien lo sabe.
- **Tres programas escritos a mano**: los dos que hablan (de antes), un
  remitente con su bucle de reintento y un receptor que pide el mensaje en
  su **pila** —su página de código es de solo lectura, el kernel no podría
  escribir ahí— y sale con la ranura de quien le escribió como código de
  salida.

### Verificación ejecutada

- Host: 236 pruebas en verde (201 + 35 nuevas: el buzón puro, las
  transiciones de estado, el orden con un proceso bloqueado, los bytes de
  los dos programas nuevos y lo que el manejador rechaza).
- QEMU, lo que demuestra que funcionó: el receptor entra en ring 3, se
  queda sin registrar nada —está bloqueado— y solo después de
  `55 byte(s) from slot 3 are waiting in the mailbox of slot 2` aparece
  `slot 2 took 55 byte(s) sent by slot 3` y, desde ring 3,
  `HARLAN: this crossed from one address space to another`. Ese texto lo
  escribió un proceso cuyo espacio está en `0x5a2000` y lo leyó otro cuyo
  espacio está en `0x598000`.
- `the process in slot 2 exited with 3`: el receptor sale con la ranura del
  remitente, que es la única forma de ver desde fuera del kernel que `RDX`
  llevó el remitente de vuelta a ring 3 a través de `sysret`.
- `0x400000 is 0x585000 in one process and 0x5a3000 in another`: la misma
  dirección sigue siendo memoria distinta en cada uno.
- `24 frame(s) back from the processes that exited; 62758 free`: seis
  marcos por proceso —código, pila de usuario y cuatro páginas de pila de
  kernel— vuelven al asignador.
- Prueba negativa 1, un receptor solo: `slot 0 is waiting for a message
  nobody could send`, la syscall devuelve `-6`, el programa sigue y sale.
  La máquina no se cuelga.
- Prueba negativa 2, un receptor y alguien más que no le escribirá: el
  receptor se bloquea, el otro proceso acaba, y el kernel recupera la CPU
  con alguien todavía esperando: `the process in slot 0 is still waiting
  for a message that will not come`, y sus marcos vuelven con los del otro
  (`12 frame(s) back`). Es el único camino que ejercita ese rescate.
- Prueba negativa 3, dos remitentes y un buzón: `the mailbox of slot 2
  still holds a message, so slot 1 was told to wait`, el remitente cede el
  turno, reintenta cuando le toca y para entonces el receptor ya no está:
  `syscall send() names slot 2, where there is no process`. Los tres
  caminos de `send` —entregado, ocupado, nadie— en un arranque.
- `boot-test --repeat 10` 10/10, soak de 120 s PASS, `fmt-lint` limpio.
- Mutación: 12, las 12 detectadas a la primera. Dos de ellas obligaron a
  mover lógica a donde una prueba de host la alcanza: las transiciones
  `waiting()` y `woken()` vivían dentro de métodos que necesitan una ranura
  con un proceso dentro, que en host no se puede construir.

### Riesgos y límites

- **Sin permisos**: cualquier proceso puede escribir en el buzón de
  cualquiera, nombrándolo por su número de ranura. Un proceso puede llenar
  el buzón de otro y dejarlo ahí. Es la denegación de servicio entre
  iguales que el ADR 0019 deja dicha y que resolverán las capacidades.
- **Un mensaje por buzón y 64 bytes**: suficiente para la salida de fase, y
  lo primero que se queda corto con trabajo real.
- **El remitente lleva la ranura del destino escrita por el kernel que lo
  arranca**: v0 no tiene forma de que un programa pregunte quién hay.
- Las tablas de páginas de un proceso muerto siguen sin liberarse (cinco
  marcos por proceso), como en el Incremento 22.

## Incremento 22 — El scheduler: dos procesos turnándose

`docs/adr/0018-fase3-context-switch.md`. Ya hay dos procesos con espacio
propio, y ahora existen a la vez y se pasan la CPU.

### Qué hace

- **El estado de un proceso vive en su propia pila de kernel**, que es
  donde ya lo dejaban el trampolín de la IDT y el stub de `syscall`. Cada
  proceso tiene una, con guard pages.
- **`switch`**: diez instrucciones que apilan los registros que la ABI
  obliga a conservar, guardan `rsp` en el que sale, cargan el del que
  entra, escriben su CR3 y vuelven. Quien vuelve es el otro proceso.
- **Un proceso nuevo tiene la pila preparada** para que ese primer retorno
  caiga en un trampolín que entra en ring 3: no hay dos caminos, arrancar
  y reanudar, solo uno.
- **Round robin**, con dos formas de ceder: el temporizador y la syscall
  `yield`. La segunda hace la prueba determinista.
- **`TSS.rsp0` y la pila que usa `syscall` se actualizan en cada cambio**,
  porque son del proceso que entra.
- **Un proceso que sale devuelve su memoria de usuario y su pila de
  kernel**: 12 marcos de los dos, medido. Sus tablas, no —queda dicho.

### Lo que la máquina enseñó, y es lo mejor del incremento

**`GS` no sobrevive a un cambio de contexto.** El stub de `syscall` usaba
`swapgs` y `KERNEL_GS_BASE` para encontrar la pila del kernel, como hace un
kernel multinúcleo. En cuanto los procesos pudieron turnarse, eso se rompió:
un proceso entra al kernel por syscall —que hace `swapgs`— y **sale por el
`iretq` del temporizador, que no lo deshace**. El siguiente `swapgs` deja
`GS` con el valor del usuario, y el stub escribe a través de él:
`#PF accessing 0x8, error_code=0x2`.

Con un solo núcleo, `GS` no aportaba nada: la dirección se lee de un
estático, RIP-relativo, y el puntero de pila del usuario se guarda **en la
pila del propio proceso**, no en un global —porque un global lo sobrescribe
quien corra mientras ese proceso está aparcado a mitad de syscall—.
Recuperar `swapgs` significa enseñarle al camino de interrupción a hacerlo
también, y eso va con el resto del trabajo multinúcleo.

Y una segunda, pequeña: la primera corrida imprimió **una línea de registro
partida en dos**, porque la preempción cayó en medio. El sumidero escribe
byte a byte por un puerto, así que ahora una línea sale entera con las
interrupciones desactivadas.

### Verificación ejecutada

- Host: 220 pruebas en verde.
- QEMU, la alternancia completa: proceso 1 habla y cede → entra el 2 y
  habla → vuelve el 1, habla otra vez y sale con 7 → sigue el 2 y sale →
  `every process has exited; the kernel has the CPU back` → `12 frame(s)
  back from the processes that exited` → shell.
- `boot-test --repeat 10` 10/10, soak de 120 s PASS, `shutdown` apaga,
  `reboot` rearranca, `fmt-lint` limpio.
- Mutación: 7. Dos detectadas a la primera; **cinco sobrevivieron** porque
  el orden del turno, la guarda del tick y el marco que prepara la pila no
  tenían prueba en host. Escritas —incluida una que lee el marco slot a
  slot—, las 7 detectadas. Una de esas pruebas era además tautológica: ataba
  el tamaño del marco a su propia constante; ahora lo ata a la posición del
  último registro.

### Riesgos y límites

- **Una syscall ya no puede dar por hecho que vuelve al mismo proceso.** Si
  cede, vuelve más tarde y en otra pila.
- Las tablas de un proceso muerto no se recuperan: cinco marcos por
  proceso. El mapper todavía no sabe qué tablas son de quién.
- Sin prioridades y sin dormir: un proceso que no hace nada sigue
  gastando su turno.
- `swapgs` volverá con SMP, y entonces el camino de interrupción tendrá que
  cambiar también.

## Incremento 21 — El proceso como objeto

`docs/adr/0017-fase3-process-address-space.md`. Hasta ahora el programa
corría dentro de las tablas del kernel, separado por un bit por página. Eso
lo mantiene fuera del kernel y no dice nada de un segundo programa.

### Qué hace

- **Un PML4 por proceso**: marco propio, mitad baja vacía y las entradas de
  la mitad alta **copiadas del kernel**. Copiar la entrada, no el subárbol:
  los dos roots apuntan a las mismas tablas, así que lo que el kernel mapee
  después está en todos los espacios sin sincronizar nada.
- **Cambiar de espacio es escribir CR3**, y funciona porque el kernel corre
  en la mitad alta, que es idéntica en todos.
- **`Process`**: su espacio, sus rangos, su entrada y su pila. Lo que el
  manejador de syscalls valida deja de ser una global y pasa a ser la
  memoria del proceso que llamó.
- El arranque crea **dos** procesos, con el mismo programa y las mismas
  direcciones, y al salir el primero el kernel vuelve a su propio espacio.

### La demostración

Dos cosas, y la segunda es la que convence:

1. El kernel comprueba en las tablas que la misma dirección da marcos
   distintos: `0x400000 is 0x581000 in one process and 0x587000 in the
   other: different memory, same address`.
2. **Prueba negativa de extremo a extremo**: con una sonda temporal se
   sobrescribió el mensaje del **segundo** proceso en su propio marco. El
   primero, corriendo en **la misma dirección virtual**, siguió imprimiendo
   el suyo: `from ring 3: HARLAN: hello from ring 3`. Si compartieran
   memoria habría dicho el del segundo.

### Verificación ejecutada

- Host: 219 pruebas en verde.
- QEMU: los dos espacios se crean (`0x580000` y `0x586000`), uno corre,
  sale con 7, y el kernel vuelve a su espacio y llega al shell.
- `boot-test --repeat 10` 10/10, soak de 120 s PASS, `shutdown` apaga,
  `reboot` rearranca, `fmt-lint` limpio.
- Mutación: 4, las 4 detectadas —incluidas "un espacio nuevo hereda también
  la mitad baja" y "un puntero de ring 3 se toma por bueno".

### Riesgos y límites

- **No hay planificador**: el kernel arranca uno, el proceso sale, el
  kernel sigue. El segundo espacio se crea para demostrar el aislamiento y
  nunca corre. Guardar y restaurar el estado de un proceso interrumpido es
  el Incremento 22.
- La pila de syscalls sigue siendo única: vale con un proceso y con
  syscalls que no se interrumpen.
- Los procesos comparten las tablas de la mitad alta del kernel. Lo que
  sostiene el aislamiento es que el mapper **rechaza** mezclar páginas de
  usuario y de kernel (Incremento 18); si eso se rompiera, se rompería para
  todos a la vez.
- Un proceso que sale no devuelve sus marcos: no hay destrucción todavía.

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
