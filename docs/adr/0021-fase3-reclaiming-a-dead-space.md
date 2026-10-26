# ADR 0021: Recuperar el espacio de un proceso muerto

Sustituye al punto 7 del ADR 0018 y corrige la aritmética del ADR 0017.

## Contexto

Un proceso que termina devolvía su memoria de usuario y su pila de kernel,
pero no sus tablas de páginas. El ADR 0018 lo dejó dicho con nombre —"son
cinco marcos por proceso muerto"— porque el mapeador no sabía qué tablas
pertenecían a qué espacio.

Con la salida de Fase 3 eso dejó de ser una nota al pie: la demostración
arranca ocho procesos y todos mueren. La fuga pasó de teórica a medible, y
crece con cada proceso que el sistema llegue a arrancar.

Y la cifra estaba mal. Medida, es **cuatro** marcos por proceso, no cinco:
el código en 4 MiB y la pila en 5 MiB caen en la misma entrada del
directorio —que cubre 2 MiB— así que comparten tabla de páginas. Raíz,
puntero de directorios, directorio y tabla: cuatro. Los ADR 0017 y 0018
contaron una tabla por región mapeada sin mirar la granularidad.

## Decisión

1. **Un espacio muerto devuelve sus tablas.** `AddressSpace::destroy`
   recorre la mitad baja de su propio árbol y ofrece cada marco de tabla
   al asignador, la raíz incluida.
2. **Solo la mitad baja.** Las entradas en y por encima de
   `KERNEL_SPACE_BASE` apuntan a las tablas del kernel, copiadas por
   referencia (ADR 0017): liberar una desmapearía el kernel de todos los
   demás espacios, incluido el que la CPU está recorriendo. El recorrido se
   detiene en el índice del PML4 que empieza la mitad alta.
3. **Una entrada hoja no es una tabla.** Lo que apunta una entrada de
   página grande —2 MiB o 1 GiB— es memoria de alguien, y el recorrido no
   baja de ahí. Lo mismo con las entradas de la tabla de páginas: esas son
   la memoria del proceso, que se devuelve por su propio camino.
4. **Quién decide si un marco era una tabla es el asignador, no el
   recorrido.** `destroy` recibe una función que devuelve si aceptó el
   marco, y el kernel le pasa `deallocate_as(frame, PageTable)`. Un marco
   etiquetado de otra cosa se rechaza, así que la memoria del proceso no
   puede volver dos veces aunque el recorrido se equivocara. Es la
   propiedad por marco del ADR 0004 haciendo de red.
5. **Se cuenta lo que volvió de verdad**, no lo que se ofreció. Un marco
   rechazado no suma, para que el número del registro no mienta.
6. **Las entradas de la mitad baja de la raíz se ponen a cero al pasar**,
   de modo que un marco reutilizado no pueda recorrerse como un árbol de
   tablas desde una raíz que alguien conservara.
7. **El arranque comprueba que no queda nada.** El kernel anota cuántos
   marcos libres había antes de existir ningún proceso y, cuando todos han
   muerto, compara: si no coincide dice cuántos siguen retenidos. La fuga
   que este ADR cierra habría sido visible desde el primer día con esa
   línea.

## Alternativas consideradas

- **Un recuento de referencias por tabla**: es lo que hará falta el día
  que dos espacios compartan algo de la mitad baja —memoria compartida,
  copy-on-write—. Hoy no comparten nada ahí y un contador sería estado que
  mantener correcto sin nadie que lo lea.
- **Que el mapeador registre qué tablas creó para quién**, en una lista
  aparte: duplica lo que el árbol ya dice y puede desincronizarse de él.
  El árbol es la fuente.
- **Liberar dentro de `exit`**, en cuanto el proceso muere: ocurre con el
  espacio del propio proceso activo y su pila de kernel en uso. Tiene que
  pasar después, con el kernel en su espacio y en su pila, que es donde ya
  se devuelve todo lo demás.
- **Dejarlo para Fase 4**: la demostración de salida de fase arranca ocho
  procesos y los mata a todos. Cerrar la fase con una fuga proporcional al
  número de procesos es cerrarla con la propiedad al revés.

## Consecuencias

- Un proceso ya no cuesta nada permanente: diez marcos mientras vive
  —código, pila de usuario, cuatro páginas de pila de kernel y cuatro
  tablas— y cero cuando muere.
- `destroy` recorre las tablas de un espacio muerto. La seguridad de ese
  recorrido descansa entera en el punto 4: sin la etiqueta por marco sería
  un doble `free` esperando a ocurrir.
- `dead_processes` entrega los procesos como `&mut`, porque recorrer un
  árbol y vaciarlo lo modifica.
- Queda pendiente lo que el punto 2 protege por ahora a mano: cuando dos
  espacios compartan algo de la mitad baja, el recorrido necesitará saber
  qué es compartido, y eso es el recuento de referencias que la primera
  alternativa aplaza.
