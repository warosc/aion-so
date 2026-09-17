# AION OS

AION OS es un proyecto educativo y experimental para construir un sistema operativo **AI-native**, seguro y portable. La primera meta es un kernel real que arranque en QEMU mediante UEFI y presente una consola `AION>`.

## Principios

- El sistema debe arrancar y funcionar sin IA ni conexión a Internet.
- La IA se ejecuta exclusivamente en espacio de usuario.
- El kernel permanece pequeño, determinista y auditable.
- Todo acceso de agentes se controla mediante capacidades explícitas.
- La portabilidad se protege con una HAL: x86_64 primero, ARM64 después.
- QEMU es el entorno inicial; el hardware físico llega cuando el arranque sea estable.

## Documentos

| Archivo | Propósito |
| --- | --- |
| `VISION.md` | Producto, experiencia objetivo y límites |
| `ARCHITECTURE.md` | Arquitectura y decisiones no negociables |
| `ROADMAP.md` | Fases y criterios verificables |
| `AGENTS.md` | Instrucciones para Codex |
| `CLAUDE.md` | Instrucciones para Claude Code |
| `WORKFLOW.md` | Ramas, revisiones y coordinación |
| `SETUP.md` | Preparación de VS Code, Rust y QEMU |

## Primer resultado esperado

```text
AION OS v0.0.1
Boot............ UEFI OK
Architecture.... x86_64
Kernel.......... READY

AION> help
```

Estos documentos son la constitución inicial del repositorio. El código deberá respetarlos; cualquier cambio arquitectónico importante requiere una decisión documentada.
