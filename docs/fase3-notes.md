# Notas de Fase 3 — Procesos y aislamiento

Lo más reciente arriba. Salida de la fase (ROADMAP.md): dos procesos
aislados se comunican sin compartir memoria no autorizada.

Decisiones de alcance tomadas al abrir la fase: primero la mudanza a la
mitad alta, binario plano incrustado para el primer programa de usuario, y
`syscall`/`sysret` en vez de `int 0x80`.

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
