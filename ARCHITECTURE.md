# Arquitectura de AION OS

## Regla fundamental

> La IA nunca se ejecuta en espacio de kernel ni obtiene privilegios implícitos.

## Capas

```text
Human Interface
    ↓
Intent Engine (userspace)
    ↓
Agent Runtime (sandbox)
    ↓
Capability Manager
    ↓
System Services / IPC
    ↓
AION Kernel
    ↓
HAL + Drivers
    ↓
Hardware
```

## Kernel

Responsabilidades iniciales:

- arranque y transición UEFI;
- memoria física y virtual;
- interrupciones y temporización;
- planificación de procesos/hilos;
- IPC;
- syscalls mínimas;
- aislamiento y aplicación de capacidades.

El kernel no contiene clientes HTTP, modelos, prompts, memoria semántica ni lógica de agentes.

## HAL y portabilidad

Todo acceso dependiente de arquitectura debe cruzar interfaces explícitas.

```text
arch/
├── x86_64/
└── aarch64/       # futuro

hal/
├── cpu
├── interrupts
├── timer
├── memory_map
└── platform
```

La primera plataforma soportada será `x86_64 + UEFI + QEMU`. El primer hardware físico objetivo será un PC empresarial x86_64 con UEFI.

## Seguridad por capacidades

Una tarea recibe permisos específicos y revocables, por ejemplo:

- `fs.read:/projects/aion`
- `fs.write:/projects/aion/output`
- `network.connect:api.example.com:443`
- `device.camera:use`

No existe una capacidad genérica de “control total” para agentes. Las operaciones destructivas o de alto impacto exigen confirmación y deben dejar auditoría.

## IA y fallos

Las salidas de modelos se tratan como datos no confiables. Se validan contra esquemas; nunca se traducen directamente en syscalls privilegiadas. Si el modelo, la red o el runtime fallan, el kernel, la shell clásica y los servicios esenciales deben seguir funcionando.

## Lenguajes y herramientas

- Rust `no_std` para la mayor parte del kernel y servicios de bajo nivel.
- Assembly mínimo y localizado para entrada, contexto o instrucciones especiales.
- UEFI para el arranque inicial.
- QEMU + OVMF para desarrollo y pruebas.

## Decisiones que requieren ADR

- cambio de modelo de kernel;
- nuevo formato ejecutable o ABI;
- cambios incompatibles en syscalls;
- sustitución del boot path;
- ampliación de privilegios del runtime de IA;
- dependencia obligatoria de servicios externos.
