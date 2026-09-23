# ADR 0012: El kernel se reubica y corre desde la mitad alta

## Contexto

El kernel se ejecuta donde el firmware dejó su imagen: una dirección física
de la mitad baja (`0x0DDC_7000` en QEMU), alcanzada por el mapa de
identidad. Eso funcionó hasta ahora porque la mitad baja no la usaba nadie
más.

Fase 3 la necesita entera: **la mitad baja pasa a ser espacio de usuario**,
un espacio por proceso, y el kernel tiene que seguir mapeado y ejecutándose
mientras esos espacios cambian bajo sus pies. Un kernel que vive en la mitad
baja no puede compartirse entre espacios de direcciones sin regalarle al
proceso la mitad de su mapa.

La imagen es un PE32+ que el firmware ya reubicó a su dirección de carga,
con tabla de relocalizaciones (`.reloc`, 1484 bytes) y `DYNAMIC_BASE`
activo. Es decir: se puede volver a reubicar.

Cambia dónde vive el kernel en el mapa de memoria, así que lleva ADR.

## Decisión

1. **La imagen se mapea también en la mitad alta**, en un hueco propio
   (`KERNEL_IMAGE_START`, PML4 259), con los mismos permisos que el ADR
   0011 fijó: el código ejecutable y de solo lectura, el resto no
   ejecutable. Son los mismos marcos físicos vistos por dos direcciones.
2. **Se aplican las relocalizaciones PE** para la diferencia entre la
   dirección de carga y la nueva. El orden importa: **primero se mapea el
   alias y después se relocaliza**, porque en cuanto se escriben las
   relocalizaciones todos los punteros absolutos de los datos apuntan al
   alias, y si no estuviera mapeado el siguiente uso fallaría.
3. **El kernel salta al alias** y sigue ahí: a partir de ese punto RIP está
   en la mitad alta. La pila y el heap ya estaban en la mitad alta desde los
   ADR 0006 y el Incremento 10, así que no hay nada más que mover.
4. **GDT, IDT y TSS se recargan en sus direcciones nuevas.** Guardan
   direcciones absolutas —el GDTR y el IDTR apuntan a las tablas, el
   descriptor del TSS lleva su base— y quedarse con las viejas significaría
   depender de la mitad baja en la siguiente interrupción.
5. **La mitad baja sigue mapeada en este incremento.** Liberarla es el
   siguiente paso (ventana física y framebuffer por la mitad alta); hacerlo
   aquí mezclaría dos cambios grandes en un solo PR.

## Alternativas consideradas

- **Enlazar el kernel como binario aparte a una base alta** y que `boot` lo
  cargue: es lo que hacen los kernels maduros y evita reubicarse a uno
  mismo, pero parte el artefacto en dos, cambia el boot path (que el
  ARCHITECTURE.md marca como decisión de ADR propia) y obliga a un cargador
  de verdad antes de tener procesos. Cuando haya filesystem (Fase 4) será el
  momento natural.
- **Quedarse en la mitad baja y dar a los procesos solo un trozo**: reparte
  el espacio de usuario alrededor del kernel, obliga a validar rangos
  partidos y regala información sobre dónde está el kernel. No.
- **Copiar la imagen a marcos nuevos** en vez de mapear los mismos: duplica
  ~320 KiB sin ganar nada; los marcos de la imagen no se reciclan de todas
  formas.
- **No relocalizar y confiar en que todo sea RIP-relativo**: falso. Los
  punteros absolutos que el enlazador deja en datos (tablas de funciones,
  vtables) son justo lo que `.reloc` enumera.

## Consecuencias

- El kernel deja de depender de dónde lo cargó el firmware: su código, sus
  datos, su pila y su heap están en la mitad alta.
- Los marcos de la imagen siguen retenidos (nunca fueron asignables) y ahora
  tienen dos direcciones; el alias es el que usa el kernel.
- Un fallo al aplicar las relocalizaciones o al recargar las tablas no se ve
  como un error: se ve como una máquina que triplefaultea. Verificado en
  QEMU: 85 páginas mapeadas, 61 de solo lectura, 731 direcciones
  relocalizadas, y el mismo alias (`0xFFFF_8180_0000_97E0`) con 256 y con
  512 MiB de RAM, aunque el firmware cargue la imagen en sitios distintos.
  Las excepciones siguen entrando (la autoprueba `int3` aparece dos veces) y
  un desbordamiento de pila sigue saliendo como double fault legible en la
  pila IST, que es lo que prueba que el TSS recargado apunta a donde debe.
- **Lo que se escribió en tiempo de ejecución no se relocaliza**: el puntero
  que `log::set_logger` guardó y las vtables de los objetos `dyn` siguen
  nombrando la imagen en la mitad baja. Mientras esa mitad siga mapeada
  funcionan; el incremento que la libere tendrá que rehacerlos. Queda
  anotado aquí porque es el cabo suelto de esta decisión.
- La mitad baja queda lista para liberarse en el incremento siguiente.
