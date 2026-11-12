# S13: aislamiento, cuotas y diez usuarios

Estado vigente: **2026-11-09**. Issue [#14](https://github.com/joalceco/mddiff/issues/14); simplificación [#18](https://github.com/joalceco/mddiff/issues/18). Rama `feature/multiuser-sources`.

El servicio recibe ambos documentos desde el agente o mediante entradas manuales. No incluye conectores. El [CI de `d76c07f`](https://github.com/joalceco/mddiff/actions/runs/42641193821) aprobó código y contenedor, incluida la carga sintética de diez usuarios, cuotas, reinicio y recreación del almacenamiento. Google/GitHub continúa aplazado.

## Límites del modo alojado

Las cuotas siguientes se aplican en el servidor. El lector local conserva su comportamiento y el máximo de 2 MiB por archivo.

| Recurso | Límite alojado |
| --- | --- |
| Documentos | 2 MiB UTF-8 por original o propuesta; etiquetas de hasta 255 bytes sin controles. |
| Complejidad por documento | 10 000 líneas, 32 768 eventos Markdown y 128 niveles de anidamiento. CR, LF y CRLF se cuentan como terminadores; CRLF cuenta una vez. |
| HTML aportado | Hasta 256 caracteres `<` en los eventos HTML del documento. Es una cota conservadora del formato, no un contador de nodos DOM. El HTML continúa pasando por el saneador. |
| Vista calculada | Hasta 24 MiB de JSON serializado, comprobados antes de escribir o publicar el cambio. No se almacena una segunda copia JSON para contar bytes. |
| Solicitudes de contenido | Hasta 16 admitidas globalmente y cuatro por identidad, incluidas las que esperan. Dos pueden leer/procesar cuerpos o enviar respuestas al mismo tiempo. |
| Espacios residentes | Hasta diez identidades con vista en memoria. Un espacio sin solicitudes, SSE o referencias activas puede salir de caché; su trabajo permanece en SQLite y se restaura al volver. Si los diez están ocupados, otra identidad recibe `429`. |
| Procesamiento | Dos tareas de cálculo simultáneas; hasta diez actualizaciones admitidas y dos por identidad. La restauración utiliza el mismo presupuesto de cálculo. |
| SSE | Hasta 40 conexiones globales y tres por identidad; se liberan cuando el servidor detecta el cierre del stream. Las sesiones y pestañas de una misma persona comparten su cuota. |

Las solicitudes de contenido incluyen `/api/work`, `/api/state` y `/mcp`. Estas cuotas no son un límite global de todo el tráfico HTTP ni una defensa contra DDoS. Los límites existentes de login siguen siendo 256 intercambios pendientes y 1024 sesiones en memoria; no equivalen a una capacidad demostrada para ese número de personas.

Las cuotas HTTP se conservan durante el cuerpo de la respuesta, también con un cliente lento. Un exceso devuelve `429` con `Retry-After: 1`; un exceso de documento, complejidad o vista devuelve `413`. Un conflicto de revisión devuelve `409`. Las herramientas MCP también pueden devolver un error estructurado con el estado correspondiente. Un reintento debe conservar la revisión esperada; no debe adoptar automáticamente una revisión nueva para forzar una escritura.

Cada propietario conserva su bloqueo de actualización. El semáforo de cálculo permite dos tareas; no sustituye esos bloqueos por un mutex global. Las actualizaciones que continúan después de una desconexión retienen sus permisos hasta terminar. Las pruebas verifican que cancelar al solicitante no permite ejecutar tareas fuera de cuota.

Los límites de complejidad y vista son nuevos respecto a S12: un documento menor de 2 MiB puede rechazarse por su estructura o expansión JSON. Antes de actualizar una instalación anterior, comprueba que su trabajo guardado cumple estas cotas y conserva una copia coherente. Un trabajo anterior que las exceda puede devolver `413` al restaurar su vista; su fila SQLite no se borra. Utiliza el binario anterior para exportarlo o reemplazarlo antes de actualizar. La incorporación de estas cotas no elimina automáticamente las filas que exceden los límites.

## Criterios de la medición

Se fijan estos umbrales para la prueba sintética de diez sesiones:

| Comprobación | Umbral |
| --- | --- |
| Documento de 4 KiB | p95 de PUT, propuesta MCP y lectura API de hasta 3000 ms por operación. |
| Documento de 2 MiB | p95 de esas operaciones de hasta 15 000 ms. |
| Memoria de mddiff | Pico de hasta 460 MiB dentro de un límite de 512 MiB, sin swap. |
| Correctitud | Cero fallos inesperados, mezcla de propietarios, pérdida de trabajo o terminaciones OOM. Los rechazos esperados se verifican por separado. |
| Continuidad | Recuperación exacta de diez trabajos, incluida una marca de borrado, tras reiniciar y recrear el contenedor. |

Son umbrales de aceptación de esta prueba, no un SLA ni una promesa sobre documentos o máquinas distintos.

## Método vigente y reproducción

El runner [`scripts/test_container.py --capacity`](../../scripts/test_container.py) usa diez propietarios distintos con sesión web, Bearer y SSE. Verifica documentos pequeños y cercanos a 2 MiB, propuestas por MCP, lecturas, aislamiento, rechazos, expulsión de caché y recuperación exacta tras reiniciar y recrear el contenedor. Los documentos y las credenciales son sintéticos; las credenciales no se incluyen en el informe.

| Componente | Presupuesto |
| --- | --- |
| mddiff release | UID/GID 10001, una CPU, 512 MiB, sin swap y 128 procesos. |
| Keycloak sintético | 1,5 CPU, 2 GiB y heap de 256–1024 MiB. Fuera del presupuesto del visor. |
| Host Docker | Al menos 6 GiB para alojar también cliente, proxy y sistema. El runner rechaza un host menor antes de crear recursos. |
| Cliente/proxy | HTTPS loopback con CA temporal verificada; fuera del presupuesto de mddiff. |

Se comprueban los límites efectivos de cgroups, el pico de memoria y el estado final de aplicación e identidad, incluidos `OOMKilled` y el código de salida. Un Mac con una VM Docker de 2 GiB agotó la memoria de Keycloak mientras el visor permaneció activo; esa ejecución no cuenta como aprobación de capacidad. La corrida completa del CI citado sí aprobó los umbrales. No se extrapolan sus percentiles a internet ni se atribuyen cifras de mediciones anteriores a esta imagen.

1. Construye la imagen antes de iniciar Keycloak:

   ```sh
   docker build --tag mddiff:capacity .
   ```

2. Ejecuta la fixture y ambos recorridos, retirando la identidad sintética al terminar:

   ```sh
   (
     set -e
     trap 'python3 spikes/identity/keycloak.py down' EXIT
     python3 spikes/identity/keycloak.py up
     python3 scripts/test_container.py --image mddiff:capacity
     python3 scripts/test_container.py --image mddiff:capacity --capacity
   )
   ```

3. Revisa `target/capacity-report.json`. Contiene agregados, entorno, límites e imagen; permanece ignorado. Un informe con estado `running` o una terminación fallida no acredita el cumplimiento.

## Cobertura y límites

El job `check` del mismo CI aprobó formato, Clippy, 51 pruebas Rust, MCP local, separación de modos, autenticación, recuperación durable, importaciones en Chromium, API/MCP sobre HTTPS y coordinación entre agentes/pestañas. El cliente Codex real se valida por separado con OAuth PKCE y cero turnos de modelo; consulta [la evidencia operativa](beta-operations.md).

La admisión usa rol, issuer, audiencia y scopes verificados. Las mutaciones por cookie requieren Origin y CSRF; las revisiones obsoletas y los excesos no publican un par parcial ni eventos para otro usuario. El modo alojado no acepta rutas del host.

La memoria se mide para el servicio Rust, no para diez navegadores. No se ejecutó una prueba prolongada, un pentest exhaustivo ni carga con documentos privados o proveedores externos. Esta prueba sintética no constituye un SLA. La publicación requiere además identidad, HTTPS, respaldo, observabilidad y recuperación en la instalación real; consulta [la operación de la beta](beta-operations.md).
