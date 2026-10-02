# Notas de Fase 5 — Hardware físico

Lo más reciente arriba. Salida de la fase (ROADMAP.md): **HARLAN OS arranca
de forma repetible en el PC objetivo sin escribir en discos no
seleccionados**.

Esa salida **no se puede alcanzar sin la máquina**. Hay dos cosas que no
dependen de este repositorio: saber qué lleva dentro el equipo objetivo, y
arrancarlo. Lo que sí se puede hacer entre tanto es todo lo demás, y lo
primero de todo es que cuando ese arranque ocurra, la máquina diga algo.

## Incremento 38 — Solo nuestro disco

`docs/adr/0033-fase5-only-our-disk.md`.

### Por qué ahora y no después

La salida de la fase lo dice con todas las letras: *"sin escribir en discos no
seleccionados"*.

Hoy el kernel coge el primer dispositivo virtio que encuentra y le escribe. En
QEMU ese es el único disco y es nuestro. **En el PC objetivo hay discos con las
cosas de alguien dentro.** Ahora mismo no llegaría a tocarlos porque no hay
driver para NVMe ni SATA — pero el bullet siguiente de la fase es *drivers
mínimos de almacenamiento*, y el día que exista, un kernel sin esta regla
escribiría en el primer disco que supiera manejar.

**La regla tiene que existir antes que los drivers**, porque después se escribe
con un disco ya roto.

### Qué es "nuestro"

El serial que escribe el formateador de `xtask` —`HARL`, cuatro bytes ASCII
leídos como número— **y** la etiqueta `HARLAN`. Las dos, no una. Un volumen que
no lleva el bloque extendido del BPB no dice nada, y lo que no dice nada no es
nuestro: negarse es la respuesta que es segura cuando se equivoca, porque
equivocarse por ese lado cuesta un disco nuestro en el que no escribimos y por
el otro cuesta los datos de alguien.

**Y es una guarda contra escribir en el disco equivocado, no contra un
adversario.** Ni el serial ni la etiqueta son secretos. Lo dice el ADR porque
un documento que presentara esto como seguridad invitaría a apoyar encima algo
que no aguanta.

### Dos compuertas, y las dos hacen falta

Una en `fs::write_file`, antes que nada —incluso antes de mirar si el nombre
vale, porque "¿puedo escribir aquí?" no es una pregunta sobre los argumentos—,
que da el error bueno. Y otra en `Sectors::write_sector`, que es el punto por
donde pasa **cada byte** que llegaría a un plato, y esa es la que no se puede
rodear.

Medido, quitándolas una a una:

| | el disco ajeno |
|---|---|
| las dos puestas | sin tocar |
| sin la de `write_file` | sin tocar — la del dispositivo lo para |
| sin la del dispositivo | sin tocar — la de `write_file` lo para |
| **sin ninguna** | **1 860 bytes escritos** |

Cada una basta sola, y quitando las dos el kernel escribe en el disco de otro.
La defensa en profundidad es real, no decorativa.

### La prueba que vale es la que compara bytes

Un volumen FAT32 **bien formado** que no es nuestro —el mismo disco,
reetiquetado `NOTYOURS` con serial `0xDEADBEEF`— puesto donde el kernel busca:

```
HARLAN: this disk says it is "NOTYOURS" with serial 0xdeadbeef, which is not
        what this kernel's tooling writes; it will be read and never written
HARLAN: BOOTS.TXT could not be written (NotOurDisk)
HARLAN: EVENTS.LOG could not be written (NotOurDisk)
```

y **el mismo sha256 antes y después**. La línea del log es la palabra del
kernel sobre sí mismo; la imagen sin cambiar es la evidencia.

La máquina arranca hasta la shell en ring 3 igualmente: lee el programa y la
shell, y falla solo lo que necesitaba escribir. Un primer arranque en una
máquina desconocida no se queda a medias.

### Un error de mi script que casi me cuela una prueba falsa

La primera versión del script que prepara el disco ajeno escribía la identidad
con `foreach ($base in @(0, 6 * 512))`. En PowerShell **la coma liga más fuerte
que el `*`**, así que eso es `(0,6)` repetido 512 veces: escribió la identidad
mil veces en sitios equivocados y dejó el volumen hecho un lío.

La prueba **pasó igual** —un volumen destrozado tampoco es nuestro—, y casi la
doy por buena. Lo que la delató fue que el kernel imprimió la etiqueta como
`""` en vez de `"NOTYOURS"`. Una prueba que pasa por el motivo equivocado se
parece mucho a una que vale; la diferencia estaba en una línea del log que no
cuadraba.

### Verificación ejecutada

- 130 pruebas en `kernel` y 103 en `hal`, con la decisión probada por cada
  forma de ser casi nuestro: el serial solo, la etiqueta sola, cualquiera de
  las dos, un nombre que empieza igual, uno que es prefijo, otro en
  minúsculas, y uno vacío.
- Mutación: 11, 9 detectadas en host. Las 2 que no son las compuertas mismas,
  que ninguna prueba de host alcanza porque necesitan un disco montado — las
  caza el arranque con el disco ajeno, que es la tabla de arriba.
- Y nuestro propio disco sigue siendo nuestro: `this disk is ours ("HARLAN",
  serial 0x4841524c)`, y los arranques siguen contándose.

## Incremento 37 — La consola serie

`docs/adr/0032-fase5-serial-console.md`.

### El problema que bloquea la fase entera

Todo lo que este kernel dice sale por el puerto `0xE9`. Ese puerto existe
porque QEMU y Bochs lo miran; **hardware real lo ignora por completo**.

En el PC objetivo, el kernel de hoy arrancaría y no diría **absolutamente
nada**: ni una línea, ni un error, ni dónde se quedó. No es un detalle de
comodidad — es que el primer arranque real sería una pantalla negra sin
ninguna información, y no habría forma de saber si falló el firmware, el
cargador, el mapa de memoria o el disco.

Lo que esa máquina sí tiene, o puede tener por un adaptador USB de dos euros,
es un UART. Es el canal con el que se depura un arranque que no llega a
dibujar nada.

### Qué hace

COM1 en `0x3F8`, 115 200 8N1, por sondeo. El log va **a los dos sitios**: el
puerto de depuración se queda porque todas las herramientas de este
repositorio lo leen, y una línea que fuera solo a uno es una línea que
alguien no puede ver.

Se detecta antes de escribir, con dos preguntas y no una, porque cada una por
separado la contesta mal un bus vacío: el registro de scratch tiene que
devolver lo que se le mete, y un byte mandado por el loopback del propio
integrado tiene que volver como él mismo.

### El número que importa

Esperar a que el transmisor esté listo **tiene un tope**. Sin él, un UART que
no responde haría girar para siempre un bucle dentro del camino del log — que
corre desde manejadores de interrupción— y el kernel se colgaría en su
primera línea, en hardware real, en silencio. Un byte perdido es un log peor;
una espera sin fin es una máquina muerta.

### Dos medidas que no valían

Y aquí está lo que más me interesa de este incremento, porque **las dos
primeras formas en que intenté demostrar el tope no probaban nada, y las dos
pasaron**.

**La primera**: correr `boot-test` sin el tope. Pasó — porque `boot-test`
engancha un puerto de verdad, el transmisor siempre está listo y el bucle no
gira nunca.

**La segunda**, que es la interesante: correr el soak, que va **sin** puerto,
sin el tope. También pasó. La razón es que **un bus vacío devuelve `0xFF`, y
`0xFF` tiene el bit de "listo" puesto**. Sin UART, el bucle sale en la primera
vuelta.

O sea que el caso peligroso no es "no hay puerto". Es **"hay puerto y no
responde"**, que QEMU no sabe producir. Forzándolo —enmascarando el bit de
listo a cero, que es exactamente un UART atascado— la pareja sale clara:

| | con tope | sin tope |
|---|---|---|
| arranca | **sí, 5,69 s** | **no, ninguna shell en 45 s** |
| el log | se pierde, y `boot-test` lo dice | no hay log: no hay arranque |

Las dos pruebas que pasaron por el motivo equivocado se parecían mucho a las
que valen. Es la lección del ADR 0025 punto 5 por otro camino: **una
comprobación que pasa no es una comprobación que mide**.

### `boot-test` captura COM1 aparte

Son dispositivos distintos, así que una prueba que lee uno no dice nada del
otro. Un kernel que dejara de escribir en COM1 seguiría verde en todo lo
demás y llegaría mudo al PC objetivo, que es justo la regresión que este
incremento existe para hacer imposible.

### Verificación ejecutada

- Con puerto: 19 035 bytes del mismo arranque salen por COM1 — más que los
  18 492 del puerto de depuración, porque la salida del propio OVMF viene por
  ahí también. Eso es lo que pasará en la máquina de verdad.
- Sin puerto: el soak de 120 s da **los mismos 11 800 tics** con y sin, y el
  kernel no anuncia puerto. La detección cuesta cero.
- La pareja de mutaciones de arriba.
- 390 pruebas de host sin cambios, `fmt-lint` limpio, arranques en verde.

### Lo que no se probó

**Nada de esto se ha visto en hardware.** Que un terminal al otro lado lea
115 200 8N1 y salga texto legible es lo que falta por comprobar, y lo
comprueba quien tenga la máquina y un cable. Todo lo demás —que el kernel
escriba, que detecte, que no se cuelgue— está medido en QEMU.

### Lo que hace falta de fuera para seguir

1. **Qué equipo es el objetivo.** El bullet *inventario de hardware* no se
   puede escribir sin saber de qué.
2. **Arrancarlo.** La salida de fase es un arranque repetible en esa máquina,
   y eso no lo puede hacer este repositorio.

Lo que sí se puede ir haciendo sin la máquina: la imagen USB UEFI (y
arrancarla en QEMU desde USB, que ejercita el mismo camino), y la regla de
*no escribir en discos no seleccionados*, que es un ADR y se comprueba en
QEMU con varios discos enganchados.
