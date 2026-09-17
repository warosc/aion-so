# tests/

`AGENTS.md` distingue dos verificaciones separadas, y así se mantienen:

- **Pruebas unitarias ejecutables en host**: viven junto al código, en cada
  crate (`#[cfg(test)] mod tests` dentro de `hal/src/lib.rs`,
  `tools/xtask/src/main.rs`, etc.). Se ejecutan con `cargo xtask test`.
- **Smoke test de arranque en QEMU con timeout**: no es una prueba unitaria;
  es una verificación de sistema. Se ejecuta con `cargo xtask boot-test`.

Esta carpeta raíz `tests/` queda reservada para pruebas de integración
entre varios crates que aún no existen en Fase 0 (por ejemplo, IPC entre
procesos en Fase 3). No se crean pruebas aquí solo para tener contenido.
