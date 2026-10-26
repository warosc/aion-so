# ADR 0033: Solo nuestro disco

## Contexto

La salida de Fase 5 lo dice con todas las letras: *"HARLAN OS arranca de
forma repetible en el PC objetivo **sin escribir en discos no
seleccionados**"*.

Hoy el kernel coge el primer dispositivo de almacenamiento virtio que
encuentra en el bus PCI y le escribe: `BOOTS.TXT`, `EVENTS.LOG`,
`RING3.TXT`. En QEMU eso es el único disco que hay y es nuestro. **En tu PC
hay discos con tus cosas dentro.**

Ahora mismo no llegaría a tocarlos, porque no hay driver para NVMe ni SATA.
Pero el bullet siguiente de la fase es *drivers mínimos de almacenamiento*, y
el día que exista, un kernel sin esta regla escribiría en el primer disco que
supiera manejar. **La regla tiene que existir antes que los drivers, no
después**, porque después se escribe con un disco ya roto.

## Decisión

### Qué es "nuestro"

1. **Un volumen es nuestro si dice serlo de las dos maneras**: el serial de
   volumen que escribe el formateador de `xtask` —`HARL`, cuatro bytes ASCII
   leídos como número— **y** la etiqueta `HARLAN`. Las dos, no una.
2. **Un volumen que no dice nada no es nuestro.** El bloque extendido del BPB
   puede no estar; cuando no está, esos bytes son lo que dejara el
   formateador, y una etiqueta leída de basura es peor que ninguna etiqueta,
   porque es un nombre por el que algo podría fiarse.
3. **Negarse es la respuesta que es segura cuando se equivoca.** Equivocarse
   por este lado cuesta un disco nuestro en el que no escribimos; por el otro
   cuesta los datos de alguien.

### Lo que esto **no** es

4. **Es una guarda contra escribir en el disco equivocado, no contra un
   adversario.** Ni el serial ni la etiqueta son secretos, y cualquier
   herramienta escribe los dos: un disco que afirme ser nuestro recibe
   escrituras. Decirlo aquí es parte de la decisión — un documento que
   presentara esto como seguridad invitaría a apoyar algo encima que no
   aguanta.
5. Lo que sí impide es lo que la fase pide: que un kernel arrancado en un PC
   de verdad decida que uno de los discos de la persona es su borrador.

### Dónde está la compuerta

6. **En dos sitios, y es a propósito.**
   - En `fs::write_file`, antes que nada —incluso antes de decidir si el
     nombre vale—, porque "¿puedo escribir aquí?" no es una pregunta sobre
     los argumentos. Da el error bueno.
   - En `Sectors::write_sector`, que es el punto por donde pasa **cada byte**
     que llegaría a un plato. Esa es la que no se puede rodear.
7. **La segunda hace que la regla sea una propiedad del kernel y no de una
   función que se acuerda.** Si llegara a dispararse, algo encontró un camino
   al disco que no pasa por `write_file`, y eso merece decirse en voz alta.
8. **Se decide una vez, al montar.** Un volumen que se cambiara la etiqueta
   por debajo tiene problemas mayores, y releer el sector 0 en cada escritura
   sería pagar por una pregunta cuya respuesta no puede cambiar mientras está
   montado.

### Lo que ve un programa

9. **`ERR_NO_PERMISSION` (`-3`).** El ADR 0014 lo reservó diciendo que no
   tenía nada que rechazar todavía; esto es para lo que era. No es un
   argumento malo ni un disco roto: es una cosa legítima que no se permite.

### Lo que se dice en el registro

10. **Las dos respuestas se dicen, y la negativa se anota como evento.** Un
    kernel que declinara escribir en silencio se vería exactamente igual que
    uno con el disco roto; y uno que escribiera en silencio es lo que esta
    fase existe para no hacer.

## Alternativas consideradas

- **Elegir el disco por posición** —el primero de virtio, que es lo que hace
  hoy—: cero trabajo, y en una máquina con varios discos elige uno
  cualquiera. Es el comportamiento que esto sustituye.
- **Elegir por una marca fuera del volumen** (una variable UEFI, un
  parámetro de arranque): más explícito, y mete una pieza de configuración
  que hay que poner en algún sitio y que puede quedarse vieja. La marca va
  en el propio volumen, que es lo que viaja con él.
- **Solo la etiqueta**: más fácil de poner a mano, y `HARLAN` es una palabra
  que alguien podría ponerle a un disco suyo. El serial la ancla al
  formateador.
- **Solo el serial**: nadie lo pone por accidente, y no se ve. La etiqueta es
  lo que una persona lee en un gestor de archivos antes de enchufar el disco
  en la máquina de pruebas.
- **Una sola compuerta**: una comprobación menos, y la regla pasa a depender
  de que todo camino futuro al disco se acuerde de llamarla. Medido más
  abajo: cada una sola basta, y por eso están las dos.
- **Montar el volumen de solo lectura en el tipo** (que `write_file` no
  exista si no es nuestro): lo haría imposible en vez de comprobado, y
  obligaría a dos tipos de volumen por todo `hal`, que es un crate que no
  debería saber qué significa "nuestro".

## Consecuencias

- Un disco que no es nuestro **se lee**. Hace falta leerlo para saber de quién
  es, y leer no estropea nada. Lo que no pasa es que se escriba.
- El kernel arranca igual con un disco ajeno: lee el programa y la shell si
  están, y falla solo lo que necesitaba escribir. Un primer arranque en una
  máquina desconocida no se queda a medias.
- `hal` reporta y el kernel decide. El crate del formato dice qué pone el
  volumen; qué cuenta como nuestro es política, y la política no vive en un
  lector de formatos.

## Lo que se midió

Un volumen FAT32 **bien formado** que no es nuestro —el mismo disco,
reetiquetado `NOTYOURS` con serial `0xDEADBEEF`— puesto donde el kernel busca,
y la imagen comparada byte por byte antes y después:

```
HARLAN: this disk says it is "NOTYOURS" with serial 0xdeadbeef, which is not
        what this kernel's tooling writes; it will be read and never written
HARLAN: BOOTS.TXT could not be written (NotOurDisk)
HARLAN: EVENTS.LOG could not be written (NotOurDisk)
sha256 antes y después: iguales — ni un byte
```

Y la máquina arranca hasta la shell en ring 3 igualmente.

Las dos compuertas, cada una por su cuenta:

| | el disco ajeno |
|---|---|
| las dos puestas | sin tocar |
| sin la de `write_file` | sin tocar — la del dispositivo lo para |
| sin la del dispositivo | sin tocar — la de `write_file` lo para |
| **sin ninguna** | **1 860 bytes escritos** |

Cada una basta sola, y quitando las dos el kernel escribe en el disco de otro.
La defensa en profundidad es real, no decorativa.

Mutación sobre la decisión: once, nueve detectadas por pruebas de host —el
serial solo, la etiqueta sola, cualquiera de las dos, un nombre que empieza
igual, un volumen que no dice nada, la etiqueta leída de donde no es—. Las dos
que no, son las compuertas mismas, que ninguna prueba de host alcanza porque
necesitan un disco montado: las caza el arranque de arriba.
