# ADR 0022: Cómo el kernel encuentra el hardware

## Contexto

Fase 4 empieza por un driver de almacenamiento, y antes de hablar con un
dispositivo hay que encontrarlo. Hasta ahora todo lo que el kernel toca
está en una dirección fija que alguien decidió hace cuarenta años: el PIC
en `0x20`, el PIT en `0x40`, el teclado en `0x60`. Un disco no.

PCI es cómo se pregunta qué hay. La pregunta de este ADR es **por dónde**
se pregunta, y qué hace el kernel con la respuesta.

## Decisión

1. **El espacio de configuración se lee por los puertos `0xCF8`/`0xCFC`**,
   no por ECAM. ECAM es el mecanismo moderno y mapeado en memoria, y
   requiere la tabla MCFG de ACPI, que requiere un analizador de ACPI, que
   es un subsistema entero. Los puertos alcanzan los 256 buses, las 8
   funciones y los **primeros 256 bytes** de configuración de cada una,
   que es todo lo que hace falta: las capacidades de virtio viven ahí.
   El día que haya ACPI —lo habrá, para apagar bien y para SMP— ECAM pasa
   a ser posible y esto se revisa.
2. **Lo que queda fuera queda dicho**: la configuración extendida de PCIe,
   de `0x100` en adelante, es inalcanzable por este camino. Ningún
   dispositivo que este kernel vaya a usar la necesita todavía.
3. **El recorrido es exhaustivo y sin recursión**: los 256 buses, 32
   dispositivos por bus, y las funciones 1 a 7 solo si la función 0 dice
   que es multifunción. Un bus vacío cuesta 32 lecturas y termina. La
   alternativa —seguir los puentes desde el bus 0— es lo que hace un
   sistema grande, y aquí sería más código para encontrar lo mismo.
4. **El kernel no configura nada mientras mira.** No asigna BAR, no
   habilita bus mastering, no toca líneas de interrupción y **no
   dimensiona los BAR** —lo que exige escribir unos en el registro y
   restaurarlo—. El firmware UEFI ya asignó las direcciones antes de
   dárnoslo; rehacerlo sería pelearse con él. Enumerar es leer.
5. **Lo encontrado cabe en una tabla de tamaño fijo**, sin asignar
   memoria: 32 funciones. Pasado ese número el kernel lo dice y sigue con
   las que tiene, porque quedarse sin sitio es un hecho que contar, no un
   `Vec` creciendo dentro del arranque.
6. **Todo menos las dos instrucciones vive en `hal`**, sobre un rasgo
   `ConfigSpace` con una sola operación: leer un dword de una función. La
   decodificación de la cabecera y **el recorrido entero** —qué buses, qué
   funciones, cuándo preguntar por las otras siete— quedan de este lado, y
   se prueban contra una máquina que no existe. `arch` conserva `in` y
   `out`, que es lo único que de verdad depende de x86_64.
   Es la misma forma que `PageTables` sobre `TableAccess`: lo que se puede
   probar sin hardware se prueba sin hardware, porque un desplazamiento de
   cuatro bits no debería descubrirse en un arranque.
7. **Una dirección de configuración es un tipo**, no tres enteros sueltos:
   `Address { bus, device, function }` sabe construir el valor de `0xCF8`,
   y es lo único que lo sabe.

## Alternativas consideradas

- **ECAM con ACPI**: lo correcto a medio plazo, y hoy significa escribir
  el analizador de ACPI antes que el driver de disco. El orden estaría al
  revés: ACPI hará falta por sus propias razones, no por esta.
- **Recorrer siguiendo los puentes** desde el bus 0: menos lecturas y más
  código, con recursión en el arranque. Con la máquina que emula QEMU
  encuentra exactamente lo mismo.
- **Dar por hecho dónde está el disco**, como se hace con el PIT: funciona
  hasta que alguien cambia la línea de órdenes de QEMU, y no enseña nada.
- **Dimensionar los BAR al enumerar**: útil para un mapa completo de la
  máquina, y escribe en registros de dispositivos que el firmware acaba de
  configurar. El driver que use un BAR lo dimensionará cuando lo necesite,
  sabiendo qué dispositivo está tocando.

## Consecuencias

- El kernel gana dos instrucciones de puerto nuevas (`in`/`out` de 32
  bits) y un módulo que las usa. Ninguna otra parte del kernel las
  necesita.
- La máquina de pruebas gana un disco: `xtask` crea una imagen y la
  engancha como dispositivo virtio, que es el almacenamiento que el
  ROADMAP pide y el que el kernel va a aprender a leer. Nada arranca desde
  él; el firmware sigue arrancando desde la ESP.
- El recorrido cuesta unos miles de accesos a puerto en el arranque. Se
  mide en las notas; si pesara, el punto 3 es lo primero que se recorta.
- Lo que el kernel sabe del hardware deja de ser una lista de constantes y
  pasa a ser algo que descubre. El siguiente ADR —el driver de virtio—
  puede pedir "el dispositivo con este identificador" en vez de una
  dirección escrita a mano.
- No se toca el mapa de memoria: enumerar no mapea nada. Mapear los
  registros de un dispositivo es una decisión del driver, con su propia
  política de caché, y tendrá su ADR.
