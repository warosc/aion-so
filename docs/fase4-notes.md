# Notas de Fase 4 — Almacenamiento y shell

Lo más reciente arriba. Salida de la fase (ROADMAP.md): arrancar, leer y
escribir ficheros desde una shell de usuario.

Decisiones de alcance tomadas al abrir la fase: **virtio-blk** como primer
driver de almacenamiento, **FAT32 empezando por solo lectura**, y **ELF64
estático** como formato ejecutable —el ADR 0014 dejó el binario plano
incrustado explícitamente como provisional "hasta que haya filesystem"—.

## Incremento 26 — El kernel pregunta qué hay

`docs/adr/0022-fase4-pci-enumeration.md`. Todo lo que el kernel tocaba
hasta ahora estaba en una dirección que alguien fijó hace cuarenta años:
el PIC en `0x20`, el PIT en `0x40`, el teclado en `0x60`. Un disco no.

### Qué hace

- **El espacio de configuración se lee por los puertos `0xCF8`/`0xCFC`**,
  no por ECAM: ECAM necesita la tabla MCFG de ACPI, que necesita un
  analizador de ACPI, que es un subsistema entero. Los puertos alcanzan
  los 256 buses y los primeros 256 bytes de cada función, que es donde
  viven las capacidades de virtio.
- **El recorrido es exhaustivo y sin recursión**, y solo pregunta por las
  funciones 1 a 7 cuando la función 0 dice que el dispositivo es
  multifunción: uno que no lo es puede contestar por las ocho, y el mismo
  disco aparecería ocho veces.
- **El kernel no configura nada mientras mira.** No asigna BAR, no
  habilita bus mastering, no dimensiona nada —eso exige escribir en el
  registro—. El firmware ya lo hizo. Enumerar es leer.
- **Todo menos `in` y `out` vive en `hal`**, sobre un rasgo `ConfigSpace`
  de una sola operación. La misma forma que `PageTables` sobre
  `TableAccess`: el recorrido entero se prueba contra una máquina que no
  existe.
- **`xtask` le da un disco a la máquina**: una imagen cruda enganchada
  como `virtio-blk-pci`. Nada arranca desde él; el firmware sigue
  arrancando desde la ESP.

### Verificación ejecutada

- QEMU, que es lo que lo demuestra —la máquina entera, encontrada:

  ```
  pci 00:00.0 8086:1237 host bridge (class 06.00)
  pci 00:01.0 8086:7000 ISA bridge (class 06.01)
  pci 00:01.1 8086:7010 IDE storage (class 01.01)
  pci 00:01.3 8086:7113 bridge (class 06.80)
  pci 00:02.0 1234:1111 display (class 03.00)
  pci 00:03.0 1af4:1001 SCSI storage (class 01.00)
  a virtio disk at pci 00:03.0 (1af4:1001), first BAR 0xc001
  ```

  Es exactamente lo que `-machine pc` emula, con nuestro disco en `00:03.0`
  y ni una función repetida. El puente PIIX3 en `00:01.0` **sí** es
  multifunción, así que el camino de las funciones 1 a 7 lo recorre
  también un arranque de verdad, no solo las pruebas.
- Host: 258 pruebas (246 + 12: la dirección de configuración campo por
  campo, la cabecera, la tabla de dispositivos, y el recorrido contra una
  máquina falsa que registra qué se le preguntó).
- **Coste**: ninguno medible. Diez arranques entre 5,24 s y 5,48 s, dentro
  del margen de antes del escaneo (5,68–6,69 s en el incremento previo, en
  la misma máquina). Los ~8200 accesos a puerto no se notan.
- `boot-test --repeat 10` 10/10, soak de 120 s PASS, `fmt-lint` limpio.
- Mutación: 15, las 15 detectadas. Todas alcanzables desde host porque el
  recorrido está en `hal`: desplazar el bus un bit, leer la clase donde
  está la subclase, buscar el bit de multifunción en el sitio equivocado,
  recorrer un solo bus, preguntar por las ocho funciones siempre.

### Lo que hay que saber para el siguiente incremento

- **El dispositivo es transicional**: QEMU presenta `1af4:1001`, no
  `1af4:1042`. Es decir, habla virtio legacy por un BAR de E/S **y**
  virtio 1.0 por capacidades PCI. Cuál de los dos usar es la decisión del
  ADR del driver.
- **El FAT32 sintético de QEMU no sirve de referencia.** Enganchar un
  directorio con `fat:32:rw:` funciona, pero QEMU avisa: *"FAT32 has not
  been tested. You are welcome to do so!"*. Verificar un lector contra una
  implementación no probada no verifica nada. El incremento del filesystem
  tendrá que construir la imagen y comprobarla con algo independiente
  —`fsck.vfat` en CI, que es Linux— en vez de confiar en el sintetizador.

### Riesgos y límites

- La configuración extendida de PCIe (de `0x100` en adelante) es
  inalcanzable por este camino. Nada de lo que este kernel usa la
  necesita todavía.
- 32 funciones como máximo. Pasado eso el kernel dice cuántas perdió.
- No se sigue ningún puente: los 256 buses se visitan a pelo. Encuentra lo
  mismo en esta máquina y es menos código.
