# Comparaciones enviadas por el agente

Decisión del propietario: 2026-11-05. Sustituye el alcance anterior de conectores personales.

El agente obtiene los documentos mediante sus propias herramientas y envía al visor el contenido Markdown de la versión previa y la actual. mddiff autentica al agente, compara los textos y conserva únicamente el último par de su usuario. La aplicación no conecta con servicios documentales ni recibe sus credenciales.

## Contrato

1. Llama a `get_current_work` para obtener `revision`, incluso en un espacio vacío.
2. Llama a `submit_comparison` con `expected_revision`, `base: {label, markdown}` y `candidate: {label, markdown}`. `base` es la versión previa y `candidate` la actual.
3. Muestra `viewer_url`. El navegador de esa identidad recibe la actualización mediante SSE.

Cada documento admite hasta 2 MiB UTF-8 y los límites existentes de complejidad. El par se confirma en una sola transacción. Si un documento es inválido o la revisión cambió, se conserva el par anterior. Un conflicto requiere volver a leer y decidir; el agente no debe forzar una revisión nueva automáticamente. No se aceptan rutas del servidor ni un propietario elegido por el cliente.

La API equivalente es `PUT /api/work` con el mismo cuerpo. Usa un Bearer personal de mddiff; las sesiones web mantienen su protección CSRF. `submit_proposal` y `PATCH /api/work` permiten actualizar únicamente el segundo documento de un par existente.

## Retiro de conectores

Se retiran controles, endpoints, consentimiento, captura remota, conversión ADF, credenciales y operación específica de Jira/Memos. Keycloak sigue proporcionando identidad a mddiff. Google/GitHub permanece aplazado. El lector local conserva su contrato por rutas y las entradas web de texto y archivo continúan disponibles.

El esquema SQLite 7 conserva por identidad las etiquetas, Markdown, revisiones y metadatos de entrada. La migración desde esquemas anteriores elimina las credenciales y metadatos de conectores. No modifica los textos ni reinicia sus revisiones. Los binarios antiguos no deben abrir la base nueva. El despliegue debe actualizar también configuración, respaldos y recuperación; un respaldo antiguo puede conservar credenciales durante su retención.

## Criterios de entrega

- El agente abre ambos documentos desde un espacio vacío, con comparación automática y aislamiento por usuario.
- Los errores de contenido o revisión no reemplazan parcialmente el par.
- El visor y el servidor no ofrecen conectores ni solicitan credenciales documentales externas.
- Reinicio, borrado, checkpoints y migración conservan los contratos del trabajo actual.
- La configuración publicada, la documentación y las pruebas de contenedor/capacidad utilizan el nuevo flujo. El respaldo y el rollback se verifican antes de dar por terminado el despliegue.
