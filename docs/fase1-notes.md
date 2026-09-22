# Notas de implementación — Fase 1

> **Nota (ADR 0003):** documento anterior a la migración de marca AION OS →
> HARLAN OS. Se conserva sin reescribir porque registra lo que se decidió y
> observó entonces. `AION` / `aion-*` / `aion_*` equivalen hoy a `HARLAN` /
> `harlan-*` / `harlan_*`; tabla completa en
> `docs/adr/0003-brand-migration-harlan.md`.

## No se necesitó un ADR nuevo

Fase 1 se queda enteramente dentro de UEFI Boot Services: no se llama a
`ExitBootServices`, no se carga una imagen de kernel separada, sigue
siendo el mismo único binario `.efi` de Fase 0 con `aion-kernel` enlazado
como librería. `BootInfo` no cambia de forma. Por lo tanto no se dispara
ninguna de las condiciones de `ARCHITECTURE.md` que exigen ADR
("sustitución del boot path", nuevo formato ejecutable, cambios de ABI).
`docs/adr/0001-fase0-boot-path.md` sigue siendo la única autoridad sobre
el boot path; el disparador de "loader real" que menciona sigue
pendiente, presumiblemente para cuando Fase 2/3 necesiten memoria e
interrupciones propias.

## El prompt visible y el marcador de debugcon son canales distintos

`Console::write_str` (el banner, el prompt `AION> `, el eco de teclas, la
salida de comandos) escribe a través de `uefi::system::with_stdout`, que
UEFI expone en el dispositivo de video emulado por QEMU (VGA/GOP). El
marcador `kernel::shell::SHELL_READY_MARKER` que usa
`cargo xtask boot-test` se loguea vía `log::info!`, que sale por el
dispositivo `isa-debugcon` (puerto 0xE9) — un dispositivo QEMU
completamente separado. Por eso el marcador de verificación nunca es el
texto literal del prompt: son dos pipelines distintos y uno no implica el
otro.

## Verificación real ejecutada durante la implementación

Antes de dar por buena la abstracción `Console`/`UefiConsole`, se verificó
end-to-end con tecleo real simulado (vía el monitor de QEMU, comando
`sendkey`, sobre un socket TCP) contra un build con logging temporal
espejo en `write_str` (revertido antes de commitear): se tecleó `help`,
`version`, `clear` y `shutdown` carácter por carácter sobre el teclado
emulado real de QEMU, confirmando en el log de debugcon que cada eco,
respuesta de comando y el nuevo prompt aparecían exactamente como se
esperaba, y que `shutdown` efectivamente terminó el proceso de QEMU (el
mismo camino de código que usa `reboot`, solo con otro `ResetType`). Esto
respeta la regla de `AGENTS.md`/`CLAUDE.md` de nunca afirmar un resultado
que no fue observado.

## Inyección de teclas simulada: diferida como mecanismo permanente

Se decidió no construir en `tools/xtask` un mecanismo permanente de
inyección de teclas por QEMU para probar la shell en CI. La lógica de
despacho de comandos (`help`/`clear`/`version`/`reboot`/`shutdown`) ya
está cubierta con tests de host deterministas contra una `Console` falsa
(`kernel/src/shell.rs`), que no dependen de QEMU ni de temporización. El
criterio de "diez arranques consecutivos" de `ROADMAP.md` se verifica con
`cargo xtask boot-test --repeat 10` contra el marcador de debugcon. La
verificación interactiva de teclado real (arriba) se hizo una vez, a mano,
durante esta implementación; queda pendiente para una fase donde el
input real por interrupciones (Fase 2+) justifique invertir en un arnés
de pruebas más elaborado.

## Simplificaciones de esta fase

- Buffer de línea de tamaño fijo en stack (128 bytes) — no hay heap
  todavía (Fase 2).
- Solo ASCII imprimible (más espacio): caracteres no-ASCII y caracteres de
  control ASCII (como Tab, 0x09) se ignoran silenciosamente en vez de
  insertarse en el buffer o ecoarse como glifo.
- `ConsoleKey::Unknown` (flechas, teclas de función) se ignora en la
  shell — no hay historial de comandos ni edición más allá de backspace.
- El reporte de mapa de memoria es puramente informativo (cuenta de
  descriptores y páginas totales); no se usa para nada todavía — el
  administrador de frames real es Fase 2.
