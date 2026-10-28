# S13: aislamiento, cuotas y diez usuarios

Fecha: 2026-10-28. Issue: [#14](https://github.com/joalceco/mddiff/issues/14). Rama: `feature/multiuser-sources`.

Esta verificación cubre el servicio implementado: trabajo manual o capturado desde Memos, propuestas API/MCP y eventos privados. Utiliza identidades y documentos sintéticos. El resultado no autoriza por sí solo la publicación: Jira como original, el selector general y la coordinación completa continúan en sus historias; el despliegue y la recuperación operativa pertenecen a [S14](https://github.com/joalceco/mddiff/issues/15). Google/GitHub sigue pendiente.

## Límites del modo alojado

Las cuotas siguientes se aplican en el servidor. El lector local conserva su comportamiento y el máximo de 2 MiB por archivo.

| Recurso | Límite alojado |
| --- | --- |
| Documentos | 2 MiB UTF-8 por original o propuesta; etiquetas de hasta 255 bytes sin controles. |
| Complejidad por documento | 10 000 líneas, 32 768 eventos Markdown y 128 niveles de anidamiento. CR, LF y CRLF se cuentan como terminadores; CRLF cuenta una vez. |
| HTML aportado | Hasta 256 caracteres `<` en los eventos HTML del documento. Es una cota conservadora del formato, no un contador de nodos DOM. El HTML continúa pasando por el saneador. |
| Vista calculada | Hasta 24 MiB de JSON serializado, comprobados antes de escribir o publicar el cambio. No se almacena una segunda copia JSON para contar bytes. |
| Solicitudes de contenido | Hasta 16 admitidas globalmente y cuatro por identidad, incluidas las que esperan. Dos pueden leer/procesar cuerpos o enviar respuestas al mismo tiempo. |
| Lectura de fuentes | Hasta dos solicitudes de captura simultáneas, con presupuesto separado del contenido; una fuente lenta no ocupa los permisos de lectura del trabajo. Comparte las cotas de 16 globales y cuatro por identidad. |
| Espacios residentes | Hasta diez identidades con vista en memoria. Un espacio sin solicitudes, SSE o referencias activas puede salir de caché; su trabajo permanece en SQLite y se restaura al volver. Si los diez están ocupados, otra identidad recibe `429`. |
| Procesamiento | Dos tareas de cálculo simultáneas; hasta diez actualizaciones admitidas y dos por identidad. La restauración utiliza el mismo presupuesto de cálculo. |
| SSE | Hasta 40 conexiones globales y tres por identidad; se liberan cuando el servidor detecta el cierre del stream. Las sesiones y pestañas de una misma persona comparten su cuota. |

Las solicitudes de contenido incluyen `/api/work`, `/api/state` y `/mcp`; las capturas utilizan `/api/sources/`. Estas cuotas no son un límite global de todo el tráfico HTTP ni una defensa contra DDoS. Los límites existentes de login siguen siendo 256 intercambios pendientes y 1024 sesiones en memoria; no equivalen a una capacidad demostrada para ese número de personas.

Las cuotas HTTP se conservan durante el cuerpo de la respuesta, también con un cliente lento. Un exceso devuelve `429` con `Retry-After: 1`; un exceso de documento, complejidad o vista devuelve `413`. Un conflicto de revisión devuelve `409`. Las herramientas MCP también pueden devolver un error estructurado con el estado correspondiente. Un reintento debe conservar la revisión esperada; no debe adoptar automáticamente una revisión nueva para forzar una escritura.

Cada propietario conserva su bloqueo de actualización. El semáforo de cálculo permite dos tareas; no sustituye esos bloqueos por un mutex global. Las actualizaciones que continúan después de una desconexión retienen sus permisos hasta terminar. Las pruebas verifican que cancelar al solicitante no permite ejecutar tareas fuera de cuota.

Los límites de complejidad y vista son nuevos respecto a S12: un documento menor de 2 MiB puede rechazarse por su estructura o expansión JSON. Antes de actualizar una instalación anterior, comprueba que su trabajo guardado cumple estas cotas y conserva una copia coherente. Un trabajo anterior que las exceda puede devolver `413` al restaurar su vista; su fila SQLite no se borra. Utiliza el binario anterior para exportarlo o reemplazarlo antes de actualizar. La entrega no migra ni elimina documentos automáticamente.

## Criterios de la medición

Se fijan estos umbrales para la prueba sintética de diez sesiones:

| Comprobación | Umbral |
| --- | --- |
| Documento de 4 KiB | p95 de PUT, propuesta MCP y lectura API de hasta 3000 ms por operación. |
| Documento de 2 MiB | p95 de esas operaciones de hasta 15 000 ms. |
| Memoria de mddiff | Pico de hasta 460 MiB dentro de un límite de 512 MiB, sin swap. |
| Correctitud | Cero fallos inesperados, mezcla de propietarios, pérdida de trabajo o terminaciones OOM. Los rechazos esperados se verifican por separado. |
| Continuidad | Recuperación exacta de diez trabajos, incluida una marca de borrado, y diez conexiones cifradas tras reiniciar y recrear el contenedor. |

Son umbrales de aceptación de esta prueba, no un SLA ni una promesa sobre documentos o máquinas distintos.

## Método y entorno

El runner [`scripts/test_container.py --capacity`](../../scripts/test_container.py) reutiliza la imagen y los volúmenes aislados de S12. [`scripts/capacity_scenario.py`](../../scripts/capacity_scenario.py) ejecuta diez propietarios distintos, cada uno con sesión web, credencial Bearer y SSE. La fixture Keycloak añade `load03` a `load10` a Alice/Bob; Mallory permanece sin admisión. Las credenciales se generan en privado y nunca forman parte del informe.

Las sesiones se establecen antes de medir. Una barrera inicia diez recorridos concurrentes; cada recorrido reemplaza el original por API, recibe una propuesta por MCP y consulta el trabajo y la vista renderizada desde su sesión web. Verifica bytes y revisión después de cada ronda. El perfil pequeño realiza tres rondas por persona; el de 2 MiB, una. Son 80 escrituras y 80 lecturas medidas, además de las comprobaciones de fuentes, límites, aislamiento y recuperación.

El documento grande tiene párrafos de unos 4 KiB, una marca sintética por propietario y un cambio localizado. Otros casos prueban líneas, formato, profundidad, HTML y expansión excesivos. Diez solicitudes simultáneas con 2 MiB de controles JSON deben devolver `413` y conservar los diez trabajos. Una identidad adicional admitida temporalmente comprueba el aforo de memoria, logout que libera un stream y restauración tras salir de caché. Estos casos no constituyen un benchmark de todo Markdown posible.

| Componente | Configuración |
| --- | --- |
| mddiff | Rust release, Linux arm64, UID/GID 10001, una CPU, 512 MiB, swap desactivado y 128 procesos. |
| Host Docker medido | Docker Engine 29.5.2, VM aarch64 con dos CPU y 2 054 840 320 bytes de memoria; comparte recursos con otras cargas. |
| Cliente y proxy | Python 3.13 en imagen oficial fijada por digest; HTTPS loopback con CA temporal verificada y backlog 64. Fuera del presupuesto de mddiff. |
| IdP | Keycloak 26.8.1 sintético, HTTP loopback, una CPU, 768 MiB y heap JVM de 128–384 MiB. Fuera del presupuesto de mddiff. |
| Fuente | Memos 0.31 simulado, credenciales personales diferentes y 50 ms de demora por GET. Solo acepta lecturas de negocio. |

La cuota de CPU, RAM, swap y procesos se comprueba también en cgroups, no solo en los argumentos de Docker. `memory.peak` recoge el máximo del cgroup, incluidas memoria anónima, caché y estructuras contabilizadas por el kernel; no representa únicamente RSS. Se toma el máximo de la carga, rechazos y recuperación. Consulta [cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html) y [los límites de recursos de Docker](https://docs.docker.com/engine/containers/resource_constraints/).

## Resultados

La corrida local completa terminó con código `0`. Las diez sesiones completaron **80 escrituras y 80 lecturas medidas**, sin fallos inesperados. Se rechazaron diez expansiones excesivas simultáneas sin cambiar el trabajo. Se comprobó el aforo de diez espacios residentes, liberación mediante logout y restauración del propietario anterior desde SQLite. El reinicio y la recreación recuperaron los diez trabajos y las diez conexiones; no se detectó OOM en mddiff ni en la fixture de identidad.

| Operación | 4 KiB p50 / p95 (ms) | 2 MiB p50 / p95 (ms) |
| --- | --- | --- |
| PUT del original | 19.92 / 33.85 | 241.39 / 400.10 |
| Propuesta MCP | 19.36 / 33.96 | 282.06 / 340.75 |
| GET del trabajo | 18.12 / 44.15 | 209.45 / 269.54 |
| GET de la vista | 16.65 / 34.38 | 198.41 / 239.17 |
| Núcleo de cálculo Rust | 0.06 / 0.12 | 12.78 / 23.72 |

El pico completo fue **296.30 MiB**, por debajo del umbral de 460 MiB y del límite de 512 MiB. La captura inicial Memos tuvo p50 **121.80 ms** y p95 **134.85 ms**, con diez muestras y dos GET sintéticos de 50 ms por captura.

Los percentiles usan rango más próximo: 30 muestras por operación pequeña y diez por operación grande. El núcleo Rust aporta 60 y 20 muestras, respectivamente, porque mide tanto la sustitución como la propuesta. Con diez muestras, p95 coincide con el máximo; no se atribuye precisión estadística de una prueba prolongada.

Las latencias HTTP/MCP incluyen espera, introspección, TLS loopback, serialización y cliente de prueba. `processing_ms` mide preparación Markdown/diff/render antes de contabilizar el tamaño JSON, persistir y enviar; excluye cola, SQL y red. La captura de fuente se informa por separado; no representa la latencia de Memos o Jira reales. No se midieron percentiles de pintado del navegador.

Pasaron 60 pruebas Rust, formato, Clippy y release. Las regresiones y el estado de las corridas de contenedor, navegador, modos, autenticación, persistencia y proveedores se registran en el issue y en CI. La corrida de navegador utilizó Chrome local; CI mantiene Chromium.

## Cobertura y hallazgos

| Área | Evidencia |
| --- | --- |
| UI | Conexiones, Memos e importaciones en navegador: token privado, cambio de cuenta, dos pestañas, conflictos, errores, límites, borrado y logout. Se ejecuta por separado de la carga Docker. |
| Web/API/MCP/SSE | Propietarios derivados de credenciales verificadas; manipulación de propietario/rutas rechazada, DELETE Bearer prohibido y revisiones obsoletas rechazadas. Cambios y borrado no alteran el trabajo ni eventos de las otras nueve cuentas. |
| Admisión y credenciales | Las regresiones OIDC/MCP cubren invitación por rol, tokens inválidos o vencidos, issuer/audiencia/scope, cabeceras ambiguas, CSRF y revocación durante SSE. |
| Proveedores | Memos y las conexiones Jira se auditan con fixtures. Destinos y redirecciones no autorizados se rechazan; los errores no reemplazan trabajo. Jira como original/ADF todavía no está implementado. |
| Persistencia | Reinicio y recreación con la misma base y clave conservan las filas actuales y credenciales cifradas. Las regresiones adicionales cubren SIGKILL y fallo de escritura sin publicar eventos o estado parcial. |
| Privacidad | El runner examina logs, errores y almacenamiento en busca de credenciales sintéticas; los logs y respuestas denegadas también se revisan contra marcas de documentos. Las respuestas autorizadas contienen el trabajo de su propietario. |

La revisión inicial detectó cálculo y colas de actualizaciones sin una cota global, ausencia de un límite de complejidad/vista y una caché de propietarios que crecía sin expulsar vistas inactivas. Esta entrega incorpora los presupuestos anteriores. La lectura de fuentes utiliza un presupuesto separado; la prueba mantiene una fuente pausada y comprueba que otra cuenta puede leer su trabajo.

Durante la preparación, la fixture Keycloak terminó con `OOMKilled=true`. Se acotó su heap mediante `JAVA_OPTS_KC_HEAP`, se separó la construcción de la imagen de su arranque y el proxy de pruebas pasó a reenviar cuerpos por bloques de 64 KiB, sin acumular las diez peticiones grandes en memoria. La corrida final comprobó que el IdP siguió activo sin OOM. Esa configuración sirve para la fixture; no dimensiona un IdP de producción. La [guía oficial de contenedores Keycloak](https://www.keycloak.org/server/containers) explica el ajuste del heap y el dimensionamiento del servicio real.

## Reproducir

Necesitas Docker con cgroup v2, Compose, Python 3 y OpenSSL. Los controles de navegador necesitan además las dependencias de `tests/browser/` y Chromium o Chrome.

1. Construye la imagen antes de iniciar Keycloak:

   ```sh
   docker build --tag mddiff:capacity .
   ```

2. Ejecuta el contenedor y la carga con la fixture retirada incluso si falla:

   ```sh
   (
     set -e
     trap 'python3 spikes/identity/keycloak.py down' EXIT
     python3 spikes/identity/keycloak.py up
     python3 scripts/test_container.py --image mddiff:capacity
     python3 scripts/test_container.py --image mddiff:capacity --capacity
   )
   ```

3. Revisa `target/capacity-report.json`: solo contiene agregados, límites, entorno y referencias de imagen. Permanece ignorado y privado por defecto; si una comprobación falla, no interpretes un informe con estado `running` como aprobación.

GitHub Actions incorpora ambos recorridos Docker y conserva las regresiones del lector local, autenticación, persistencia, proveedores y navegador. El estado vigente de las corridas se consulta en [el PR WIP](https://github.com/joalceco/mddiff/pull/16); los números de este documento describen la corrida local indicada.

## Condiciones antes de publicar

Esta evidencia admite el perfil sintético descrito para diez sesiones; no certifica el cluster ni una exposición a internet. Antes de publicar deben cerrarse el origen HTTPS/IdP real, invitaciones, recuperación y respaldo de la clave/base/IdP, observabilidad y rollback de S14, y debe repetirse la comprobación con los recursos efectivos del host elegido.

Las funciones pendientes de Jira, selección de fuentes y coordinación requieren ampliar y ejecutar sus escenarios cuando se implementen; no heredan una validación de una ruta inexistente. Google/GitHub permanece pendiente por decisión del usuario. No se ejecutó un pentest exhaustivo, una prueba de larga duración, diez navegadores renderizando a la vez ni carga sobre servicios o documentos reales.
