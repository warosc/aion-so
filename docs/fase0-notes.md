# Notas de implementación — Fase 0

> **Nota (ADR 0003):** documento anterior a la migración de marca AION OS →
> HARLAN OS. Se conserva sin reescribir porque registra lo que se decidió y
> observó entonces. `AION` / `aion-*` / `aion_*` equivalen hoy a `HARLAN` /
> `harlan-*` / `harlan_*`; tabla completa en
> `docs/adr/0003-brand-migration-harlan.md`.

Notas honestas sobre limitaciones conocidas de esta fase, para no
sorprender a quien retome el trabajo (Codex, Claude Code u Oscar).

## Depuración simbólica de `aion-boot.efi`

`AION: Debug QEMU` deja la CPU pausada en el reset y expone un gdbstub en
`tcp::1234` (`-s -S`). El `.efi` es un binario PE32+ cargado por el
firmware UEFI en una dirección base que **no se conoce de antemano**: la
elige OVMF en tiempo de arranque. Esto significa que, al conectar LLDB,
los símbolos no estarán alineados hasta ejecutar manualmente algo como
`target modules load --slide <base>` una vez que se conoce la dirección
real (visible en el log de consola UEFI). Para Fase 0, el log por
`debugcon` (capturado por `cargo xtask boot-test`) sigue siendo la
herramienta principal de verificación; invertir en stepping simbólico
pulido tiene más sentido en Fase 1+, cuando haya más lógica de kernel que
recorrer.

## rust-analyzer y múltiples targets

El workspace mezcla tres targets: `x86_64-unknown-uefi` (`boot`),
`x86_64-unknown-none` (`kernel`, `hal`, `arch/x86_64`) y el target del host
(`tools/xtask`). rust-analyzer no puede tipar los tres correctamente en una
sola pasada. `.vscode/settings.json` fija
`rust-analyzer.cargo.target = "x86_64-unknown-none"`, que cubre la mayoría
del código futuro (kernel/hal/arch). `boot/` y `tools/xtask/` pueden
mostrar falsos positivos en el editor; no se resuelve con precisión en
Fase 0.

## OVMF sin conexión a red

`cargo xtask` obtiene el firmware OVMF mediante el crate `ovmf-prebuilt`,
que lo descarga y cachea bajo `target/ovmf/` la primera vez que se
necesita. Esto es una dependencia de las **herramientas de build**, no del
arranque del propio sistema operativo (equivalente a necesitar firmware en
la flash de un PC real) — no contradice la regla de "sin red para
arrancar" de `ARCHITECTURE.md`, que se aplica al sistema en ejecución, no
al pipeline de build.

Para desarrollar sin red disponible, coloca manualmente `code.fd` y
`vars.fd` bajo `target/ovmf/x64/` (mismo layout que genera
`ovmf-prebuilt`) — por ejemplo copiándolos desde otra máquina con acceso a
red, o desde el paquete `ovmf` de una distribución Linux/WSL.

## Puerto de `debugcon`

El backend `log-debugcon` del crate `uefi` escribe directamente al puerto
de E/S `0xE9` (el "hack" de debug de estilo Bochs), no al puerto `0x402`
que usa por defecto QEMU para su dispositivo `isa-debugcon` (el "info
port" de Bochs BIOS). `cargo xtask` fuerza `isa-debugcon.iobase=0xe9` para
que coincidan; si se cambia el logger o su configuración, hay que revisar
que ambos puertos sigan alineados.
