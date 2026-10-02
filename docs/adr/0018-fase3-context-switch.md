# ADR 0018: Cambio de contexto y planificación

## Contexto

Hay procesos con espacio propio (ADR 0017), pero solo uno llega a correr:
el kernel lo arranca, el proceso sale y el kernel sigue. Para que dos se
comuniquen —la salida de Fase 3— tienen que existir a la vez y turnarse.

Turnarse exige guardar el estado de uno y restaurar el del otro. La
pregunta es **dónde vive ese estado**.

## Decisión

1. **El estado de un proceso vive en su propia pila de kernel.** Cada
   proceso tiene una, con guard pages como las del kernel. Cuando entra al
   kernel —por syscall o por interrupción— sus registros se apilan ahí,
   que es lo que el trampolín de la IDT ya hacía; cambiar de proceso es
   entonces **cambiar de pila**.
2. **`switch` es una función desnuda de diez instrucciones**: apila los
   registros que la ABI obliga a conservar, guarda `rsp` en el proceso que
   sale, carga el `rsp` del que entra, escribe su CR3, desapila y vuelve.
   Quien "vuelve" es el otro proceso, desde el `switch` que hizo la vez
   anterior.
3. **Un proceso que nunca ha corrido tiene una pila preparada** para que
   ese primer retorno caiga en un trampolín que entra en ring 3. Así no hay
   dos caminos —arrancar y reanudar—, solo uno.
4. **Round robin, sin prioridades.** Con dos procesos y un temporizador a
   100 Hz, cualquier otra cosa sería adorno.
5. **Dos formas de ceder la CPU**: el temporizador (expropiación) y la
   syscall `yield` (cooperación). La segunda existe porque hace la prueba
   determinista: sin ella, el registro depende de cuándo caiga el tick.
6. **Lo que la CPU necesita saber de "el proceso actual" se actualiza en
   cada cambio**: `TSS.rsp0` y el puntero por CPU que usa `syscall`, ambos
   a la pila de kernel del proceso que entra. Olvidarlo significa que la
   siguiente interrupción escribe en la pila de otro.
7. **Un proceso que sale libera su memoria de usuario y su pila de
   kernel.** Sus tablas de páginas **no**: el mapper todavía no sabe qué
   tablas pertenecen a qué espacio. Queda dicho aquí y en las notas, con
   nombre: son cinco marcos por proceso muerto.

## Alternativas consideradas

- **Guardar el estado en una estructura del proceso** en vez de en su
  pila: hay que copiar registros dos veces y decidir qué pasa con lo que ya
  apiló el trampolín. La pila es donde el estado ya está.
- **Cambiar de contexto dentro del manejador de interrupción**, reescribiendo
  el marco antes de `iretq`: evita una pila por proceso y mete la
  planificación en el camino más delicado del kernel, donde un error no se
  depura.
- **Solo cooperativo**, sin temporizador: un proceso que no cede cuelga la
  máquina, y el kernel no tendría forma de recuperarla.
- **Solo expropiativo**, sin `yield`: la prueba dependería de dónde caiga
  el tick, que es justo lo que no se quiere en una prueba.

## Consecuencias

- Un proceso cuesta ahora también su pila de kernel: cuatro páginas y dos
  guard pages.
- El kernel deja de ser "lo que corre entre procesos" y pasa a tener un
  estado —quién corre— que hay que mantener correcto en cada cruce.
- **Una syscall ya no puede asumir que vuelve al mismo proceso**: si cede,
  vuelve más tarde, en otra pila. El manejador tiene que estar escrito para
  eso.
- Las interrupciones siguen desactivadas dentro de una syscall (`FMASK`,
  ADR 0014), así que un `yield` no puede ser interrumpido a medias.
- El aislamiento no cambia: cada proceso sigue en su espacio, y lo único
  que comparten es la mitad alta del kernel.
