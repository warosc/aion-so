# ADR 0030: La shell, en ring 3

## Contexto

La shell de HARLAN existe desde Fase 1 y corre **dentro del kernel**: lee
teclas por `Console::read_key`, escribe por `Console::write_str` y, desde que
hay disco, no sabe que lo hay. Es el último bullet grande de Fase 4.

Moverla a ring 3 no es una reorganización de código. Es la prueba de que el
límite de privilegio sirve para algo: una shell es el primer programa que una
persona conduce, y **una shell que necesitara privilegios que nadie más
recibe sería un límite con un agujero con forma de shell**.

El ADR 0028 le dio ficheros y el 0029 la consola y el directorio. Lo que queda
por decidir es qué pasa con la del kernel, dónde vive la otra, y qué signfica
que arranque.

## Decisión

### Dos shells, y por qué

1. **La shell del kernel se queda, como camino de reserva.** El kernel tiene
   que arrancar sin red, sin modelo y sin API (CLAUDE.md), y sin disco. Sin
   disco no hay `SHELL.ELF`, y sin `SHELL.ELF` no hay programa que conducir.
   Borrarla sería hacer que un arranque sin disco no tuviera con qué hablar.
2. **Cada una tiene su propio marcador.** `HARLAN-PHASE1-SHELL-READY` es de la
   del kernel; `HARLAN-RING3-SHELL-READY` es de la de ring 3. Que uno de los
   dos significara "una shell subió" escondería justo la regresión que hay que
   cazar: el programa deja de cargar, sube la del kernel, y el arranque sigue
   en verde con **nada** funcionando en ring 3.
3. **Con disco, `boot-test` exige la de ring 3.** Un arranque que llegue a la
   del kernel teniendo disco es un arranque en el que algo falló.

### Dónde vive y cómo llega

4. **Un segundo ejecutable en el disco**, `SHELL.ELF`, construido como el
   primero: mismo objetivo, mismos flags —reubicación estática, sin PIE, base
   de imagen fija, sin información de depuración— porque esos flags son el
   contrato con el cargador. `xtask` construye los dos con **una** función que
   toma los nombres, no con una copia por programa.
5. **El kernel lo arranca el último.** No termina nunca, así que cualquier
   cosa arrancada después de él se arrancaría, pero el arranque no llegaría a
   las líneas que dicen qué hicieron los demás.
6. **No llegar a la shell del kernel es lo normal.** Como el proceso de la
   shell no termina, `run_until_empty` no vuelve, y la shell del kernel —y su
   banner— no corren. Eso significa que la consola se la queda la de ring 3,
   que es lo que hace falta.

### Un binding del ABI, no dos

7. **`user/abi` es la única traducción del ABI a Rust.** Dos programas hacen
   ahora las mismas once llamadas. Dos juegos de envolturas son dos copias de
   un contrato, y una copia de un contrato es una copia que se separa — el
   mismo fallo que este kernel ya tuvo con un `CURRENT` desactualizado, con una
   cuenta de clusters libres llevada a mano, y con un formateador y una prueba
   que compartían un malentendido.
8. **Las sondas de puntero crudo se quedan fuera del binding.** Existen para
   hacer lo que las envolturas tipadas hacen imposible: entregar al kernel una
   dirección que no es un slice que el programa pudiera prestar. Un binding que
   lo pusiera fácil sería un binding que pone fácil el error.

### Qué sabe hacer

9. **Seis comandos**: `help`, `ls`, `cat NAME`, `write NAME TEXT`, `echo TEXT`
   y `exit`. Ni `reboot` ni `shutdown`: apagar la máquina necesita una syscall
   que no existe, y inventarla aquí sería inventar ABI sin ADR.
10. **`cat` lee en trozos de 64 bytes**, mucho menores que los ficheros del
    disco. Un descriptor existe precisamente para que un fichero no tenga que
    caber en un buffer (ADR 0028, punto 1); leer de golpe sería tener
    descriptores y no usarlos para nada.
11. **`write` escribe el fichero entero**, que es lo único que el ADR 0028
    permite, y le añade un salto de línea: un fichero que alguien va a `cat`
    debería acabar en uno.
12. **Los errores se dicen con palabras**, no con el número. `no such file` en
    vez de `-9`: el número es el ABI y la frase es la interfaz.

### Esperar

13. **Esperar una tecla es preguntar, ceder y volver a preguntar**, porque
    `console_read` no bloquea (ADR 0029, punto 11). Mientras no haya nada más
    ejecutable eso es **una espera activa que consume CPU**. Se acepta, se
    nombra, y es la misma deuda de Fase 5 que el ADR 0028 anotó para la E/S:
    desaparece cuando el scheduler sepa dormir a un proceso hasta que haya algo
    que leer.

### La consola es por orden de llegada

14. **Cualquier proceso puede llevarse una tecla destinada a otro.** Hay una
    cola de teclas y no tiene dueño: el que pregunta primero se la lleva. No se
    arregla aquí, se **anota**, porque se encontró de la peor manera posible —
    el programa de pruebas, que sondeaba el teclado, se comió la primera letra
    del primer comando escrito en la shell. En este kernel se resolvió quitando
    esa sonda; de verdad se resuelve cuando la consola sea algo que se entrega,
    que es la pregunta de Fase 6.

## Alternativas consideradas

- **Tirar la shell del kernel**: un binario menos y dos marcadores menos, y un
  arranque sin disco sin nada con que hablar. El kernel tiene que arrancar sin
  disco, así que la del kernel es el suelo.
- **Que las dos usen el mismo marcador**: una comprobación menos en
  `boot-test`, y el fallo más probable —la shell no carga, sube la del kernel—
  pasaría inadvertido.
- **Una shell con heap**, para líneas y nombres de longitud arbitraria: haría
  falta un `mmap` o un `brk` que no existen. Buffers fijos sobre una página de
  pila es lo que hay, y es suficiente para seis comandos.
- **Que la shell sea el único proceso**: más limpio de leer en el registro, y
  perdería las ocho demostraciones de Fase 3 que siguen siendo las que prueban
  el aislamiento en cada arranque.
- **`exec`, para que la shell lance programas**: lo que una shell hace de
  verdad, y necesita que un proceso pueda crear otro, lo que necesita decidir
  qué hereda —y ahí es donde el descriptor se convierte en capacidad. Fase 6.

## Consecuencias

- El arranque normal **ya no llega** a la shell del kernel. `boot-test` espera
  ahora a dos señales, no a una: la shell de ring 3 diciendo que lee teclas, y
  el programa de ring 3 terminando sus comprobaciones. Esperar a cualquiera de
  las dos mataba QEMU en cuanto llegaba la primera, que es la shell, y cortaba
  al programa a media comprobación — un arranque verde que no había comprobado
  nada.
- La máquina pasa a tener **una espera activa permanente** cuando nadie
  escribe. Se mide en el soak.
- `user/abi` es ahora parte del contrato igual que `kernel/src/user.rs`: un
  cambio de ABI toca los dos extremos del mismo cable.
- Lo que la shell puede hacer es exactamente lo que el ABI permite. Añadirle un
  comando que necesite algo nuevo es añadir una syscall, y eso es un ADR.
