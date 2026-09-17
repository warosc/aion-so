# Flujo de trabajo: Oscar, Codex y Claude Code

## Fuente única de verdad

GitHub contiene el historial oficial. `main` siempre debe ser arrancable; `develop` integra trabajo validado antes de promoverlo.

## Ramas

```text
main
└── develop
    ├── codex/<issue>-<tema>
    └── claude/<issue>-<tema>
```

Ejemplos: `codex/12-frame-allocator` y `claude/15-uefi-loader`.

## Asignación

Cada issue debe indicar:

- objetivo y no-objetivos;
- archivos o módulos reservados;
- interfaces que puede consumir pero no cambiar;
- criterios de aceptación;
- agente implementador y agente revisor.

Codex y Claude no deben editar los mismos archivos al mismo tiempo. Si existe solapamiento inevitable, primero se acuerda el contrato y después se secuencian los cambios.

## Ciclo

1. Oscar selecciona el milestone y aprueba el alcance.
2. El implementador crea una rama pequeña.
3. Ejecuta pruebas y documenta evidencia.
4. El otro agente revisa el diff y reproduce las verificaciones críticas.
5. El implementador corrige hallazgos.
6. Oscar autoriza el merge a `develop`.
7. Un smoke test completo precede la promoción a `main`.

## Commits

Usar commits pequeños con mensajes como:

```text
boot: load kernel image through UEFI
mm: add bitmap physical frame allocator
docs: record syscall ABI decision
test: add QEMU boot smoke check
```

## Definition of Done

- criterio de aceptación demostrado;
- cambios revisados por el otro agente;
- formato, lint, build y pruebas relevantes en verde;
- boot test cuando corresponda;
- documentación actualizada;
- sin secretos, binarios generados ni artefactos pesados versionados;
- riesgos pendientes registrados.

## Conflictos

No resolver conflictos escogiendo automáticamente “ours” o “theirs”. Comparar intención, conservar cambios compatibles y repetir toda validación afectada.
