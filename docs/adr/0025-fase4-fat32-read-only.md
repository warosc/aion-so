# ADR 0025: El sistema de ficheros, y cómo se comprueba que lo leemos bien

## Contexto

El kernel lee sectores (ADR 0024). Un sector no es un fichero: para que la
shell de Fase 4 pueda abrir algo hace falta decidir **qué formato** tiene
el disco, y eso el ROADMAP lo marca explícitamente como decisión de ADR.

Hay una segunda pregunta, menos obvia y más importante: **contra qué se
comprueba que el lector es correcto**. Un analizador de sistemas de
ficheros escrito y probado por la misma persona que escribió la imagen
que lee pasa todas sus pruebas aunque los dos compartan el mismo
malentendido. Eso no es verificación, es un eco.

## Decisión

### Qué formato

1. **FAT32**, y se empieza **solo por lectura**. Escribir exige asignar
   clusters, mantener dos FAT coherentes y decidir qué pasa si se corta la
   corriente a medias; nada de eso hace falta para abrir un fichero, y
   todo ello se diseña mejor cuando ya hay un lector que funciona.
2. **Nombres 8.3, sin nombres largos.** Las entradas de nombre largo son
   un añadido de Windows que se guarda en entradas de directorio falsas
   con un checksum; se puede leer un disco entero sin tocarlas. Queda como
   límite dicho, no como olvido.
3. **Sin particiones**: el sistema de ficheros empieza en el sector 0 del
   disco. Una tabla de particiones es otra estructura que analizar para
   llegar al mismo sitio, y el disco es nuestro.

### Contra qué se comprueba

4. **La imagen la construye `xtask`**, no QEMU. El sintetizador `fat:` de
   QEMU puede presentar un directorio del host como un sistema de ficheros,
   y al pedirle FAT32 avisa: *"FAT32 has not been tested. You are welcome
   to do so!"*. Verificar un lector contra una implementación que se
   declara no probada no verifica nada.
5. **Y la comprueba `fsck.vfat`**, en CI, que es Linux. Es una
   implementación independiente, escrita por otra gente, que sabe qué es
   una imagen FAT32 válida. Si nuestro formateador y nuestro lector
   comparten un malentendido, `fsck` lo ve y nosotros no. Es la pieza que
   convierte las pruebas en verificación.
6. **El formateador es puro y se prueba en host**: de unos parámetros a
   los bytes de la imagen. Lo que escribe se comprueba campo a campo, y
   además lo mira `fsck`.

### Qué lee el kernel, y cómo se sabe que lo leyó bien

7. **El sector de arranque y su copia de seguridad.** FAT32 guarda una
   copia del sector 0 en el sector 6; el kernel lee los dos y comprueba
   que coinciden. Eso prueba tres cosas de una vez: que el BPB se analiza,
   que el número de sector llega al dispositivo —6 no es 0— y que la
   imagen que `xtask` escribió tiene su copia donde debe.
8. **El BPB se valida, no se cree.** Un sector de arranque es datos que
   vienen de fuera del kernel: bytes por sector, sectores por cluster, el
   número de FAT y el tamaño de cada una se comprueban antes de usarse
   para calcular una dirección. Un `sectors_per_cluster` de cero sale de
   una división por cero y un tamaño absurdo sale de una lectura fuera del
   disco.
9. **La aritmética de clusters vive en `hal` y es pura.** De un número de
   cluster a un número de sector hay tres multiplicaciones y una suma, y
   equivocarse en una lee el sitio equivocado sin dar ningún síntoma.

## Alternativas consideradas

- **Un sistema de ficheros propio**: todo el código sería nuestro y
  comprobable, y ninguna herramienta de fuera podría leer ni escribir el
  disco — ni comprobarlo, que es justo lo que el punto 5 aprovecha.
- **ext2 solo lectura**: mejor diseñado, con inodos y permisos de verdad
  que Fase 5 querrá, y más estructura que entender antes del primer byte.
  Sigue siendo la opción si FAT se queda corto.
- **FAT32 lectura y escritura de una vez**: la shell podría crear ficheros
  desde el primer día, con mucha más superficie donde equivocarse y sin un
  lector probado debajo.
- **El sintetizador de QEMU como imagen**: cero código de formateo, y la
  referencia sería una implementación que avisa de no estar probada.
- **`mkfs.vfat` para construir la imagen**: una implementación
  independiente construyéndola, lo cual es bueno, y una dependencia de
  herramientas de Linux para *construir* y no solo para comprobar. El
  reparto elegido —nosotros construimos, otro comprueba— deja el build
  funcionando en Windows y la verificación en CI.

## Consecuencias

- El disco pasa de 8 MiB a 64 MiB: FAT32 exige más de 65 525 clusters, y
  con menos la propia especificación dice que es FAT16. Un disco demasiado
  pequeño con un BPB que dice FAT32 es exactamente la clase de imagen que
  `fsck` rechaza y un lector descuidado acepta.
- Los dos marcadores en crudo del ADR 0024 desaparecen: el sector 0 pasa a
  ser el BPB. Lo que demostraban —que el número de sector llega— lo
  demuestra ahora la copia de seguridad del sector 6.
- CI gana una dependencia, `dosfstools`, y un paso que puede fallar por
  algo que no es el kernel. Es el precio de tener una segunda opinión.
