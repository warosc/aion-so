# ADR 0014: Ring 3 y el ABI de syscalls v0

## Contexto

La mitad baja ya no es del kernel (ADR 0013). Falta lo que la vacía tenía
sentido: que ahí corra código sin privilegios y pueda pedirle cosas al
kernel sin poder tocarlo.

Dos decisiones que el ROADMAP marca como de ADR —"nuevo formato ejecutable
o ABI" y "cambios incompatibles en syscalls"— y una que cambia el layout de
la GDT.

## Decisión

### Cómo se entra y se sale del kernel

1. **`syscall`/`sysret`**, el mecanismo nativo de x86_64, no `int 0x80`.
   Se habilita con `EFER.SCE` y se programa con tres MSR: `STAR` (los
   selectores), `LSTAR` (dónde entra) y `FMASK` (qué bits de RFLAGS se
   limpian al entrar: `IF`, `DF` y `AC`).
2. **`IF` se limpia al entrar**, así que el manejador corre con las
   interrupciones desactivadas hasta que el kernel decida otra cosa. Es lo
   contrario de lo que hace una puerta de interrupción con IST, y es
   deliberado: `syscall` **no cambia de pila**, y hasta que no se haya
   cambiado a una del kernel no hay dónde atender nada.
3. **La pila se cambia a mano**, con `swapgs` y `KERNEL_GS_BASE` apuntando
   a una estructura por CPU que guarda la pila del kernel y dónde dejar la
   del usuario. Con un solo núcleo bastaría una variable global; se hace
   así desde el principio porque la versión global hay que tirarla en
   cuanto haya dos.

### La GDT

4. Se añaden **descriptores de usuario** y el orden pasa a ser el que
   `sysret` exige: `null`, código de kernel (`0x08`), datos de kernel
   (`0x10`), datos de usuario (`0x18`), código de usuario (`0x20`), TSS
   (`0x28`). `STAR[63:48] = 0x10`, porque `sysretq` carga `SS` de
   `base + 8` y `CS` de `base + 16`, ambos con RPL 3.

### El ABI, versión 0

5. **Registros**, siguiendo lo que la instrucción ya impone: `RAX` el
   número de syscall, `RDI`, `RSI`, `RDX`, `R10`, `R8`, `R9` los
   argumentos (`R10` en vez de `RCX` porque `syscall` pisa `RCX` con la
   dirección de vuelta), `RAX` el resultado. `RCX` y `R11` los destruye la
   instrucción; el resto de registros llamada-salvados se conservan.
6. **Resultado**: `RAX` negativo es un error (`-1` desconocida, `-2`
   argumento inválido, `-3` sin permiso). Un número, no un bitfield: los
   errores son pocos y explícitos.
7. **Las syscalls de v0 son dos**: `0 = log(ptr, len)`, que escribe un
   texto del usuario en el registro del kernel, y `1 = exit(code)`, que
   detiene el proceso. Nada más: lo justo para demostrar que se entra, se
   vuelve y se aísla.
8. **Versionado**: v0 no es estable. Todo cambio incompatible mientras
   exista un solo programa incrustado se hace subiendo este ADR, no
   inventando números nuevos. El primer consumidor externo congelará el
   contrato.
9. **Todo puntero que llega del usuario se valida en el kernel** antes de
   leerlo: dentro de la mitad baja, sin desbordar, y mapeado para el
   proceso. Es el punto 5 de `docs/memory-safety.md` —validación de
   rangos— que por fin tiene un sitio natural donde vivir.

### El programa

10. **Binario plano incrustado** en la imagen del kernel, copiado a una
    página de usuario al arrancar. Sin analizador, sin reubicaciones, sin
    formato que mantener: lo que hace falta para probar el aislamiento. El
    formato de verdad se decide cuando haya filesystem (Fase 4).

## Alternativas consideradas

- **`int 0x80`**: reutiliza la IDT que ya funciona y cambia de pila sola,
  pero es más lenta, no es lo que usará el sistema y habría que migrar
  después con cambio de ABI de por medio.
- **Una variable global en vez de `swapgs`**: funciona con un núcleo y hay
  que tirarla con dos. El coste de hacerlo bien ahora es un MSR.
- **Devolver errores en un registro aparte**: más limpio en teoría, pero
  obliga a leer dos registros para saber si algo salió bien.
- **ELF desde el principio**: ver ADR del Incremento 19; aquí solo haría
  falta para cargar un programa que todavía no existe.

## Consecuencias

- La GDT cambia de layout y el selector del TSS pasa de `0x18` a `0x28`.
  Es interno: nadie fuera de `arch` lo nombra.
- El kernel gana una superficie de ataque real: todo lo que llega en
  registros desde ring 3 es dato no confiable. La validación de punteros es
  parte del ABI, no un detalle de implementación.
- `PageFlags` gana `user`, y el mapper deja de rechazar la mitad baja
  cuando esa bandera está puesta: era la comprobación que impedía mapear
  nada para un proceso.
- Con las interrupciones desactivadas dentro del manejador, una syscall
  larga retrasa el reloj. En v0 ninguna lo es; el scheduler (Incremento 20)
  tendrá que decidir dónde se vuelven a activar.
