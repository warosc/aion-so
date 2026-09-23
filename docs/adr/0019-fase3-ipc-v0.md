# ADR 0019: Paso de mensajes, versión 0

## Contexto

Hay dos procesos con espacio propio turnándose la CPU (ADR 0017 y ADR
0018). La salida de Fase 3 pide que **se comuniquen**, y comunicarse es lo
primero que obliga a decidir qué es un proceso para otro: si puede
alcanzarlo, con qué nombre, y quién copia los bytes.

La respuesta gobierna el aislamiento entero. Dos procesos que comparten
una página se comunican sin que el kernel se entere, y a partir de ahí el
kernel ya no sabe quién puede leer qué.

## Decisión

1. **Mensajes copiados por el kernel, no memoria compartida.** Los bytes
   cruzan de un espacio a otro porque el kernel los copia; ninguna página
   de un proceso aparece nunca en las tablas de otro. Es la decisión que
   hace el aislamiento demostrable: quitar el buzón no deja ningún camino
   residual entre los dos.
2. **Cada copia ocurre con el CR3 de su dueño activo.** `send` copia de la
   memoria del remitente al buzón mientras corre el remitente; `recv`
   copia del buzón a la memoria del receptor mientras corre el receptor.
   El kernel nunca necesita leer un espacio que no es el que está activo,
   que es justo lo que no sabría hacer sin una ventana y no debería
   aprender para esto.
3. **Un buzón por proceso, de un mensaje, de tamaño fijo** (64 bytes), en
   memoria del kernel —dentro de la ranura del planificador, en la mitad
   alta, alcanzable con cualquier CR3—. Sin colas y sin asignación
   dinámica: un buzón se llena dentro de un manejador de syscall con las
   interrupciones desactivadas, y ahí no se pide memoria.
4. **El destino es el número de ranura** del planificador. v0 no tiene
   nombres ni capacidades: cualquier proceso puede escribir en el buzón de
   cualquiera. Es un límite, no un descuido; el sitio donde vivirán los
   permisos es este ADR cuando haya más de un programa (Fase 4).
5. **`send` no bloquea.** Si el buzón del destino ya tiene un mensaje
   devuelve `-4`, y el remitente decide: el programa de la demostración
   cede la CPU y reintenta. Bloquear al remitente exigiría una cola de
   espera por buzón, que con dos procesos sería adorno.
6. **`recv` bloquea.** Si el buzón está vacío el proceso pasa a
   `Blocked`, deja de recibir turnos y vuelve, dentro de la misma syscall,
   cuando alguien le escribe. Es el encuentro: sin bloqueo el receptor
   tendría que girar en vacío quemando turnos.
7. **El kernel no deja a nadie esperando para siempre.** `recv` mira,
   antes de bloquear, si queda alguien que pueda correr; si no queda,
   devuelve `-6` en vez de colgar la máquina. Y cuando el kernel recupera
   la CPU con procesos todavía bloqueados, los da por muertos y libera su
   memoria, diciéndolo en el registro.
8. **El receptor sabe quién le escribió**: `recv` devuelve la longitud en
   `RAX` y la ranura del remitente en `RDX`. Un mensaje del que no se sabe
   el origen no es un mensaje, es un ruido; y el kernel ya lo sabe.
9. **Los punteros se validan como los de `log`** (ADR 0014, punto 9):
   dentro de la memoria del proceso, sin desbordar, y con `len` acotado
   por la capacidad del buzón. `recv` además **escribe** en memoria del
   usuario, que es la primera vez que el kernel lo hace: el rango se
   comprueba entero antes del primer byte.
10. **Dos syscalls nuevas**, `3 = send(to, ptr, len)` y
    `4 = recv(ptr, cap)`, y tres errores nuevos: `-4` buzón ocupado, `-5`
    no hay tal proceso, `-6` nadie podría escribir nunca. Sigue siendo v0:
    el ABI no está congelado (ADR 0014, punto 8).

## Alternativas consideradas

- **Memoria compartida entre los dos procesos**: es más rápida y es lo que
  querrá un día el subsistema de IA, pero no se puede demostrar
  aislamiento con ella y habría que rehacer el modelo de permisos antes de
  tener uno.
- **Buzón en memoria del proceso** en vez del kernel: ahorra la copia de
  salida, y obliga al kernel a escribir en el espacio del receptor
  mientras corre el remitente —exactamente lo que el punto 2 evita—.
- **Colas de mensajes** en vez de uno: hace falta cuando haya más de dos
  procesos y trabajo real; hoy solo añadiría un asignador dentro de un
  manejador.
- **`send` bloqueante**, con cola de espera por buzón: es lo que hará
  falta con varios remitentes; con dos procesos es una lista de un
  elemento y un estado más que mantener correcto.
- **Puertos con nombre o capacidades desde ya**: es la forma correcta, y
  pide un espacio de nombres, un registro y una política de creación. Nada
  de eso se puede diseñar bien con un único programa incrustado.

## Consecuencias

- El planificador gana un estado, `Blocked`, y con él la primera forma de
  que un proceso exista y no pueda correr. `next_alive` lo salta; `alive`
  lo cuenta.
- Una syscall ya podía volver más tarde (ADR 0018); ahora puede volver
  **mucho** más tarde, y en medio ha corrido otro proceso. El manejador de
  `recv` está escrito como un bucle que vuelve a mirar el buzón al
  despertar, porque lo que vio antes de bloquear ya no vale.
- El kernel escribe en memoria de usuario por primera vez. La validación
  de rangos deja de ser una precaución de lectura.
- 64 bytes por proceso de estado nuevo, dentro de la ranura, sin
  asignación.
- Un proceso puede llenar el buzón de otro y dejarlo ahí: es una
  denegación de servicio entre iguales que v0 no evita, y que las
  capacidades del punto 4 resolverán.
