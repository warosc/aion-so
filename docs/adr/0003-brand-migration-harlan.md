# ADR 0003: Migración de marca — AION OS → HARLAN OS

> Brand migration: AION OS → HARLAN OS
>
> This change updates product identity only. Kernel architecture,
> security boundaries, boot protocol and technical roadmap remain unchanged.

## Contexto

El proyecto cambia de identidad pública. Un cambio de nombre no está en la
lista de "Decisiones que requieren ADR" de `ARCHITECTURE.md` (no toca el
modelo de kernel, la ABI, las syscalls, el boot path, los privilegios del
runtime ni las dependencias externas). Se registra igualmente como ADR
porque el repositorio no tiene changelog y porque la migración renombra
cosas que un lector de los documentos anteriores necesita poder traducir:
los crates, los marcadores que consume `cargo xtask boot-test` y el nombre
del binario UEFI intermedio.

## Decisión

| Antes | Después |
| --- | --- |
| `AION OS` | `HARLAN OS` |
| `AION Kernel` | `Harlan Kernel` |
| prompt `AION> ` | `Harlan> ` |
| banner `AION OS v0.0.1` y salida de `version` | `HARLAN OS 0.0.1` (el banner añade la línea `Computing with intent.`) |
| crates `aion-boot`, `aion-kernel`, `aion-hal`, `aion-arch-x86_64`, `aion-fbcon` | `harlan-boot`, `harlan-kernel`, `harlan-hal`, `harlan-arch-x86_64`, `harlan-fbcon` |
| rutas Rust `aion_hal::…`, `aion_kernel::…`, etc. | `harlan_hal::…`, `harlan_kernel::…`, etc. |
| binario intermedio `aion-boot.efi` | `harlan-boot.efi` |
| marcadores `AION-PHASE0-BOOT-OK`, `AION-PHASE1-SHELL-READY`, `AION-PHASE2-POST-EXIT-OK` | `HARLAN-PHASE0-BOOT-OK`, `HARLAN-PHASE1-SHELL-READY`, `HARLAN-PHASE2-POST-EXIT-OK` |
| prefijos de log `AION:` y `AION PANIC:` | `HARLAN:` y `HARLAN PANIC:` |
| tareas de VS Code `AION: …` | `Harlan: …` |
| directorio de ejemplo `aion-os` | `harlanos` |

Los nombres de componente `Harlan Shell`, `Harlan Runtime`, `Harlan Intent`
(antes "AION Intent Engine") y `Harlan Guard` (antes "AION Security") quedan
reservados; hoy ninguno de los antiguos aparece en el repositorio.

La identidad visible vive en un solo sitio, `kernel/src/identity.rs`
(`PRODUCT_NAME`, `PRODUCT_ID`, `VERSION`, `SHELL_PROMPT`, `TAGLINE`), y la
usan el banner, el prompt y el comando `version`. `VERSION` se sigue leyendo
de `CARGO_PKG_VERSION`: la única fuente de la versión es el `Cargo.toml` del
workspace. Los mensajes de log por debugcon (`HARLAN: …`, los marcadores y
`HARLAN PANIC:`) son literales en `arch/x86_64`, `boot` y `kernel`: son
diagnóstico para desarrolladores, no identidad visible, y `arch` no depende
de `kernel`, así que compartir una constante obligaría a añadir esa
dependencia e invertiría la dirección de las dependencias.

**Sin cambios:** nombres neutrales de directorios y módulos (`kernel`,
`arch`, `hal`, `drivers`, `userspace`, `intelligence`…), nombres de
constantes (`BOOT_OK_MARKER`, `SHELL_READY_MARKER`, `PROMPT`), ABI, boot
path (el ESP sigue arrancando `efi/boot/bootx64.efi`), `BootInfo`, layout de
memoria, dependencias externas y sus versiones, toolchain y targets.

## Documentos históricos

`docs/adr/0001-*`, `docs/adr/0002-*` y `docs/fase{0,1,2}-notes.md` (hasta el
Incremento 4 de Fase 2) **no se reescriben**. Registran lo que se decidió y
se observó en su momento, incluidas líneas de log literales (`AION:
ticks=300`) que eran la salida real entonces. Cambiarlas falsearía ese
registro. Cada uno lleva una nota de cabecera que remite a esta tabla.

## Compatibilidad

- **Sin alias**: los cinco crates son `publish = false` y solo se consumen
  dentro del workspace. No hay consumidores externos que romper.
- Los comandos de los documentos históricos que usan marcadores antiguos
  (por ejemplo `cargo xtask boot-test --marker "AION: ticks=300"`) hay que
  escribirlos ahora con `HARLAN`. El valor por defecto de `--marker` y la CI
  ya están actualizados.
- El repositorio remoto (`warosc/aion-so`) no se renombra aquí: es un
  ajuste de GitHub, fuera del árbol de código.
- `target/` puede conservar un `aion-boot.efi` de builds anteriores. Git lo
  ignora y `cargo clean` lo elimina.

## Consecuencias

- La salida visible cambia en tres puntos: el banner (nombre, versión sin
  `v` y el tagline), el prompt y la respuesta de `version`. El resto de
  comandos se comporta igual.
- Para revertir solo esta migración, basta revertir sus commits: no
  comparten cambios con ningún otro trabajo.
