# ADR 0007: El kernel deja de depender de la memoria del firmware

## Contexto

Los ADR 0004 y 0005 dejaron el kernel a medio camino: tiene su propia tabla
raíz, pero las tablas de niveles inferiores del mapa de identidad siguen
siendo del firmware; y aunque desde el Incremento 10 corre sobre una pila
propia, el mapa de memoria, el bitmap de marcos y la consola siguen viviendo
en la pila del firmware. Por eso la memoria de boot services seguía
retenida: unos 43 MiB, el 17 % de la RAM en QEMU. Y la página 0 seguía
mapeada con escritura, así que una desreferencia nula no fallaba.

El ADR 0004 fijó la condición para recuperar esa memoria: "que el kernel ya
corra sobre pila y CR3 propios". Este incremento la cumple.

Es la tercera de las tres medidas que el usuario priorizó tras cerrar la
Fase 2 (ver `docs/memory-safety.md`).

## Decisión

1. **Contrato de arranque**: `kmain` pasa a recibir `BootInfo`, la consola y
   el control de energía **por valor**, no por referencia. `boot` se los
   entrega y deja de ser su dueño. Es lo que permite al kernel moverlos.
2. **Todo lo vivo se muda al heap**: en cuanto hay mapper, heap y pila
   propia, el kernel copia al heap el mapa de memoria y el bitmap de marcos,
   y mueve allí la consola y el control de energía. El asignador se
   reconstruye sobre esas copias con `BitmapFrameAllocator::adopt`, que
   conserva exactamente qué marcos estaban ya entregados. El contexto del
   kernel vive también en el heap, así que el cambio de pila no deja nada
   atrás.
3. **Mapa de identidad propio**: `rebuild_identity_map` construye, con
   marcos del kernel, un mapa de identidad de `0..4 GiB` en páginas de
   2 MiB —los primeros 2 MiB a 4 KiB— y lo instala como toda la mitad baja,
   **dejando la página 0 sin mapear**. Son 6 tablas. A partir de ahí no se
   lee ni se usa ninguna tabla del firmware. Lo que el firmware mapeaba por
   encima de 4 GiB (hasta 1 TiB) se descarta: no hay nada ahí que el kernel
   use.
4. **Recuperación de la memoria de boot services**: solo si lo anterior
   salió bien, `reclaim_boot_services` añade esas regiones al pool, con las
   mismas reglas conservadoras de siempre (nunca un marco que toque una
   región reservada, nunca la página 0, nunca uno ya entregado). Es
   idempotente.
5. **Si algo de esto falla**, no es fatal y se registra: el kernel sigue con
   las tablas del firmware y su memoria queda retenida, como antes.

## Alternativas consideradas

- **Seguir sin recuperar la memoria**: se pierden ~43 MiB y, sobre todo, la
  página 0 sigue mapeada, que es un fallo de seguridad de memoria real (un
  puntero nulo escribe en memoria válida).
- **Desmapear solo la página 0 sobre las tablas del firmware**: imposible,
  son de solo lectura y CR0.WP está activo (ADR 0005).
- **Mapa de identidad de toda la RAM detectada en vez de 4 GiB fijos**:
  aplazado. 4 GiB cubren la RAM que el asignador puede gestionar (256 MiB),
  el framebuffer y el MMIO heredado en todas las máquinas en las que este
  kernel ha corrido, con 6 tablas y sin lógica de descubrimiento.
- **Mapa de identidad con W^X** (no ejecutable salvo el código del kernel):
  aplazado, y anotado como hueco. Haría falta saber dónde empieza y acaba
  la imagen cargada, que solo da el protocolo `LoadedImage` **antes** de
  salir de boot services. Hoy la mitad baja queda escribible y ejecutable,
  igual que la dejaba el firmware.

## Consecuencias

- En QEMU con 256 MiB: **+10 998 marcos (42 MiB)**, de ~51 800 a 62 815
  libres.
- Un puntero nulo ahora provoca `#PF accessing 0x0` (verificado).
- `BootInfo` y la consola ya no viven en memoria del firmware, así que el
  kernel puede entregar esa memoria sin más cuidado.
- Los *runtime services* de UEFI siguen funcionando (`reboot` y `shutdown`
  verificados tras la recuperación): viven en memoria de tipo runtime, que
  sigue reservada.
- El ADR 0004 queda cumplido en su condición de recuperación; su regla de
  retención sigue vigente para cualquier kernel que aún no haya hecho estos
  pasos.
- Queda pendiente W^X en la mitad baja (ver arriba) y, con ello, marcar la
  memoria de datos como no ejecutable.
- Detalle de la verificación: `docs/fase2-notes.md`, Incremento 11.
