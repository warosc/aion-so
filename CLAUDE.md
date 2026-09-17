# Instrucciones para Claude Code

## Misión

Construir o revisar subsistemas de AION OS sin romper los contratos definidos en `ARCHITECTURE.md`.

## Inicio de cada tarea

1. Lee la documentación raíz completa.
2. Revisa el diff y las ramas activas antes de editar.
3. Confirma el alcance y los archivos bajo tu responsabilidad.
4. Identifica invariantes, riesgos de memoria y criterios de aceptación.

## Reglas técnicas

- La IA pertenece únicamente a userspace.
- El kernel debe poder arrancar sin red, modelo o API.
- La portabilidad cruza la HAL; evita dependencias de máquina fuera de ella.
- Todo `unsafe` debe declarar precondiciones, invariantes y razón por la que no existe una alternativa segura práctica.
- No cambies ABI, syscalls, boot protocol o layout de memoria sin ADR.
- No reemplaces una implementación correcta por una abstracción prematura.
- No edites simultáneamente archivos reservados por Codex.
- No hagas merge de tu propio cambio sin revisión cruzada.

## Rol de revisión

Cuando revises trabajo de Codex, prioriza:

1. corrección de memoria y concurrencia;
2. límites de privilegio;
3. supuestos de UEFI y arquitectura;
4. manejo de errores y estados parciales;
5. reproducibilidad de build y boot;
6. cobertura de pruebas y documentación.

Clasifica hallazgos como `blocker`, `major`, `minor` o `suggestion`, indicando archivo, evidencia e impacto.

## Entrega

Incluye resumen, diff conceptual, verificación ejecutada, riesgos pendientes y qué debe validar Codex. No declares éxito basándote únicamente en compilación si el criterio exige arranque.
