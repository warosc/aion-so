# Roadmap de HARLAN OS

Cada fase termina únicamente cuando sus criterios pueden reproducirse desde un entorno limpio.

## Fase 0 — Base del repositorio

- Workspace Rust y toolchain fijado.
- Build reproducible.
- QEMU + OVMF ejecutables desde una tarea de VS Code.
- CI para formato, lint, build y pruebas host.

**Salida:** un comando construye y arranca la imagen.

## Fase 1 — HARLAN OS v0.0.1: primer arranque

- Aplicación UEFI carga el kernel.
- Framebuffer o consola funcional.
- Reporte básico de arquitectura y mapa de memoria.
- Panic handler legible.
- Shell mínima con `help`, `clear`, `version`, `reboot` y `shutdown`.

**Salida:** diez arranques consecutivos exitosos en QEMU.

## Fase 2 — Núcleo básico

- GDT/IDT e interrupciones en x86_64.
- Temporizador y entrada de teclado.
- Administrador de frames y memoria virtual.
- Heap del kernel con pruebas.

**Salida:** prueba prolongada sin corrupción de memoria ni panic inesperado.

## Fase 3 — Procesos y aislamiento

- Modo usuario.
- Syscalls versionadas.
- Scheduler inicial.
- IPC mínimo.
- Carga de un programa de usuario.

**Salida:** dos procesos aislados se comunican sin compartir memoria no autorizada.

## Fase 4 — Almacenamiento y shell

- Driver inicial de almacenamiento virtual.
- VFS y filesystem seleccionado mediante ADR.
- Shell de usuario y utilidades básicas.
- Registro de eventos del sistema.

**Salida:** crear, leer y persistir un archivo entre reinicios.

## Fase 5 — Hardware físico

- Imagen USB UEFI.
- Diagnóstico temprano por consola/serial.
- Inventario de hardware del equipo objetivo.
- Drivers mínimos de entrada, pantalla, almacenamiento y red.

**Salida:** HARLAN OS arranca de forma repetible en el PC objetivo sin escribir en discos no seleccionados.

## Fase 6 — Capacidades y agentes

- Capability Manager y auditoría.
- Sandbox de agentes en userspace.
- Intent Engine con salida estructurada.
- Confirmaciones de riesgo y operaciones reversibles.
- Proveedor local primero; API externa opcional.

**Salida:** un agente completa una tarea de archivos dentro de un directorio permitido y falla de forma segura fuera de él.

## Fase 7 — Portabilidad e IA local

- HAL madura.
- Preparación AArch64.
- Abstracción de GPU/NPU fuera del kernel.
- Memoria semántica local, cifrada y controlable.

**Salida:** el código portable se comparte sin bifurcar el diseño del sistema.
