# ADR 0013: La mitad baja deja de ser del kernel

## Contexto

El ADR 0012 sacó el **código** del kernel de la mitad baja. Seguía
dependiendo de ella para tres cosas:

- **La ventana física**: `PhysWindow` era el mapa de identidad
  (`base == 0`). Todo marco que el kernel pone a cero se escribe por ahí, y
  las tablas de páginas se leen y escriben por su dirección física.
- **El framebuffer**: la consola dibuja directamente en él, por su
  dirección física.
- **El registro**: el logger del `log` crate es un `&'static dyn Log`
  registrado por el cargador; sus dos mitades —dato y vtable— apuntan a la
  imagen donde el firmware la cargó.

Fase 3 necesita la mitad baja entera para los procesos, así que hay que
cortar las tres.

Cambia el layout de memoria y el contrato de la consola, así que lleva ADR.

## Decisión

1. **Una ventana onto toda la memoria física en PML4 260**
   (`KERNEL_PHYSMAP_START`): `[0, 4 GiB)` en páginas de 2 MiB, escribible y
   **nunca ejecutable**. `PhysWindow` pasa a tener esa base, y el paginador
   alcanza las tablas por `base + dirección física` en vez de por la
   dirección física a secas.
2. **La consola sigue a su framebuffer**: `Console::framebuffer_moved` le
   dice dónde está ahora, y `FramebufferSurface::rebase` cambia el puntero.
   La consola que no dibuja en memoria lo ignora.
3. **El registro deja de pasar por el `log` crate.** `hal::klog` es una
   fachada con `info!`/`warn!`/`error!` y un **puntero a función** que el
   kernel puede reapuntar cuantas veces quiera: una al arrancar y otra
   desde su dirección nueva. El sumidero sigue siendo el puerto 0xE9.
4. **La mitad baja se vacía**, salvo lo que el firmware necesita para sus
   runtime services: su código (ejecutable y escribible, ADR 0011) y **sus
   datos** (escribibles, no ejecutables). `hal` distingue ahora
   `RuntimeData` del resto de lo reservado.
5. **Si algo no cuadra, no se vacía**: un `page_count` que se desborda en
   cualquiera de esos rangos deja la mitad baja como estaba y lo registra.

## Alternativas consideradas

- **Seguir con el `log` crate y reinstalar el logger tras la mudanza**:
  imposible. `set_logger_racy` devuelve error si ya hay uno —comprobado en
  su código, no supuesto— y cualquier `&'static dyn Log` apunta a la
  imagen. El primer intento lo hizo con `let _ =`, que escondió el error;
  el fallo salió como un `#PF` leyendo `.data` en su dirección vieja.
- **Registrar el logger solo después de la mudanza**: perdería todas las
  líneas del cargador y del arranque temprano, que es justo donde se
  diagnostica un arranque roto.
- **Mapear el framebuffer en un hueco propio de espacio de kernel**: la
  ventana ya lo cubre; un hueco aparte sería una tabla más para lo mismo.
  Cuando la consola sea un driver de verdad, se replanteará.
- **Llamar a `SetVirtualAddressMap`** para reubicar los runtime services y
  soltar del todo la mitad baja: es la solución definitiva y la UEFI spec
  solo deja llamarla una vez. Queda para cuando haya procesos y haga falta
  de verdad.
- **Dejar la mitad baja mapeada y dar a los procesos otro trozo**: ya
  descartado en el ADR 0012.

## Consecuencias

- El kernel no toca la mitad baja para nada suyo: comprobado por la vía
  negativa, leer `0x10_0000` da `#PF accessing 0x100000, error_code=0x0`.
- En QEMU: la ventana cuesta 5 tablas (4 GiB en páginas de 2 MiB) y lo que
  queda abajo son **5 rangos del firmware en 7 tablas**; todo lo demás,
  desmapeado. `shutdown` apaga y `reboot` rearranca.
- **Los runtime services del firmware siguen en la mitad baja**, que será
  espacio de usuario. Mientras el shell corra en el kernel no hay conflicto;
  en cuanto haya procesos, una llamada a `reboot` o `shutdown` tendrá que
  hacerse con el CR3 del kernel (o reubicar los servicios). Es el cabo
  suelto que hereda el incremento de procesos.
- El kernel deja de depender del crate `log`.
- La consola gana un método con implementación por defecto; ninguna
  implementación existente se rompe.
