# ADR 0020: Una falta en ring 3 es del proceso, no de la máquina

## Contexto

Dos procesos aislados se comunican (ADR 0019). Falta la otra mitad de lo
que "aislado" significa: que uno pueda portarse mal sin arrastrar a nadie.

Hoy no es así. Toda excepción —`#PF`, `#GP`, la que sea— termina en
`halt()`. Un programa de usuario que lea una dirección que no es suya para
la máquina entera, con el kernel y los demás procesos dentro. Eso no es
aislamiento; es un único fallo compartido por todos.

Y hay un segundo agujero del mismo tipo: de los 32 vectores de excepción
que define la CPU, solo seis tienen puerta. Los demás no existen en la
IDT, así que alcanzarlos produce un `#GP` cuyo código de error nombra un
selector de segmento que no tiene nada que ver con lo ocurrido. Mientras
solo corría el kernel eso era un detalle; un proceso puede provocar la
mitad de ellos a propósito y `ud2` ocupa dos bytes.

## Decisión

1. **Quién causó la falta lo dice la CPU, no el kernel.** Los dos bits
   bajos del `CS` que la interrupción apiló son el nivel de privilegio en
   que se estaba ejecutando. `CS & 3 == 3` es ring 3 y nada más lo es.
2. **Si fue ring 3, el proceso termina y la CPU pasa al siguiente.** El
   manejador de excepciones llama a lo mismo que `exit`: marcar la ranura
   muerta y cambiar de contexto. No vuelve nunca, y el marco que la
   excepción dejó en la pila de kernel de ese proceso se abandona con
   ella; nadie la reanuda, y su memoria vuelve con la del resto.
3. **Si fue ring 0, la máquina se para donde está.** Una falta dentro del
   kernel no tiene a quién echarle la culpa, y seguir adelante sobre lo
   que acaba de salir mal es peor que detenerse.
4. **`arch` no sabe qué es un proceso.** La excepción se entrega por
   puntero a función, como el reloj: `set_user_fault_handler`. Hasta que
   el kernel lo instala, una falta en ring 3 para la máquina, igual que
   antes; el arranque temprano no tiene planificador al que recurrir.
5. **Los 32 vectores tienen puerta**, y el despachador los nombra. Un
   proceso no debe poder alcanzar un vector sin puerta.
6. **Qué vectores llevan código de error se escribe dos veces**, en
   sitios distintos: en la lista que genera los stubs y en una función que
   copia la tabla 6-1 del SDM. Una prueba las compara. Equivocarse ahí
   desplaza ocho bytes todo el marco, y lo que se lee como `rip` es lo que
   había antes.
7. **Un `#BP` desde ring 3 también mata al proceso.** El kernel rompe a
   propósito en su autoprueba y sigue; un proceso no tiene depurador con
   quien hablar.
8. **El NMI no es culpa de nadie**: sigue registrándose y volviendo, venga
   de donde venga.

## Alternativas consideradas

- **Señales, como en Unix**: dejar que el proceso decida qué hacer con su
  propia falta. Hace falta un formato de marco, una pila alternativa y un
  camino de vuelta a ring 3 desde el manejador. Nada de eso se puede
  diseñar bien con programas escritos a mano en hexadecimal.
- **Reintentar la instrucción** después de mapear lo que faltaba: es lo
  que hará falta el día que haya memoria bajo demanda, y hoy no hay nada
  que mapear.
- **Matar al proceso desde el kernel "después"**, dejando la excepción
  volver por `iretq` a una dirección de parada: añade un camino de vuelta
  a ring 3 para un proceso que ya está condenado.
- **Dejarlo como estaba** hasta Fase 4: significa que la salida de Fase 3
  se demuestra con procesos que no pueden equivocarse, que es demostrar
  otra cosa.

## Consecuencias

- El kernel gana su primer camino en el que el estado del planificador se
  toca desde el camino de interrupción y no desde una syscall. Está
  permitido por la misma razón: un núcleo, interrupciones desactivadas, y
  el proceso que falla no puede estar a la vez dentro de una syscall.
- Una falta se registra dos veces: la línea de `arch`, que dice lo que vio
  la CPU, y la del kernel, que dice de quién era. Son dos hechos
  distintos y el segundo no se puede deducir del primero.
- Las tablas de páginas del proceso muerto siguen sin liberarse, igual que
  con `exit` (ADR 0018, punto 7).
- Un proceso puede terminarse a sí mismo provocando una falta en vez de
  llamando a `exit`. Da igual: el kernel hace lo mismo con los dos.
