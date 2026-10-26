# ADR 0032: La consola serie

## Contexto

Fase 5 es hardware físico, y su salida es *"HARLAN OS arranca de forma
repetible en el PC objetivo"*. Antes de nada de eso hay un problema que lo
bloquea todo:

**Todo lo que este kernel dice sale por el puerto 0xE9.** Ese puerto existe
porque QEMU y Bochs lo miran; **hardware real lo ignora por completo**. En el
PC objetivo el kernel arrancaría y no diría absolutamente nada — ni una
línea, ni un error, ni dónde se quedó. Es el peor estado posible desde el que
depurar un primer arranque, y es el estado en el que estamos ahora mismo.

Lo que esa máquina sí tiene, o puede tener por un adaptador USB, es un UART.
Es el canal con el que se depura un arranque que no llega a dibujar nada, y
el que un BMC o un cable serie leen desde otro ordenador.

## Decisión

### Dónde y cómo

1. **COM1, en `0x3F8`, 115 200 8N1.** La dirección fija de siempre, no una
   encontrada sondeando: es donde la ha puesto todo PC desde el primero, y es
   donde la pone el firmware cuando usa una. 115 200 porque es lo que cualquier
   terminal asume y porque más lento costaría más arranque del que vale — una
   línea son unos 60 bytes, que a 115 200 son 5 ms.
2. **Por sondeo, no por interrupción.** Esto corre desde dentro de manejadores
   de interrupción y mientras el mapa de memoria se reconstruye debajo, así
   que no puede tocar memoria, ni coger cerrojos, ni asignar nada — el mismo
   contrato que el puerto de depuración ya cumple.
3. **A los dos sitios, no a uno.** El puerto de depuración se queda: todas las
   herramientas de este repositorio lo leen. Una línea que fuera solo a uno de
   los dos es una línea que alguien no puede ver.
4. **Se instala en `klog::install`**, que ya se llama dos veces —al principio
   del arranque y otra vez cuando la imagen se muda—, así que el puerto queda
   configurado también después de que el firmware lo haya soltado.

### El número que importa

5. **Esperar a que el transmisor esté listo tiene un tope, y es la decisión
   más importante del ADR.** En una máquina sin COM1, o con un UART atascado,
   el bit de "listo" no se pone nunca. Un bucle sin tope giraría para siempre
   dentro del camino del log, que corre desde manejadores de interrupción: **el
   kernel se colgaría en su primera línea, en hardware real, en silencio**.
6. **Un byte perdido es un log peor; una espera sin fin es una máquina
   muerta.** Se elige lo primero. 100 000 intentos, que son generosos al lado
   de lo que tarda un byte (87 µs) y nada al lado de un arranque.

### Detectar, no suponer

7. **Se comprueba que hay un UART antes de escribirle.** Una máquina moderna a
   menudo no tiene COM1, y escribir el log a un puerto que nadie contesta
   costaría el tope entero por byte para nada.
8. **Dos preguntas, no una**, porque cada una por separado la contesta mal un
   bus vacío:
   - el registro de scratch es un byte de memoria que no hace nada, así que lo
     que entra tiene que salir;
   - y un byte mandado por el loopback del propio integrado tiene que volver
     como él mismo, lo que descarta un puerto que devuelve lo último escrito
     sin ser un UART.

### Lo que **no** se decide aquí

9. **No se lee del puerto.** La shell lee del teclado. Una consola serie de
   entrada es otra decisión —quién es dueño de las teclas cuando hay dos
   fuentes— y la consola ya tiene ese problema sin resolver (ADR 0030,
   punto 14).
10. **No hay segundo puerto, ni COM2, ni descubrimiento por ACPI.** Cuando el
    inventario del equipo objetivo diga que hace falta.

## Alternativas consideradas

- **Dejar solo el puerto de depuración**: cero trabajo, y un primer arranque en
  el PC objetivo del que no se podría saber nada. Es el problema que este ADR
  existe para quitar.
- **Sustituir el puerto de depuración por el serie**: un canal en vez de dos, y
  habría que cambiar todas las herramientas del repositorio a la vez para
  ganar nada.
- **UART por interrupción, con una cola**: no perdería bytes cuando el
  transmisor va justo, y necesita memoria y un cerrojo en el camino del log,
  que es justo lo que no puede tener.
- **Sin tope de espera**: un log completo siempre, y una máquina que se cuelga
  en la primera línea si el puerto no responde. Medido (ver más abajo): sin
  tope no llega ninguna shell en 45 segundos.
- **Esperar menos**: menos riesgo de gastar tiempo, y bytes perdidos en un
  puerto lento que sí funciona.

## Consecuencias

- El arranque entero sale por COM1, incluido lo que el firmware escribe antes
  de que el kernel exista — en QEMU la salida de OVMF y la del kernel llegan
  por el mismo fichero, que es lo que pasará también en la máquina de verdad.
- `boot-test` captura COM1 aparte y exige que el arranque haya salido por ahí.
  Son dispositivos distintos: una prueba que lee uno no dice nada del otro, y
  un kernel que dejara de escribir en COM1 seguiría verde aquí y llegaría mudo
  al PC objetivo.
- En una máquina sin COM1 esto no cuesta nada: la detección lo dice y
  `write_str` vuelve sin hacer nada. Medido: el soak de 120 s da los mismos
  11 800 tics con y sin puerto.

## Lo que se midió, y dos medidas que no valían

El tope del punto 5 está **demostrado**, no razonado, y las dos primeras
formas en que intenté demostrarlo no probaban nada:

- Correr `boot-test` sin el tope **pasó**, porque `boot-test` engancha un
  puerto de verdad: el transmisor siempre está listo y el bucle no gira nunca.
- Correr el soak —que va sin puerto— sin el tope **también pasó**, y esta es
  la interesante: **un bus vacío devuelve `0xFF`, y `0xFF` tiene el bit de
  "listo" puesto**. Sin UART el bucle sale a la primera vuelta. El caso
  peligroso no es "no hay puerto": es "hay puerto y no responde", que QEMU no
  sabe producir.

Forzándolo —el bit de listo enmascarado a cero, que es exactamente un UART
atascado— la pareja sale clara:

| | con tope | sin tope |
|---|---|---|
| arranca | **sí, 5,69 s** | **no, ninguna shell en 45 s** |
| el log | se pierde, y `boot-test` lo dice | no hay log: no hay arranque |

Las dos pruebas que pasaron por el motivo equivocado se parecían mucho a las
que valen. Es la misma lección del ADR 0025 punto 5 por otro camino: una
comprobación que pasa no es una comprobación que mide.
