# ADR 0017: Cada proceso con su espacio de direcciones

## Contexto

El Incremento 18 dejó un programa corriendo en ring 3, pero **dentro de las
tablas del kernel**: lo único que lo separaba era el bit de usuario en sus
páginas. Eso basta para que no lea al kernel y no basta para nada más: dos
programas compartirían el mapa y se verían el uno al otro.

La salida de Fase 3 es que dos procesos aislados se comuniquen sin
compartir memoria no autorizada. Sin un espacio por proceso no hay nada que
aislar.

El borrador de este ADR, escrito antes de la revisión de Codex, resolvía
las llamadas al firmware volviendo al CR3 del kernel. Ya no hace falta: el
Incremento 20 movió los runtime services a la mitad alta y **la mitad baja
está vacía**, así que un espacio de usuario no tiene nada del firmware
dentro ni el kernel tiene que malabarear con CR3 para apagar la máquina.

## Decisión

1. **Un PML4 por proceso.** Se asigna un marco, se pone a cero y se
   **copian las entradas de la mitad alta** del kernel. Copiar la entrada,
   no el subárbol: los dos espacios apuntan a las mismas tablas, así que
   cualquier mapeo que el kernel haga después se ve desde todos los
   procesos, sin sincronizar nada a mano.
2. **La mitad baja de cada espacio empieza vacía.** Lo que el proceso tiene
   es lo que se le mapea: su código y su pila. Nada más, y nada de nadie.
3. **Cambiar de espacio es escribir CR3.** El kernel corre en la mitad
   alta, idéntica en todos los espacios, así que la instrucción siguiente
   al cambio sigue estando donde estaba.
4. **El proceso es un objeto**: su espacio, sus rangos, su entrada y su
   pila. Lo que el manejador de syscalls valida deja de ser una global y
   pasa a ser la memoria del proceso que hizo la llamada.
5. **Dos procesos, mismas direcciones, memoria distinta.** El arranque crea
   dos y comprueba que la misma dirección virtual resuelve a marcos
   distintos en cada espacio. Es la demostración de que hay aislamiento y
   no solo separación de privilegios.

## Alternativas consideradas

- **Seguir con un solo espacio y el bit de usuario**: es lo que hay. No
  aísla procesos entre sí, que es lo que pide la fase.
- **Copiar el subárbol de la mitad alta** en vez de las entradas: cada
  espacio tendría su copia de las tablas del kernel, que habría que
  mantener sincronizadas en cada mapeo. Bugs a cambio de nada.
- **Un espacio compartido con la mitad baja partida** (un proceso por
  trozo): obliga a que cada proceso conozca direcciones ajenas y no escala.
- **Espacios con la mitad alta también propia** (aislar al kernel de sí
  mismo por proceso): no aporta nada mientras haya un solo kernel, y
  costaría un cambio de tablas en cada syscall.

## Consecuencias

- Un proceso cuesta su PML4 más una tabla por nivel y región mapeada: con
  código y pila separados por un mega, cinco marcos.
- **El aislamiento pasa a ser comprobable**: la misma dirección en dos
  procesos da marcos distintos, y un proceso que lea la memoria del otro
  recibe `#PF` porque en su espacio ahí no hay nada.
- El kernel tiene que saber en qué espacio está para validar punteros, y
  eso deja de ser un detalle: es parte del ABI (ADR 0014, punto 9).
- Todavía **no hay planificador**: el kernel arranca un proceso, el proceso
  sale, el kernel vuelve a su espacio y sigue. Guardar y restaurar el
  estado de un proceso interrumpido es el Incremento 22.
- Mientras el kernel comparta sus tablas de la mitad alta, un fallo suyo
  que mapee algo con el bit de usuario lo expondría a todos los procesos.
  El mapper ya rechaza mezclar los dos mundos (Incremento 18), y esa
  comprobación pasa a ser la que sostiene el aislamiento.
