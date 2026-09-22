# Instrucciones para Codex

## Misión

Implementar HARLAN OS de forma incremental, verificable y segura conforme a `ARCHITECTURE.md` y `ROADMAP.md`.

## Antes de editar

1. Lee `README.md`, `VISION.md`, `ARCHITECTURE.md`, `ROADMAP.md` y `WORKFLOW.md`.
2. Inspecciona el estado del repositorio y cambios existentes.
3. Define el criterio de aceptación de la tarea.
4. Limita el cambio al subsistema asignado.

## Reglas obligatorias

- No ejecutar IA en kernel space.
- No introducir dependencia de red para arrancar.
- Mantener código dependiente de arquitectura dentro de `arch/` o `hal/`.
- Preferir Rust seguro; cada bloque `unsafe` debe ser mínimo, justificado y documentar invariantes.
- No cambiar interfaces públicas, ABI o arquitectura sin un ADR aprobado.
- No ocultar errores con stubs que reporten éxito.
- No modificar archivos asignados activamente a Claude sin coordinación explícita.
- No trabajar directamente en `main`.

## Calidad requerida

- `cargo fmt --check`
- `cargo clippy` en los targets disponibles, sin nuevas advertencias injustificadas
- pruebas unitarias ejecutables en host cuando la lógica no dependa de hardware
- build del target freestanding
- smoke test de arranque en QEMU con timeout
- documentación de comandos y limitaciones

## Entrega

Al terminar, reporta:

- resultado funcional;
- archivos modificados;
- comandos ejecutados y resultados;
- riesgos o limitaciones;
- pasos exactos para reproducir;
- revisión recomendada para Claude.

Nunca afirmes que HARLAN OS arranca si no fue observado en QEMU o en el hardware indicado.
