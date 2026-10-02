# ADR 0031: El registro de eventos del sistema

## Contexto

Es el último bullet de Fase 4. Y lo que decide su forma no está en el
ROADMAP sino en `ARCHITECTURE.md`:

> Las operaciones destructivas o de alto impacto exigen confirmación y deben
> dejar **auditoría**.

Eso no describe un log de depuración. Describe un rastro que alguien va a
mirar **después**, para saber qué hizo la máquina, y que por tanto tiene que
sobrevivir a que la máquina se pare.

Lo que hay hoy es `klog`: `info!`/`warn!`/`error!` escribiendo bytes al
puerto 0xE9. Sirve, y no es esto:

- **se va con la máquina.** Existe porque un emulador lo copia a un fichero;
  en hardware de verdad no hay nadie escuchando ese puerto;
- **no lo puede leer el sistema.** Un programa en ring 3 no tiene forma de
  preguntar qué pasó;
- **no tiene forma.** Es texto libre pensado para que yo lo lea mientras
  depuro, no registros que signifiquen lo mismo dentro de un año.

Un registro de auditoría tiene que ser las tres cosas que `klog` no es.

## Decisión

### Qué es

1. **Un anillo de líneas en memoria del kernel**, de tamaño fijo, sin
   asignar nada. Lo mismo que el resto de recursos de este kernel: un array
   en `.bss`, no un `Vec`. Un registro que puede quedarse sin memoria es un
   registro que deja de registrar justo cuando algo va mal.
2. **Cuando se llena, se pierde lo más viejo.** Es la elección menos mala de
   las dos: parar de registrar oculta lo que acaba de pasar, que es
   normalmente lo que importa.
3. **Cada evento es una línea de texto**, no un registro binario. Esto es
   una decisión y tiene un motivo concreto: la regla de verificación de este
   proyecto (ADR 0025, punto 5) es que **lo que escribimos lo lee otro**. Un
   fichero de texto lo lee `cat` desde la shell, lo extrae 7-Zip, y lo lee
   una persona. Un formato binario propio solo lo lee nuestro propio
   analizador, que es exactamente la situación que el `EMPTY.BIN` del
   Incremento 29 enseñó a desconfiar.
4. **El formato de la línea es fijo**: `<arranque> <tics> <qué> <detalle>`.
   Fijo para que sea rastro y no prosa — dos eventos iguales en dos
   arranques tienen que escribirse igual, o comparar dos arranques es leer.

### Dónde acaba

5. **En `EVENTS.LOG`, en el disco.** Es lo que lo hace auditoría y no una
   ventana: un registro que se va con la máquina no responde a "qué pasó
   anoche".
6. **Se acumula entre arranques.** El kernel lee lo que hay, le añade lo de
   este arranque, y lo escribe entero. Es lo único que el ADR 0027 permite
   —no hay escritura parcial— y además es lo que hace que el fichero sea la
   historia de la máquina y no la del último arranque.
7. **Tiene un tope, y es el del escritor**: `MAX_FILE_CLUSTERS`, 32 KB. Al
   pasarlo se tiran líneas **enteras** por el principio, nunca media línea:
   un registro que se corta a mitad de línea es un registro que miente sobre
   el último evento que conserva.
8. **Se escribe en momentos nombrados, no en cada evento.** Un evento por
   escritura serían un recorrido de la FAT y dos tablas por cada proceso que
   termina, que es más caro que lo que registra. Se escribe:
   - cuando acaba la ronda de autotest, antes de arrancar la shell;
   - cuando la shell termina.

   Entre medias vive en memoria, y un corte de corriente se lleva lo que no
   se haya escrito. **Eso se dice en vez de disimularse**: este kernel no
   promete un registro síncrono, promete que lo escrito está completo.

### Qué se registra

9. **Lo que cambia de estado, no lo que se hace.** Arranque, disco montado o
   no, programa cargado, proceso arrancado, proceso terminado con su código,
   proceso caído por una falta, fichero escrito, marcos devueltos, shell
   arrancada. No "estoy leyendo el sector 6": eso es depuración y es de
   `klog`.
10. **Un número de arranque en cada línea**, que es el que ya lleva
    `BOOTS.TXT`. Sin reloj de pared no hay fecha, y "el arranque 7" es lo
    más parecido a *cuándo* que esta máquina sabe decir.
11. **Los tics desde el arranque** como segundo campo. No son una hora, y no
    se presentan como una: son para ordenar dentro de un arranque y para ver
    cuánto tardó algo.

### Lo que **no** se decide aquí

12. **No hay syscall nueva.** La shell lee el registro con el `cat` que ya
    tiene, porque es un fichero de texto en el directorio raíz. Un ABI nuevo
    para leer algo que ya se puede leer sería ABI de más, y el ADR 0014
    punto 8 dice que v0 no es estable: cada llamada que se añade es una que
    habrá que mantener o romper.
13. **No hay filtro, ni niveles, ni rotación por tamaño configurable.** Cada
    una es una decisión sobre qué es un registro, y ninguna hace falta para
    tener auditoría.
14. **Nadie puede escribir en él desde ring 3.** Un registro de auditoría en
    el que el auditado escribe no es auditoría. Un programa puede leerlo con
    `cat`; escribirlo con `write EVENTS.LOG ...` lo **sobrescribiría**, y eso
    es un agujero que este ADR deja abierto a sabiendas y que se cierra en
    Fase 6, cuando un fichero pueda ser de alguien. Queda anotado como lo que
    es: una cosa que la shell puede hacer y no debería.

## Alternativas consideradas

- **Escribir cada evento en cuanto pasa**: un registro sin ventana de
  pérdida, y un recorrido de la FAT y dos tablas por cada proceso que
  termina. Haría que registrar cueste más que lo registrado, y en el momento
  de arrancar es cuando la máquina tiene menos que gastar.
- **Registros binarios de tamaño fijo**: más compactos, y solo los lee
  nuestro propio analizador. La regla del ADR 0025 punto 5 existe porque un
  escritor comprobado solo por su propio lector no está comprobado.
- **Un fichero por arranque** (`BOOT0007.LOG`): sin tope, sin tirar nada, y
  un directorio raíz que se llena —no hay subdirectorios (ADR 0028)— y una
  pregunta "qué pasó" que hay que hacerle a *n* ficheros.
- **Sustituir `klog` por esto**: un canal menos que mantener, y se pierde lo
  que `klog` hace bien — escribir desde un manejador de interrupción, sin
  tocar memoria, mientras el mapa de memoria se está reconstruyendo debajo.
  Son dos cosas distintas: una es el microscopio y la otra es el acta.
- **Una syscall para leer el registro**: control sobre quién lo lee, y no hay
  a quién negárselo todavía — no hay usuarios, no hay permisos, y hasta que
  los haya sería un ABI que no protege nada.

## Consecuencias

- Cada arranque escribe un fichero más, que crece. `fsck.vfat` y 7-Zip pasan
  a comprobar un fichero que **cambia de tamaño** en cada arranque, lo que
  ejercita la escritura mucho más que `BOOTS.TXT` con sus dos bytes.
- Hay una ventana de pérdida entre un evento y la escritura. Es parte de la
  decisión y está nombrada; cerrarla necesita escritura parcial, que FAT y el
  ADR 0027 no dan.
- El registro es escribible desde ring 3 con `write`. Se anota como deuda de
  Fase 6; cerrarlo antes exigiría decidir de quién es un fichero, que es la
  pregunta de las capacidades.
- Cuando el fichero llega a 32 KB, la máquina deja de recordar sus primeros
  arranques. Un sistema que tuviera que recordarlos necesitaría un registro
  que no viva en un directorio raíz FAT32 sin subdirectorios.
