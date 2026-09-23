# ADR 0016: Los runtime services se mudan a la mitad alta

## Contexto

El firmware sigue viviendo en la mitad baja. El ADR 0013 la vació salvo sus
rangos, y el 0015 los conservó todos y bien; pero la mitad baja va a ser
espacio de usuario, y ahí no puede quedar nada del firmware: un proceso
tendría el flash de la máquina mapeado dentro de su espacio, y el kernel
tendría que malabarear con CR3 en cada `reboot`.

UEFI tiene una salida para esto, `SetVirtualAddressMap`: se le dice al
firmware dónde estarán sus cosas a partir de ahora y él reubica sus propios
punteros. Se puede llamar **una sola vez por arranque**, y solo funciona si
los mapeos nuevos existen antes de llamar.

La revisión cruzada de Codex recomendó hacerlo **antes** de que haya
procesos, en un incremento propio, en vez de arrastrar un CR3 especial para
las llamadas al firmware. Oscar lo aprobó así.

## Decisión

1. **Una ventana para los runtime services en PML4 261**
   (`KERNEL_RUNTIME_START`): cada rango con `EFI_MEMORY_RUNTIME` se mapea en
   `KERNEL_RUNTIME_START + su dirección física`, con la caché que el
   descriptor declaró (ADR 0015) y ejecutable solo si es `RuntimeCode`.
   Traducir una dirección pasa a ser una suma, que es lo que hace falta
   para rellenar el mapa que el firmware espera.
2. **La llamada la hace el cargador, la decisión la toma el kernel.**
   `boot` es la única parte del sistema que habla UEFI, así que conserva el
   mapa que recibió de `ExitBootServices` y expone **una función** que el
   kernel llama con la base que ha elegido. El kernel no aprende a hablar
   UEFI; el cargador no decide el layout de memoria.
3. **El orden es el que exige la especificación**: mapear primero, llamar
   con los mapeos físicos todavía presentes, y solo después vaciar.
4. **Si el firmware se niega, no se vacía nada.** La mitad baja se queda
   con sus rangos identity-mapped, como la dejó el ADR 0015, y se registra.
   Un `reboot` que funciona con el mapa viejo vale más que una mitad baja
   limpia y una máquina que no se apaga.
5. **La tabla del sistema se reubica también**: la API del crate `uefi`
   actualiza su puntero global cuando la llamada tiene éxito, que es lo que
   hace que `reboot` y `shutdown` sigan encontrando al firmware.
6. **`PageFlags` gana la política de caché.** Hasta ahora solo la conocían
   los rangos que sobrevivían en la mitad baja; para mapear el flash del
   firmware en la mitad alta hace falta en el mapeo normal.

## Alternativas consideradas

- **Un CR3 dedicado al firmware**, con sus rangos identity-mapped y solo de
  supervisor, al que cambiar en cada llamada: no es irreversible y evita
  depender de que el firmware implemente bien `SetVirtualAddressMap`, pero
  arrastra ese CR3 para siempre y hay que acertar en cada llamada. Es el
  plan B si la llamada falla en hardware real.
- **Dejar de llamar al firmware**: apagar por el puerto ACPI y reiniciar
  por el 0xCF9. Quita el problema de raíz y cambia portabilidad por
  dependencia de detalles de plataforma. Sigue siendo una opción si
  `SetVirtualAddressMap` resulta ser un campo de minas.
- **Llamarla desde el cargador, antes de entregar el control**: el
  cargador tendría que construir las tablas del kernel, que es justo lo que
  este proyecto le quitó en Fase 2.
- **Posponerlo hasta que estorbe**: es lo que Codex desaconsejó, y tiene
  razón: cuanto más tarde, más código habrá que asuma que el firmware está
  donde estaba.

## Consecuencias

- La mitad baja queda **completamente vacía**, lista para ser espacio de
  usuario sin un solo rango prestado.
- `reboot` y `shutdown` pasan a llamar al firmware en sus direcciones
  nuevas. Es el único test que importa de este incremento, y falla ruidoso:
  o apaga, o no.
- `BootInfo` gana un puntero a función. El contrato de arranque deja de ser
  solo datos, y eso se dice aquí: es la única forma de que el kernel mande
  sin aprender UEFI.
- La llamada es irreversible dentro de un arranque. Si falla a medias —el
  firmware relocaliza parte y devuelve error— no hay vuelta atrás; por eso
  no se vacía nada hasta que devuelve éxito, y por eso el fallback es
  quedarse con el mapa viejo.
- Deja de haber excusa para que un proceso vea memoria del firmware.
