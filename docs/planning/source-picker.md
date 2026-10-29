# S10: elegir el origen y revisar su procedencia

Issue: [#11](https://github.com/joalceco/mddiff/issues/11). Rama: `feature/multiuser-sources`.

El modo alojado permite elegir el origen desde las cabeceras existentes, sin añadir un lateral. Cada lado mantiene sus controles de texto y archivo; el original añade Jira Cloud y Memos, y la propuesta explica cómo recibir contenido del agente.

| Lado | Opciones | Resultado |
| --- | --- | --- |
| Original | Pegar texto, subir Markdown, abrir Jira o abrir Memos | Sustituye el original con la revisión vigente y conserva la propuesta. |
| Propuesta | Pegar texto, subir Markdown o recibir del agente | Actualiza la propuesta; el original y su captura se conservan. |

Los controles son botones nativos agrupados y etiquetados por lado. Los diálogos mantienen foco, cierre con Escape, estado de carga y errores con texto. En móvil, los detalles de procedencia pueden desplazarse sin ocupar todo el espacio del documento. El modo local sigue usando rutas y su botón de recarga; no muestra opciones alojadas.

## Qué muestra la captura

Jira y Memos muestran referencia, enlace validado y momento de lectura. Jira puede añadir título, fecha de actualización remota, versión de conversión y avisos. Los avisos de ADF aparecen como texto legible, con una marca y lista de detalles; no dependen únicamente del color. El contenido no representable conserva su representación identificable dentro del Markdown convertido.

Las importaciones manuales muestran el tipo de entrada y el momento recibido por el servidor. `Snapshot` incluye `base_input` y `candidate_input` con `kind` y `captured_at`; el frontend no inventa fechas ni deduce el origen por el nombre del archivo. Los documentos antiguos sin esa información no reciben una fecha ficticia. La propuesta recibida por MCP se identifica como **Agente · MCP**.

Los títulos y avisos se insertan mediante `textContent`. Los enlaces de procedencia admiten HTTPS sin credenciales; HTTP se reserva al origen loopback de las pruebas. Se abren con `noopener noreferrer`.

## Actualizar requiere una acción explícita

Una captura de Jira o Memos permanece fija aunque el contenido remoto cambie. SSE actualiza el trabajo guardado; no vuelve a leer proveedores. **Actualizar origen** vuelve a leer únicamente la referencia capturada con su conexión personal y la revisión observada al pulsar el botón.

La petición usa `POST /api/sources/jira` o `POST /api/sources/memos`, con `destination`, `reference` y `expected_revision`. Un fallo conserva el trabajo válido. La falta de permiso del proveedor se presenta como un error de lectura, sin cerrar la sesión de mddiff ni vaciar la comparación. Si otra pestaña o un agente modifica el trabajo durante la lectura, el servidor rechaza el commit obsoleto con `409`; la interfaz conserva la revisión reciente y exige otro clic para reintentar. No hace un reintento automático sobre una revisión nueva.

Las lecturas `GET /api/*` de la interfaz incluyen el token de la sesión que identifica la pestaña. El backend comprueba esa asociación cuando se envía el header. Los eventos SSE alojados también se vinculan a la sesión: un identificador ausente o distinto vacía la vista antes de pedir contenido. Esto impide que una pestaña antigua muestre datos de otra cuenta tras cambiar la cookie en otra pestaña.

## Verificación de interfaz

El 2026-10-29 se ejecutó `tests/browser/sources.mjs` en Chrome con respuestas sintéticas. Cubre metadatos manuales recuperados tras recargar, selección de conexiones Jira, título/referencia/fechas/avisos, ausencia de sincronización remota, actualización explícita, fallos y carreras con un agente, revisión esperada, cambio de cuenta, eventos sin sesión válida, teclado, móvil y lector local. Las regresiones existentes de Memos y conexiones se comprueban por separado.

```sh
MDDIFF_BROWSER_CHANNEL=chrome node tests/browser/sources.mjs
```

Las capturas de verificación permanecen ignoradas en `target/browser/`. Esta prueba verifica la interfaz con contratos simulados; no acredita por sí sola OAuth de una cuenta Jira real, persistencia de backend ni el despliegue. La [guía de Jira](jira-source.md) y las suites integradas cubren esos contratos por separado.

El mismo día pasó `scripts/test_hosted_imports.py` contra el backend y Keycloak reales de la fixture, con Chrome. Comprobó tipo y momento de recepción de texto/archivo en ambos lados, persistencia al recargar y conservación de la propuesta y sus metadatos al reemplazar solo el original.

## Decisiones de interfaz

El objetivo visual es el visor existente: fondo oscuro, tipografía del sistema, metadatos monoespaciados, bordes discretos y cabeceras compactas. No se añadió otra paleta ni una biblioteca de componentes.

| Decisión | Referencia y adaptación |
| --- | --- |
| Conservar densidad y roles de color | [Linear Changelog](https://linear.app/changelog), consultado mediante Refero: superficies grises y contraste sobrio. Se conservan los tokens del visor existente. |
| Acciones claras sin un selector redundante | [shadcn/ui](https://ui.shadcn.com), consultado mediante Refero: controles semánticos compactos y separación por bordes; se reutilizan botones nativos. |
| Elegir proveedor y abrir un diálogo enfocado | [Cycle, Add sources](https://refero.design/pages/577c0811-0f7f-4f04-a2ac-b9f4454a8727) y [Linear, integrations](https://refero.design/pages/96a95e41-7c23-4cf7-85fd-2485e62dbe26): se adapta la selección explícita y el diálogo, sin copiar sus grids o laterales. |
| Teclado y estados | Refero `craft-details`: foco visible, labels asociados, diálogo nativo y anuncios de carga/error con texto. |
