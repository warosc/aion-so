# ADR 0035: El inventario de hardware

## Contexto

Bullet de Fase 5: *inventario de hardware del equipo objetivo*. No se puede
escribir desde aquí: nadie de este lado de la pantalla ha visto la máquina.
Lo que sí se puede escribir es **la cosa que lo averigua**.

Y hace falta antes que nada de lo que viene después. El bullet siguiente es
*drivers mínimos de entrada, pantalla, almacenamiento y red*, y elegir qué
drivers escribir sin saber qué hay dentro es escribir a ciegas.

## Decisión

### Dónde corre, y por qué ahí

1. **En el cargador, bajo UEFI, antes de `ExitBootServices`.** No es un
   detalle: es la única razón por la que esto funciona. Mientras los
   servicios del firmware existen, se puede **escribir un fichero en el
   stick del que arrancó** sin tener driver para el dispositivo en el que
   esté enchufado — USB, NVMe, lo que sea. El firmware ya sabe hablarle.
2. **El informe se escribe en `INVENTORY.TXT`, en la raíz de la ESP.** La
   persona arranca el stick, lo vuelve a enchufar a un ordenador normal, y
   lee un fichero de texto. Sin fotografiar una pantalla, sin transcribir, y
   sin cable serie.
3. **Y también sale por pantalla y por el registro.** El fichero es la copia
   buena; la pantalla es la que no necesita nada.

### En qué volumen escribe

4. **En el que este programa fue cargado, y en ningún otro.** El manejador
   sale del protocolo de imagen cargada: no es "un disco" ni "el primero que
   haya". Es el mismo cuidado que el ADR 0033 pone dentro del kernel, por el
   único medio disponible a este lado de `ExitBootServices`.
5. **Medido**: tras un arranque, un byte cambiado dentro de la ESP y **cero
   en la partición de datos**. Nada fuera de las dos.

### Sin asignar memoria

6. **Todo en un buffer fijo.** El heap del kernel es el único asignador
   enlazado y no está inicializado durante la fase de arranque, así que un
   `String` sería un puntero nulo. El informe se trunca antes que crecer, y
   **dice de sí mismo que se ha truncado** en vez de acabarse a media frase
   y dejar al lector preguntándose.

### Qué cuenta

7. **Lo que decide qué drivers hacen falta y qué supuestos de este kernel
   son ciertos en esa máquina**: el firmware y su versión; el procesador —
   fabricante, nombre, familia/modelo, bits de dirección reales, y las
   funciones de las que dependen cosas ya escritas (`nx` para W^X, `syscall`
   para el ABI, páginas de 1 GiB para la ventana física); el mapa de memoria;
   el framebuffer; y **todas las funciones PCI** con su clase.
8. **La memoria se lista por los tipos que el firmware les da**, no por una
   clasificación nuestra. Dos intentos de resumirla en "usable" y
   "reservada" salieron engañosos —una máquina de 512 MiB informaba de doce
   gigabytes reservados, que es el tipo de número que manda a alguien a
   buscar una avería que no existe—. Adivinar qué tipos son memoria es el
   error: el firmware sabe lo que quiere decir, y el lector lo puede ver.
9. **Si el escaneo PCI pierde funciones, lo dice en mayúsculas.** Un
   inventario que se calla lo que no cupo manda a alguien a escribir un
   driver para un dispositivo que no es el suyo. El límite sube de 32 a 96
   por lo mismo.

### Qué no hace

10. **No escribe en ningún disco de la máquina.** Solo en el volumen del que
    arrancó. Es un stick que se mete en el ordenador de alguien: la única
    promesa que importa es que no toca nada suyo.
11. **No es ACPI.** Las tablas ACPI dicen más —cuántos núcleos, qué
    interrupciones, qué energía— y son otro analizador. Cuando el inventario
    de verdad diga que hace falta.

## Alternativas consideradas

- **Enseñarlo solo por pantalla**: cero trabajo de ficheros, y obliga a
  fotografiar y transcribir, que es donde se pierden los datos que importan
  —un `vendor:device` mal copiado manda a escribir el driver equivocado—.
- **Solo por el puerto serie**: perfecto para quien tiene un adaptador, e
  inútil para quien no. El serie se queda; no es el camino principal.
- **Enumerar desde el kernel, después de `ExitBootServices`**: el kernel ya
  escanea PCI y podría hacerlo. Y entonces no hay forma de escribir el
  resultado en el stick sin un driver de almacenamiento, que es justo lo que
  este inventario existe para poder elegir.
- **Un segundo binario de diagnóstico**: más limpio de leer, y duplica el
  camino de arranque entero para enumerar lo mismo. Una `feature` sobre el
  cargador que ya existe es menos cosa que mantener.
- **Escribir el informe en la partición de datos** en vez de en la ESP: es
  donde el kernel guarda lo suyo, y es la que no se puede leer desde Windows
  ni macOS sin herramientas. La ESP sí.

## Consecuencias

- `cargo xtask usb-image --inventory` produce `target/harlan-inventory.img`,
  con un nombre distinto del stick normal para que no se confundan encima de
  una mesa.
- La `feature` es solo del cargador, así que `build_commands` pasa a saber
  qué `feature` es de qué crate — cargo rechaza de plano un `--features` que
  nombre una que el crate no tiene.
- `cpuid` entra en `arch`. Lo que lee es una instrucción; lo interesante es
  la decodificación, y esa se prueba en host contra lo que dicen los
  manuales — incluido que las dos "hojas 1" (la ordinaria y la extendida) no
  son intercambiables, que es el error fácil porque en conversación se
  llaman igual.

## Lo que se midió

- Arranca desde el stick y escribe: `INVENTORY.TXT written to the volume this
  booted from, 1604 byte(s)`.
- **Y lo lee otro.** 7-Zip saca la ESP de la imagen y el fichero de dentro,
  con el informe entero: firmware EDK II 2.70, el procesador con
  `hypervisor=yes` —que es verdad, es una máquina virtual—, 104 regiones de
  memoria por tipo, el framebuffer, y las seis funciones PCI incluido el
  controlador USB en el que está el propio stick.
- Un byte cambiado en la ESP tras un arranque, cero en la partición de
  datos.
