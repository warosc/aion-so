# ADR 0028: El ABI de ficheros v0

## Contexto

El kernel lee el disco (ADR 0025), escribe en él (ADR 0027) y carga
programas desde ahí (ADR 0026). Todo eso lo hace el kernel para sí mismo.
Lo que falta de Fase 4 es que un programa en ring 3 pueda hacerlo: la
shell tiene que poder listar, leer y crear ficheros sin que el kernel se
lo haga por dentro.

Eso es ABI, y el ROADMAP marca los cambios de ABI como decisiones de ADR.
Es además el punto donde se decide algo que sobrevive a Fase 4: **si un
programa nombra un fichero cada vez que lo toca, o si recibe algo que lo
representa**. Lo segundo es un descriptor, y un descriptor es la forma que
tendrá una capacidad en Fase 6. Elegir mal aquí es elegir mal allí.

Las opciones estaban entre dejarlo todo en operaciones de fichero entero
—simple, y una shell que lee un fichero de 2 MB necesita 2 MB de buffer— y
descriptores completos con posición y escritura parcial —lo que espera
cualquier programa, y escritura parcial es justo lo que el ADR 0027 se negó
a prometer sobre FAT—.

## Decisión

### La asimetría, que es la decisión

1. **Para leer, descriptores. Para escribir, el fichero entero.** No es
   una simetría rota por descuido: son dos problemas distintos.
   - Leer a trozos hace falta porque un fichero es más grande que un
     buffer, y leerlo entero obliga a quien lee a tener sitio para todo.
     Un descriptor es lo mínimo que resuelve eso: recuerda por dónde iba.
   - Escribir a trozos hace falta para cambiar un fichero por partes, y
     eso en FAT significa asignar un cluster en medio de una cadena con el
     directorio diciendo todavía el tamaño viejo. El ADR 0027, punto 2, ya
     decidió no prometerlo. Un `write` con posición sería un ABI que
     promete lo que el escritor de debajo no cumple.
2. **El coste se asume y se nombra**: un programa que quiera cambiar tres
   bytes de un fichero tiene que leerlo entero, cambiarlos y escribirlo
   entero. Para una shell y sus ficheros de configuración es suficiente.
   Para un editor de verdad no, y ese es el día en que este ADR sube de
   versión.

### Las llamadas

3. **Cuatro, numeradas tras las cinco que ya hay** (ADR 0014 punto 7, más
   `yield`, `send` y `recv` del ADR 0019):

   | nº | llamada | argumentos | resultado |
   |----|---------|-----------|-----------|
   | 5 | `open` | `RDI` = nombre, `RSI` = longitud | descriptor ≥ 0 |
   | 6 | `read` | `RDI` = descriptor, `RSI` = buffer, `RDX` = longitud | bytes leídos, 0 al final |
   | 7 | `close` | `RDI` = descriptor | 0 |
   | 8 | `write_file` | `RDI` = nombre, `RSI` = longitud, `RDX` = datos, `R10` = longitud | bytes escritos |

   `list` no está: el directorio raíz se lee abriendo el nombre especial
   que FAT ya reserva, y eso es una decisión con sus propios casos raros.
   La shell de Fase 4 lista con una llamada aparte cuando la necesite.

### Qué es un descriptor

4. **Un entero pequeño, índice en una tabla del proceso.** No un puntero
   disfrazado, no un identificador global, no un número con estructura
   dentro. Un programa no puede inventarse uno válido ni adivinar el de
   otro proceso, porque el espacio de números es por proceso y el kernel
   no mira más allá de su tabla.
5. **La tabla es fija y pequeña: cuatro por proceso.** Vive dentro de
   `Process`, sin asignar memoria, como los `owned` y los `frames`
   (ADR 0017). Un programa que necesite más de cuatro ficheros a la vez en
   Fase 4 está haciendo algo que esta fase no tiene que soportar.
6. **Un descriptor guarda la entrada de directorio con la que se abrió y
   por dónde va.** No un puntero al volumen, no un estado del driver: los
   datos suficientes para hacer la lectura siguiente desde cero. El
   volumen es del kernel y nunca se le presta a nadie.
7. **Un descriptor no se transfiere.** No se pasa por IPC, no se hereda:
   no hay con qué, porque en Fase 4 nadie crea procesos. Decirlo es
   importante porque **transferirlo es exactamente lo que lo convierte en
   una capacidad**, y ese es el trabajo de Fase 6. Lo que se diseña ahora
   es la parte que no cambia: que sea opaco, por proceso y revocable
   cerrándolo.
8. **`exit` cierra los descriptores del proceso**, como libera sus marcos
   (ADR 0021). Un proceso que acaba no deja nada.

### Lo que llega de ring 3 no se cree

9. **Los punteros se validan como dice el ADR 0014, punto 9**: dentro de la
   mitad baja, sin desbordar, y mapeados para *este* proceso. El buffer de
   `read` además tiene que ser escribible por el usuario: un programa que
   pida leer encima de su propio código recibe `ERR_BAD_ARGUMENT`, no una
   violación de W^X hecha por el kernel en su nombre.
10. **El nombre pasa por el mismo `encode_name` que usa el kernel.** Un
    nombre con una barra, un NUL o un carácter que FAT reserva es un
    argumento inválido, no un disco corrupto. Esto es lo que hace que no
    haya rutas: un nombre con `/` se rechaza antes de llegar al volumen.
11. **Las longitudes se acotan antes de usarse.** `read` lee como mucho lo
    que quepa en el buffer y lo que quede del fichero; `write_file` tiene
    el límite de `MAX_FILE_CLUSTERS` que ya impone el ADR 0027.

### Lo que no puede pasar a la vez

12. **`write_file` se niega si ese nombre está abierto en algún proceso.**
    Un descriptor guarda la cadena con la que se abrió; sobrescribir el
    fichero reutiliza esa cadena (ADR 0027, punto 9), así que quien
    estuviera leyendo pasaría a leer bytes nuevos en medio de los viejos.
    Recorrer cuatro descriptores por proceso para comprobarlo es barato, y
    elimina la clase entera de problema en vez de documentarla.

### Los errores

13. **Siguen la numeración del ADR 0014, punto 6**, negativos y explícitos,
    continuando donde acaba el ADR 0019:
    `-7` descriptor inválido, `-8` demasiados ficheros abiertos, `-9` no
    existe ese fichero, `-10` el fichero está abierto, `-11` el volumen
    está lleno, `-12` error del disco.
14. **Un error del disco no mata al proceso.** Es la diferencia entre un
    fallo del medio y una falta del programa (ADR 0020): el programa pidió
    algo legítimo y el disco no respondió. Recibe `-12` y decide él.

## Alternativas consideradas

- **Todo por fichero entero** (`read_file(nombre, buffer)` y
  `write_file`): el ABI más pequeño que existe, sin tabla por proceso, sin
  estado que limpiar al morir, y la shell no podría leer nada más grande
  que su buffer. Además no deja nada que convertir en capacidad: un nombre
  no se puede revocar.
- **Descriptores completos, con `write` y `seek`**: lo que espera
  cualquiera que haya escrito un programa, y promete sobre FAT lo que el
  ADR 0027 decidió no prometer. Se podría implementar leyendo-modificando-
  escribiendo por debajo, y entonces el ABI estaría mintiendo sobre lo que
  cuesta cada llamada.
- **Un descriptor que sea un puntero o un identificador global**: ahorra la
  tabla y regala la capacidad de nombrar el fichero de otro proceso. Es
  exactamente lo contrario de lo que hace falta en Fase 6.
- **Mapear el fichero en memoria**: la interfaz más agradable de todas, y
  exige fallos de página que resuelven E/S, que es un mecanismo que este
  kernel no tiene y que no debería estrenar con un sistema de ficheros sin
  journal debajo.
- **Permitir sobrescribir un fichero abierto** y documentar que el lector
  puede ver cualquier cosa: más barato, y convierte un error del kernel en
  un error del programa, que es la dirección equivocada.

## Consecuencias

- `Process` crece con una tabla de cuatro descriptores, y `destroy` tiene
  una cosa más que soltar. Es el tercer recurso por proceso después de los
  rangos y los marcos, y el primero que no es memoria.
- El volumen pasa a ser un recurso compartido entre procesos, cuando hasta
  ahora solo lo tocaba el kernel al arrancar. Necesita un cerrojo, y las
  llamadas de fichero pasan a ser las primeras que pueden esperar por él.
- **Una llamada de fichero es larga.** El manejador corre con las
  interrupciones desactivadas (ADR 0014, punto 2) y una lectura de disco
  gira sobre la virtqueue, así que `read` retrasa el reloj de forma
  medible. En Fase 4 se acepta y se mide; E/S que bloquea y devuelve el
  control al scheduler es trabajo de Fase 5, y esta es la deuda que lo
  justifica.
- v0 sigue sin ser estable (ADR 0014, punto 8). El primer programa de
  fuera congelará el contrato; hasta entonces, cambiarlo es subir este ADR.
