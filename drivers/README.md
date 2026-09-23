# drivers/

Aquí viven los drivers de dispositivo (almacenamiento, entrada, pantalla,
red) a medida que `ROADMAP.md` los pide: pantalla y teclado en Fase 2,
almacenamiento en Fase 4, hardware físico en Fase 5. El código específico
de arquitectura sigue confinado a `arch/` / `hal/`; esta carpeta contiene
los drivers en sí, construidos sobre esas interfaces.

Hoy:

- `fbcon/` (`harlan-fbcon`): consola de texto sobre un framebuffer lineal de
  32 bits (fuente 8×8 de `font8x8`, dibujada a 2×). Sin dependencias de
  arquitectura ni del firmware: solo recibe un `FramebufferInfo` de `hal`.

El driver del teclado PS/2 vive en `arch/x86_64/src/keyboard.rs`, no aquí,
porque es por naturaleza específico de x86 (i8042, puertos de E/S, IRQ del
PIC); un futuro teclado USB sí pertenecería a esta carpeta.
