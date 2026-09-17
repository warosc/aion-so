# drivers/

Vacío intencionalmente en Fase 0. Aquí vivirán los drivers de dispositivo
(almacenamiento, entrada, pantalla, red) a partir de Fase 2 (temporizador,
teclado), Fase 4 (almacenamiento) y Fase 5 (hardware físico), según
`ROADMAP.md`. El código específico de arquitectura sigue confinado a
`arch/` / `hal/`; esta carpeta contiene los drivers en sí, construidos sobre
esas interfaces.
