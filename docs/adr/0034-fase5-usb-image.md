# ADR 0034: La imagen USB

## Contexto

Bullet de Fase 5: *imagen USB UEFI*. Y hasta ahora no había ninguna imagen
que nadie pudiera escribir en un stick.

Lo que QEMU arranca es `-drive format=raw,file=fat:rw:target/esp`: un
**directorio** que el emulador finge que es un volumen FAT. Al lado hay un
`disk.img` suelto con los datos. Ninguna de las dos cosas existe fuera de
QEMU. Para arrancar el PC objetivo hace falta **un fichero** que se escriba
tal cual con `dd`.

## Decisión

### Particionada, no un volumen pelado

1. **GPT, con MBR protector.** Un medio extraíble sin particionar lo arranca
   bastante firmware, y "bastante" es justo el problema: la máquina en la
   que esto tiene que funcionar es una que nadie de aquí puede probar antes.
   Una GPT es lo que escribe cualquier instalador, y es lo que funciona
   siempre.
2. **Dos particiones**: una ESP con el cargador, y el volumen HARLAN al
   lado. La segunda es exactamente la imagen que `prepare_disk` ya produce,
   metida dentro.
3. **El MBR protector cubre el disco entero y dice `0xEE`.** No es para
   arrancar —el firmware usa la GPT— sino para que una herramienta que solo
   entiende la tabla vieja vea algo que no entiende, en vez de algo que cree
   vacío y se ofrece a inicializar.

### El camino que UEFI exige

4. **`\EFI\BOOT\BOOTX64.EFI`, y nada más.** La especificación lo fija para
   un medio extraíble: el firmware busca ahí y no busca en otro sitio. Eso
   obligó al formateador a saber hacer directorios, porque solo sabía
   escribir la raíz.
5. **El formateador hace directorios; el kernel sigue sin leerlos.** Son dos
   cosas distintas y conviene no confundirlas: `xtask` escribe un volumen que
   el firmware de otro tiene que poder leer, y el ADR 0028 sigue diciendo que
   el lector del kernel ve nombres 8.3 en la raíz y nada más.

### Lo que se puede reproducir

6. **GUID fijos, no aleatorios.** Lo correcto sería aleatorios, y haría que
   cada build diera una imagen distinta, lo que convierte *"¿ha cambiado la
   imagen?"* en una pregunta que nadie puede contestar. La build
   reproducible es una promesa de Fase 0.
7. **Todo en orden determinista**, incluido el recorrido del árbol de
   directorios, por lo mismo.

### Tamaños

8. **La ESP son unos 34 MB** porque FAT32 no es FAT32 con menos de 65 525
   clusters, y con clusters de 512 bytes eso es el mínimo. El cargador son
   unos 550 KB; el resto es el precio de usar **un** formateador para las dos
   particiones en vez de dos.
9. **La imagen entera son 100 MB.** Cabe en cualquier stick y se escribe en
   segundos.

## Lo que esto **no** hace

10. **El kernel arrancado desde el stick no lee el stick.** Un stick USB no
    es un disco virtio: para leer la partición de datos hace falta un driver
    de almacenamiento USB, que es el bullet **siguiente** de la fase
    (*drivers mínimos de entrada, pantalla, almacenamiento y red*).

    Lo que pasa hoy al arrancar desde el USB es exactamente lo que debe
    pasar: `no virtio storage on the bus`, ningún programa que cargar, y la
    shell del kernel —el camino de reserva del ADR 0030, punto 1— arriba.
    Decirlo aquí es parte de la decisión: una imagen que arranca y no
    encuentra sus datos es media cosa, y media cosa anunciada como entera es
    peor que media cosa.

11. **Una idea para ese bullet, anotada aquí para no perderla**: el cargador
    corre bajo UEFI y puede leer bloques con los servicios del firmware
    **antes** de salir de ellos. Leer lo que el kernel necesita por ahí
    evitaría escribir un driver USB para el camino de lectura. Escribir
    seguiría necesitando uno. Es un ADR aparte.

## Alternativas consideradas

- **Un volumen FAT sin particionar en todo el dispositivo**: diez líneas de
  `xtask` y ningún trabajo de GPT, y depende de que el firmware de una
  máquina concreta acepte un medio sin tabla. No se puede comprobar aquí
  cuál es esa máquina.
- **MBR en vez de GPT**: más simple, y es lo viejo; UEFI prefiere GPT y
  algunas implementaciones solo arrancan de GPT en medios grandes.
- **Una sola partición que sea a la vez ESP y volumen de datos**: una
  partición menos, y mezcla lo que el firmware mira con lo que el kernel
  escribe — y el ADR 0033 acaba de decidir que el kernel solo escribe en un
  volumen que sea suyo. Un volumen que es de los dos no es de ninguno.
- **GUID aleatorios por build**: lo correcto para discos de verdad, y rompe
  la reproducibilidad. Cuando haya que escribir sticks en serie, se genera
  uno por stick y se dice de dónde sale.
- **Meter el `disk.img` como fichero dentro de la ESP**: una partición, y el
  kernel tendría que leer un fichero para encontrar un sistema de ficheros
  dentro, que es una vuelta de más.

## Consecuencias

- `cargo xtask usb-image` produce `target/harlan-usb.img`, y dice con qué
  orden de `dd` se escribe — **con el aviso de que eso sobrescribe el
  dispositivo que se le diga**, que es el mismo cuidado que el ADR 0033 pone
  dentro del kernel.
- El formateador de `xtask` sabe hacer directorios. Eso es más superficie, y
  está probado contra 7-Zip, que es quien tiene que entenderlo.
- Las dos imágenes siguen existiendo: `disk.img` para QEMU, que es rápido de
  rehacer, y la USB para una máquina. La segunda contiene a la primera.

## Lo que se midió

- **Arranca.** QEMU con el stick y **nada más** —sin directorio ESP, sin
  segundo disco—: si el firmware no lee la GPT y el FAT32 que escribimos, no
  pasa nada en absoluto. Llega hasta `HARLAN-PHASE1-SHELL-READY`, y por el
  puerto serie salieron 12 817 bytes del mismo arranque.
- **Lo lee otro.** 7-Zip abre la imagen como GPT, nombra las dos particiones
  y lista `EFI\BOOT\BOOTX64.EFI` dentro de la ESP con sus dos directorios.
- **Las sumas de comprobación son las de todo el mundo.** La CRC-32 está
  comprobada contra valores conocidos, no contra una segunda copia de sí
  misma — y esa prueba encontró que mi constante esperada para `"abc"` estaba
  mal, no la implementación.
- Y dos fallos que encontró 7-Zip **antes** de que ningún firmware viera la
  imagen: el cargador estaba en la raíz en vez de en `\EFI\BOOT\`, y la
  entrada de directorio llevaba la ruta entera en vez del último nombre.
