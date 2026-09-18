# ADR 0002: Fase 2 sale de UEFI Boot Services

## Contexto

`docs/adr/0001-fase0-boot-path.md` predijo este momento: "Cuando Fase 1
reemplace este arranque por un loader real..., `ExitBootServices`, eso
constituye una 'sustitución del boot path' y requiere un ADR formal."
Fase 1 no lo disparó (documentado en `docs/fase1-notes.md`). Fase 2
Incremento 1 (GDT/IDT/excepciones) tampoco — no toca `ExitBootServices` ni
cambia `BootInfo`. Este incremento (2) sí: `ROADMAP.md` exige temporizador
real y entrada de teclado por interrupción, y UEFI Boot Services no ofrece
ninguna de las dos — su modelo de E/S de consola (`SimpleTextInputProtocol`)
es polling sobre un dispositivo gestionado por firmware, no un IRQ real, y
no existe una API de "temporizador de hardware real" dentro de Boot
Services. El kernel necesita tomar posesión de interrupciones y
temporización, lo cual requiere salir de Boot Services.

## Decisión

Se llama a `uefi::boot::exit_boot_services(None)` exactamente una vez, en
`boot/src/main.rs`, después de que `Incremento 1` ya instaló GDT/IDT (red
de seguridad ante cualquier fallo durante la transición) y antes de
cualquier otro uso de Boot Services. Inmediatamente después:

1. Se enmascaran ambos PIC 8259 legacy (`aion_arch_x86_64::pic::mask_all`)
   — ningún IRQ de hardware puede llegar a un IDT todavía incompleto
   (Incremento 3 remapea el PIC y habilita el temporizador).
2. Se construye el `BootInfo` real: `aion_hal::memory_map::MemoryMap`,
   poblado por `boot/src/memory.rs` a partir del mapa de memoria real
   devuelto por `exit_boot_services`.
3. Se reemplaza `UefiConsole` (que envolvía `system::with_stdin`/
   `with_stdout`, ambos documentados para entrar en pánico tras salir de
   Boot Services) por `VgaConsole`, un backend de hardware que escribe
   directamente al buffer de texto VGA (`0xB8000`). `read_key()` devuelve
   `None` incondicionalmente por ahora — Incremento 4 lo completa con el
   driver de teclado PS/2 por IRQ, sin tocar `kernel::shell`.
4. `UefiPower` no cambia — `uefi::runtime` está documentado disponible
   antes y después de esta transición.
5. `aion-kernel` sigue enlazado como librería dentro del mismo único
   binario `.efi` — no se adopta una imagen de kernel separada en esta
   fase.

## Alternativas consideradas

- **Quedarse indefinidamente en Boot Services**: rechazada — no existe
  una API de temporizador real ni de teclado por IRQ dentro de Boot
  Services; ROADMAP.md exige ambos en esta fase.
- **Cargar una imagen de kernel separada ahora**: rechazada — ningún
  punto de ROADMAP.md para esta fase lo exige, y agregaría un
  cargador ELF y el problema de dirección de carga sin beneficio antes de
  Fase 5 ("hardware físico").
- **Diferir `ExitBootServices` hasta que frames/paginación/heap estén
  listos, y saltar todo de una vez**: rechazada — maximizaría el radio de
  impacto del paso más riesgoso de toda la fase, exactamente lo que este
  proyecto evita secuenciando en incrementos pequeños.

## Consecuencias

- El kernel pasa a poseer PIC, temporizador (Incremento 3) y consola.
- `BootInfo` tiene ahora forma real (`memory_map: MemoryMap`); un cambio
  futuro de forma no dispara por sí solo otro ADR salvo que constituya a
  su vez una sustitución del boot path.
- `docs/fase1-notes.md` sigue siendo correcto: documentaba el estado
  "antes" de esta transición.
- El logging por debugcon (puerto 0xE9) sigue funcionando sin cambios
  después de esta llamada — verificado contra el código fuente del crate
  `uefi`, no asumido — por lo que `cargo xtask boot-test` sigue siendo
  válido como mecanismo de verificación automatizada a través de esta
  transición.
