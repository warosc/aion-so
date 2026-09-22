# tests/

`AGENTS.md` distingue dos verificaciones separadas, y así se mantienen:

- **Pruebas unitarias ejecutables en host**: viven junto al código, en cada
  crate (`#[cfg(test)] mod tests` dentro de `hal/src/lib.rs`,
  `tools/xtask/src/main.rs`, etc.). Se ejecutan con `cargo xtask test`.
- **Smoke test de arranque en QEMU con timeout**: no es una prueba unitaria;
  es una verificación de sistema. Se ejecuta con `cargo xtask boot-test`
  (`--heap-stress` para la carga larga del heap).
- **Soak test**: una compilación de prueba mantiene el heap bajo estrés
  continuo durante un tiempo fijo, y se verifica que no haya pánico,
  excepción, reinicio de CPU ni falta de progreso. Se ejecuta con
  `cargo xtask soak-test` (ver `docs/fase2-notes.md`, Incremento 8).

Esta carpeta raíz `tests/` queda reservada para pruebas de integración
entre varios crates que aún no existen en Fase 0 (por ejemplo, IPC entre
procesos en Fase 3). No se crean pruebas aquí solo para tener contenido.
