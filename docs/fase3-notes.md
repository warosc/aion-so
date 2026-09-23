# Notas de Fase 3 — Procesos y aislamiento

Lo más reciente arriba. Salida de la fase (ROADMAP.md): dos procesos
aislados se comunican sin compartir memoria no autorizada.

Decisiones de alcance tomadas al abrir la fase: primero la mudanza a la
mitad alta, binario plano incrustado para el primer programa de usuario, y
`syscall`/`sysret` en vez de `int 0x80`.

## Incremento 16 — El kernel corre desde la mitad alta

`docs/adr/0012-fase3-higher-half-kernel.md`. Hasta ahora el kernel se
ejecutaba donde el firmware dejó su imagen, en la mitad baja, por el mapa de
identidad. Fase 3 necesita esa mitad entera para los procesos.

### Qué hace

- `hal::pe` aprende a leer la **tabla de relocalizaciones** (`.reloc`):
  otra vez código puro y probado en host, que recorre la tabla completa
  antes de dar nada por bueno y rechaza los tipos de relocalización que no
  sabe aplicar en vez de saltárselos.
- `kernel::memory::higher_half` hace la mudanza en dos pasos y en este
  orden: **mapea el alias** de la imagen en PML4 259 —mismos marcos, mismos
  permisos que el ADR 0011: código ejecutable y de solo lectura, el resto no
  ejecutable— y después **aplica las relocalizaciones**. Al revés no
  funciona: en cuanto se relocaliza, todos los punteros absolutos de los
  datos nombran el alias, y si no estuviera mapeado el siguiente uso
  fallaría.
- `kmain` salta al gemelo de su propia función en el alias y sigue ahí.
- `arch::interrupts::reinstall_descriptors` vuelve a cargar GDT, TSS e IDT
  leyendo las direcciones que tienen ahora. Guardan direcciones absolutas
  escritas en tiempo de ejecución, que ninguna relocalización toca.

### Verificación ejecutada

- Host: 196 pruebas en verde (192 + 4 nuevas de la mudanza y las
  relocalizaciones).
- QEMU, lo que demuestra que funcionó: `kernel moved into kernel space:
  code at 0xffff8180000097e0 (was 0xddc67e0), 85 page(s) mapped, 61
  read-only, 731 address(es) relocated`. Con 512 MiB de RAM la imagen se
  carga en `0x1ddc67e0` y **el alias es el mismo**: el kernel ya no depende
  de dónde lo pongan.
- Las interrupciones siguen entrando: la autoprueba `int3` aparece dos veces
  en el registro (una por `init`, otra por `reinstall_descriptors`) y el
  temporizador sigue contando después del salto.
- Prueba negativa del camino delicado: desbordar la pila del kernel después
  de la mudanza sigue dando un double fault legible en la pila IST
  (`#DF DOUBLE FAULT on the stack at 0xffff810000015f3f`), lo que prueba que
  el TSS recargado apunta a donde debe.
- `boot-test --repeat 10` 10/10, soak de 120 s PASS, `shutdown` apaga,
  `reboot` rearranca, `fmt-lint` limpio.
- Mutación: 7. Cinco detectadas a la primera; dos sobrevivieron y
  destaparon pruebas flojas —una usaba un tamaño de bloque impar, que
  rechazaba otra condición, y otra no distinguía la guarda de tamaños
  impares— . Reforzadas, las 7 detectadas.

### Riesgos y límites

- **Lo escrito en tiempo de ejecución sigue apuntando abajo**: el puntero
  que `log::set_logger` guardó, y las vtables de los objetos `dyn` del
  contexto, se escribieron antes de la mudanza y nombran la imagen en la
  mitad baja. Funciona porque la mitad baja sigue mapeada; el Incremento 17,
  que la libera, tiene que rehacerlos.
- La mitad baja sigue mapeada entera, con el código del firmware ejecutable
  y escribible.
- La imagen ocupa ahora dos direcciones. Los marcos son los mismos y nunca
  fueron asignables, así que no hay memoria duplicada.
