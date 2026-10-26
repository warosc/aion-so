# ADR 0027: Escribir en el disco

## Contexto

El kernel lee ficheros (ADR 0025) y carga programas (ADR 0026). La salida
de Fase 4 pide más: **crear, leer y persistir un archivo entre reinicios**.
Eso significa escribir, y escribir en un sistema de ficheros es
cualitativamente distinto de leerlo.

Leer mal da una respuesta equivocada y se nota al momento. Escribir mal
deja un disco roto que se descubre después, a veces mucho después, y con
él se pierde lo que hubiera dentro. Y hay un momento en que el disco está
a medias —la cadena escrita pero el directorio no, o al revés— en que
cortar la corriente deja un volumen que ni este kernel ni ningún otro
entiende.

## Decisión

### Qué se escribe

1. **Crear, extender y sobrescribir un fichero en el directorio raíz.**
   Nada más: ni borrar, ni truncar, ni subdirectorios, ni renombrar. Lo que
   la salida de fase necesita, y cada una de las otras es una operación con
   sus propios casos raros.
2. **El tamaño de un fichero lo decide quien lo escribe**, de una vez. Sin
   escritura por partes ni posición dentro del fichero: se dan los bytes y
   el kernel asigna los clusters que hagan falta. Escribir a trozos exige
   saber dónde se quedó, y eso es un descriptor de fichero, que es otra
   cosa.

### El orden, que es lo que importa

3. **Primero los datos, después la cadena, y el directorio al final.**
   Cada paso deja el volumen en un estado que un lector entiende:
   - escribir en clusters que **ninguna cadena nombra** no cambia nada que
     nadie mire;
   - escribir la cadena hace que esos clusters estén ocupados, y lo peor
     que puede pasar es que queden ocupados sin que nadie los use —una
     cadena perdida, que `fsck` sabe nombrar y recuperar—;
   - escribir la entrada del directorio es lo que hace aparecer el
     fichero, y es **una sola escritura de un sector**.
   Al revés —el directorio primero— un corte deja un fichero que apunta a
   clusters que todavía no son suyos, y eso es un disco que miente.
4. **Las dos tablas se escriben las dos**, la segunda después de la
   primera. Un volumen cuyas tablas no coinciden es el que `fsck` marca
   como dañado, y mantenerlas iguales es barato.
5. **No hay journal ni barreras.** Un corte en el momento exacto entre dos
   sectores deja lo que el punto 3 permite y nada peor. Decirlo es la
   decisión: este kernel no promete atomicidad, promete que el peor caso
   es recuperable.
6. **El FSInfo se actualiza al final, y es una pista.** La
   especificación dice que un lector no debe creerlo; se mantiene porque
   `fsck` lo comprueba y porque dejarlo mintiendo es dejar una pista falsa.

### Asignar clusters

7. **Primer hueco, buscando desde el principio de la tabla.** Sin mapa de
   bits, sin recordar dónde se quedó: se recorre la tabla hasta encontrar
   una entrada libre. Es lento y es correcto, y la alternativa —un mapa de
   bits en memoria— es estado que hay que mantener de acuerdo con el disco
   y que no sobrevive a un reinicio.
8. **Si no hay clusters libres suficientes, no se escribe nada.** Se cuenta
   lo que hace falta y se reserva antes de tocar un solo byte: un fichero a
   medio escribir porque el disco se llenó es peor que un fichero que no
   está.
9. **Sobrescribir reutiliza la cadena que ya hay** mientras alcance, y la
   extiende o la acorta por el final. Los clusters que sobran se liberan
   **después** de que el directorio diga el tamaño nuevo, por la misma
   razón que el punto 3.

### Cómo se comprueba

10. **La misma regla del ADR 0025, punto 5**: lo que este kernel escriba lo
    lee otro. `fsck.vfat` sobre la imagen después de que el kernel haya
    escrito en ella, y 7-Zip extrayendo el fichero que el kernel creó. Un
    escritor comprobado solo por su propio lector no está comprobado.
11. **La persistencia se demuestra con un reinicio**: el kernel escribe en
    el primer arranque, QEMU se reinicia con el **mismo disco**, y el
    segundo arranque lo lee. Es la salida de fase y no se puede demostrar
    de otra manera.

## Alternativas consideradas

- **Escribir con journal**: lo correcto para no perder nada nunca, y FAT no
  tiene dónde ponerlo. Haría falta otro sistema de ficheros.
- **Un mapa de bits de clusters libres en memoria**: mucho más rápido para
  asignar, y estado duplicado que hay que reconstruir en cada arranque y
  mantener de acuerdo con el disco.
- **Escribir el directorio primero** y rellenar después: más simple de
  escribir y deja el disco mintiendo durante la ventana.
- **Una sola tabla**: la segunda no la lee nadie aquí, y `fsck` sí, y un
  volumen que no la mantiene es un volumen dañado para cualquier otro
  sistema.
- **Esperar a tener descriptores de fichero** para escribir: la salida de
  fase no los necesita, y escribirlos sin un escritor debajo sería diseñar
  a ciegas.

## Consecuencias

- La imagen del disco deja de poder reescribirse en cada `build`: si el
  kernel escribe en ella, un `build` que la rehace borra justo lo que hay
  que demostrar. Pasa a escribirse solo cuando falta o cuando su contenido
  no es el esperado, y la prueba de persistencia usa una copia suya.
- El kernel pasa a poder dejar un disco peor de lo que lo encontró. Es la
  primera vez, y es la razón de que el orden del punto 3 sea una decisión y
  no un detalle.
- `fsck.vfat` en CI pasa a comprobar algo que el kernel escribió, no solo
  lo que `xtask` escribió. Ahí es donde empieza a ganarse el sueldo.
