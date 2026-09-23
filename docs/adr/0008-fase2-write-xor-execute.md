# ADR 0008: La mitad baja deja de ser ejecutable salvo donde hay código

## Contexto

El ADR 0007 dejó al kernel con su propio mapa de identidad, pero con una
limitación anotada: toda la mitad baja quedaba **escribible y ejecutable**,
igual que la dejaba el firmware. Eso significa que cualquier salto erróneo a
una página de datos —el heap visto por su alias de identidad, un marco
recién asignado, el framebuffer— ejecutaría lo que hubiera allí en vez de
fallar.

Cerrarlo exige saber qué rangos contienen código que el kernel todavía
ejecuta a través de ese mapa. Son dos:

- **La imagen del kernel**, cuyo rango solo conoce el firmware. El protocolo
  `LoadedImage` lo da, pero únicamente **antes** de `ExitBootServices`.
- **El código de los runtime services de UEFI**, que `reboot` y `shutdown`
  llaman. El mapa de memoria lo identifica con el tipo
  `EfiRuntimeServicesCode`, que hasta ahora el clasificador de `hal`
  mezclaba con todo lo demás en `Reserved`.

Es un cambio de permisos del layout de memoria y añade un campo al contrato
de arranque, así que lleva ADR.

## Decisión

1. **`boot` pregunta dónde está cargada la imagen** (`LoadedImage`), junto
   al framebuffer y antes de salir de boot services, y lo entrega en
   `BootInfo::kernel_image` como dato plano (`PhysRange`). Si el firmware no
   lo dice, el campo es `None`.
2. **`hal` distingue `MemoryRegionKind::RuntimeCode`** (tipo UEFI 5). Los
   datos de runtime services siguen siendo `Reserved`: no se ejecutan.
   Ninguna de las dos es asignable, como antes.
3. **El mapa de identidad se construye no ejecutable**, salvo las páginas
   que caen en los rangos que se le pasan: la imagen del kernel y todas las
   regiones `RuntimeCode`. Solo los bloques de 2 MiB que contienen esos
   rangos se parten en páginas de 4 KiB; el resto sigue siendo una página
   grande, ahora con NX.
4. **Si no se conoce ningún rango ejecutable**, el kernel **no** reconstruye
   el mapa: se queda con el del firmware y lo registra. Un mapa sin código
   ejecutable haría fallar la siguiente instrucción.

## Alternativas consideradas

- **Marcar ejecutable el bloque de 2 MiB que contiene la imagen**: más
  simple, pero deja ejecutables hasta 2 MiB de datos vecinos. Partir solo
  esos bloques cuesta una tabla y da granularidad de 4 KiB.
- **W^X dentro de la propia imagen** (código de solo lectura, datos no
  ejecutables): exige interpretar las secciones PE de la imagen cargada.
  Queda pendiente; hoy la imagen entera es escribible y ejecutable, que es
  como la dejaba el firmware.
- **No marcar ejecutable el código runtime del firmware**: probado, falla.
  `reboot` y `shutdown` saltan a él.

## Consecuencias

- En QEMU: 327 páginas ejecutables (71 de la imagen del kernel y 256 del
  código runtime del firmware) de 4 GiB mapeados; todo lo demás con NX.
- Verificado por la vía negativa: ejecutar desde un marco de datos produce
  `#PF ... error_code=0x11` (página presente, fallo al buscar instrucción).
- `reboot` y `shutdown` siguen funcionando.
- `BootInfo` gana un campo; el contrato de arranque queda como lo dejó el
  ADR 0007, más este dato.
- Pendiente: W^X dentro de la imagen del kernel (ver alternativas).
