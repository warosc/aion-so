# Preparación del entorno de desarrollo

Esta guía define el entorno previsto; los comandos exactos se ajustarán al sistema operativo de la estación de trabajo.

## Herramientas

- Visual Studio Code
- Git
- Rust mediante `rustup`
- QEMU con soporte x86_64
- firmware OVMF/UEFI
- LLVM/LLDB cuando el depurador lo requiera
- GitHub CLI opcional
- extensiones VS Code: `rust-analyzer`, `CodeLLDB` y soporte TOML
- Claude Code y Codex configurados por separado

## Organización recomendada

```text
aion-os/
├── .vscode/
├── boot/
├── kernel/
├── arch/x86_64/
├── hal/
├── drivers/
├── userspace/
├── intelligence/
├── tests/
├── tools/
├── AGENTS.md
├── CLAUDE.md
├── ARCHITECTURE.md
└── ROADMAP.md
```

## Configuración de VS Code

Abrir la carpeta raíz, no archivos individuales. Las tareas del proyecto deberán ofrecer como mínimo:

- `AION: Build`
- `AION: Run QEMU`
- `AION: Test`
- `AION: Format + Lint`
- `AION: Debug QEMU`

Codex y Claude deben ejecutarse desde la raíz para que ambos lean sus instrucciones y vean el mismo estado de Git.

## Flujo inicial

```bash
git clone <URL-DEL-REPOSITORIO> aion-os
cd aion-os
git switch develop
code .
```

No copies el repositorio por separado para cada IA. Usa ramas distintas o `git worktree` si deseas sesiones simultáneas; de ese modo comparten historial sin escribir sobre el mismo working tree.

## Primera sesión sugerida

1. Crear el workspace Rust y fijar toolchain.
2. Configurar el target freestanding.
3. Implementar el boot UEFI mínimo.
4. Automatizar la imagen y el arranque QEMU.
5. Añadir un smoke test que detecte `AION OS v0.0.1` por consola serial.

El primer objetivo no es añadir IA: es conseguir un arranque real, repetible y observable.
