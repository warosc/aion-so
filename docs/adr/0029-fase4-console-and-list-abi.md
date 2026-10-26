# ADR 0029: La consola y el directorio, desde ring 3

## Contexto

El ADR 0028 dio a un programa `open`, `read`, `close` y `write_file`, y dejó
dicho explícitamente que **`list` no estaba** porque "es una decisión con sus
propios casos raros". Este es el ADR de esos casos raros.

Hace falta ahora porque el bullet de Fase 4 que queda es *shell de usuario*, y
la shell de hoy corre en el kernel. Para moverla a ring 3 le faltan tres cosas,
no una:

1. **Listar el directorio.** `ls` es lo primero que hace una shell.
2. **Escribir en la consola.** `log` (syscall 0) escribe en el registro del
   kernel, que sale por debugcon. El prompt y la respuesta de un comando tienen
   que salir por el framebuffer, que es otro dispositivo. No son el mismo canal
   —el ADR 0001 ya lo dejó anotado para el marcador de la shell— y confundirlos
   sería imprimir el prompt donde nadie lo ve.
3. **Leer una tecla.** La shell lee carácter a carácter, porque tiene que
   manejar el retroceso y hacer eco. Eso no se puede hacer con una llamada que
   dé líneas.

Y hay una decisión de fondo que hay que tomar antes: **la consola es del
kernel**. La recibe de `boot` como `&mut dyn Console` y la usa desde ring 0.
Prestarla a ring 3 es lo mismo que se hizo con el disco en el ADR 0028, y por
las mismas razones.

## Decisión

### Las tres llamadas

1. **Tres números, tras los ocho que hay** (ADR 0014 punto 7, ADR 0019,
   ADR 0028 punto 3):

   | nº | llamada | argumentos | resultado |
   |----|---------|-----------|-----------|
   | 9 | `list` | `RDI` = índice, `RSI` = buffer, `RDX` = longitud | bytes escritos |
   | 10 | `console_write` | `RDI` = texto, `RSI` = longitud | bytes escritos |
   | 11 | `console_read` | — | una tecla, o 0 si no hay |

### `list`: una entrada por llamada, por índice

2. **Una llamada da **una** entrada, la que ocupa la posición `índice`.** No
   se rellena un buffer con todas las que caben.

   La alternativa —el kernel escribe tantas como quepan y dice cuántas
   escribió— obliga al programa a decir cuántas caben y al kernel a decir
   cuántas puso, y sigue sin resolver lo que parece resolver: un directorio
   que cambia entre dos llamadas da una vista a medias de todas formas. Con
   una entrada por llamada, **cada llamada es verdad por separado**, que es lo
   máximo que se puede prometer sin bloquear el volumen entero mientras alguien
   lista.

3. **El coste se nombra: recorrer el directorio una vez por entrada.** Listar
   *n* ficheros son *n* recorridos. Para un directorio raíz de media docena de
   ficheros no se mide, y es honesto sobre lo que FAT es: un directorio sin
   índice, donde llegar a la entrada *n* es pasar por las *n-1* anteriores.

4. **Un índice pasado el final es `ERR_NO_SUCH_FILE`, no cero.** Cero bytes
   escritos sería ambiguo con una entrada vacía, y un bucle que para en el
   primer error es el mismo bucle que para en el primer cero sin la ambigüedad.

5. **El registro es de 20 bytes y es parte del ABI**:

   | offset | bytes | qué |
   |--------|-------|-----|
   | 0 | 12 | el nombre 8.3 como texto, relleno con espacios a la derecha |
   | 12 | 1 | los atributos, como los guarda FAT (bit 4 = directorio) |
   | 13 | 3 | reservado, a cero |
   | 16 | 4 | el tamaño, little-endian |

   Fijo y con el tamaño alineado a cuatro, para que un programa lo lea sin
   aritmética. Los atributos van tal como FAT los guarda en vez de traducidos
   a banderas propias: traducirlos sería inventar un vocabulario que habría que
   mantener, y el bit que importa —directorio— es uno.

   Un buffer de menos de 20 bytes es `ERR_BAD_ARGUMENT`. Escribir una entrada
   a medias sería entregar un nombre sin su tamaño.

6. **Solo el directorio raíz**, como el resto del ADR 0028. No hay rutas y un
   nombre con barra se rechaza (ADR 0028, punto 10), así que no hay otro
   directorio que listar.

### `console_write`: lo que se ve, separado de lo que se registra

7. **Escribe en la consola, no en el registro del kernel.** `log` sigue
   existiendo y sigue yendo a debugcon: son dos canales y un programa elige.
   Una shell usa `console_write` para lo que el usuario lee y `log` para lo que
   quede en el registro de arranque.
8. **Tiene el mismo límite que `log`**, 4096 bytes, y el mismo requisito: el
   texto tiene que ser memoria del proceso y tiene que ser UTF-8. Lo segundo
   porque la consola escribe texto, y un programa que entregue bytes que no lo
   son recibe `ERR_BAD_ARGUMENT` en vez de dibujar basura.
9. **No hay `clear` ni cursor ni color.** Una shell que borre la pantalla puede
   esperar; cada una de esas es una decisión sobre qué es una consola, y este
   ADR solo necesita que el texto salga.

### `console_read`: no bloquea, y por eso

10. **Devuelve una tecla o cero.** Cero significa *no hay ninguna todavía*, no
    *fin*.
11. **No bloquea, y no es una comodidad: es obligatorio.** El manejador corre
    con las interrupciones desactivadas (ADR 0014, punto 2), y la tecla llega
    por la IRQ 1 del teclado. Una llamada que esperase dentro del manejador
    esperaría a una interrupción que no puede llegar, con el reloj parado: un
    bloqueo del sistema entero, no de un proceso.
12. **El programa que quiere esperar hace `yield` y vuelve a preguntar.** Es
    exactamente lo que la shell del kernel hace ya con `hlt`. El coste es que
    con un solo proceso ejecutable eso es una espera activa que consume CPU;
    se acepta y se nombra, y desaparece cuando el scheduler pueda dormir a un
    proceso hasta que haya una tecla, que es trabajo de Fase 5 junto con la E/S
    que bloquea (ADR 0028, última consecuencia).
13. **La codificación es pequeña y cerrada**, sobre `ConsoleKey`:

    | valor | qué |
    |-------|-----|
    | 0 | no hay tecla |
    | 1 | Enter |
    | 2 | Backspace |
    | 3 | una tecla que este kernel no nombra |
    | `0x20`–`0x7E` | ese carácter ASCII imprimible |

    Los números bajos para lo que no es un carácter, y el carácter como su
    propio código: un programa compara con `b' '` y con `b'~'` y ya sabe. Lo
    que no sea ASCII imprimible se da como `3` en vez de como su punto de
    código, porque la shell no lo va a usar y prometer Unicode sin tener dónde
    dibujarlo es prometer de más.

### Lo que llega de ring 3 sigue sin creerse

14. **Las mismas reglas del ADR 0028, punto 9.** El buffer de `list` se
    **rellena**, así que tiene que ser memoria que el proceso pueda escribir, no
    solo suya: `owns_writable`. El texto de `console_write` solo se lee, así
    que basta con que sea suyo.
15. **Un error nuevo y uno reutilizado.** `list` pasado del final y
    `console_write` con bytes que no son texto no necesitan números nuevos:
    `ERR_NO_SUCH_FILE` y `ERR_BAD_ARGUMENT` ya dicen lo que pasó.

### La consola pasa a ser compartida

16. **Tras un `IrqLock`, como el disco** (ADR 0028). Es el segundo dispositivo
    que este kernel comparte entre contextos y el segundo que necesita cerrojo.
17. **El kernel la toma prestada, no la posee.** `boot` la construye y se la
    pasa a `kmain`, que no vuelve nunca. Lo que se guarda es un puntero, con la
    invariante escrita: la consola vive en el marco de `boot`, que está por
    debajo de un `kmain` que no retorna, así que vive mientras viva el kernel.
    La alternativa —pasar la propiedad— obligaría a `boot` a entregar un
    `Box<dyn Console>` antes de que haya heap, o a mover un tipo concreto que
    `kernel` no conoce.
18. **La shell del kernel sigue existiendo.** El kernel tiene que arrancar sin
    disco (CLAUDE.md), y sin disco no hay programa de shell. La del kernel pasa
    a ser el camino de reserva, y el marcador que `boot-test` busca sigue
    siendo suyo para que un arranque sin disco siga siendo comprobable.

## Alternativas consideradas

- **`list` que rellena un buffer con todas las entradas que caben**: una
  llamada en vez de *n*, y el programa tiene que decir cuántas caben, el kernel
  cuántas puso, y la vista sigue pudiendo quedar a medias si el directorio
  cambia. Más ABI para la misma promesa.
- **`list` que devuelve solo el nombre**, sin tamaño ni atributos: no hace
  falta inventar un registro, y entonces `ls` no puede decir de qué tamaño es
  nada y haría falta un `stat` que es otra llamada.
- **`opendir`/`readdir`/`closedir`, con descriptor**: lo que haría un sistema
  de verdad, y lo que resuelve exactamente el caso que el punto 2 dice que no
  se puede resolver —un recorrido consistente— a cambio de un cuarto tipo de
  recurso por proceso y de decidir qué pasa cuando alguien escribe en el
  directorio que otro está recorriendo. Cuando haya subdirectorios.
- **`console_read` que bloquea**: lo que un programa espera, y con las
  interrupciones desactivadas en el manejador es un bloqueo de la máquina. Se
  podría volver a activarlas dentro de la llamada, y eso es rediseñar el
  manejador de syscalls, que es Fase 5.
- **Devolver el punto de código completo en `console_read`**: más general, y
  promete un Unicode que el teclado PS/2 de este kernel no produce y la fuente
  de 8x8 no dibuja.
- **Que `log` escriba en los dos sitios**: una llamada menos, y pierde la
  distinción que el ADR 0001 estableció a propósito. El registro de arranque es
  evidencia; la pantalla es interfaz.

## Consecuencias

- La consola deja de ser exclusiva del kernel. Es el segundo recurso
  compartido tras el volumen, y las llamadas de consola son las primeras que
  pueden esperar por él.
- **Una shell en ring 3 es una espera activa** mientras no haya nada más
  ejecutable, porque `console_read` no bloquea y la espera se hace con `yield`.
  Medible en el soak; la alternativa necesita que el scheduler sepa dormir a
  un proceso hasta que llegue una tecla, que es Fase 5.
- El registro de 20 bytes de `list` es ABI. Cambiarlo es cambiar el ABI, y por
  tanto subir este ADR.
- v0 sigue sin ser estable (ADR 0014, punto 8). Once llamadas, y el primer
  programa de fuera congelará el contrato.
