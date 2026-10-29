# S09: capturar una descripción de Jira Cloud

Issue: [#10](https://github.com/joalceco/mddiff/issues/10). La captura usa la conexión personal Jira Cloud existente, no una credencial compartida entre usuarios.

Estado al **2026-10-29**: el código está desplegado con SQLite 4 y las pruebas sintéticas pasan; #10 sigue abierto porque aún no se ha verificado una lectura real de Jira. Falta un secreto OAuth válido configurado por el operador. La autorización para otras cuentas de Atlassian está bloqueada por [#17](https://github.com/joalceco/mddiff/issues/17); **Sharing** permanece deshabilitado.

## Resultado y alcance

Una persona autenticada abre un issue por clave o por una URL `/browse/CLAVE` del sitio autorizado. El backend obtiene únicamente `summary`, `description` y `updated` mediante Jira REST API v3 y convierte la descripción ADF a una representación Markdown verificable. El título y la referencia quedan como metadatos; no se incorporan al cuerpo de la descripción.

La captura sustituye el original y conserva la propuesta. El original queda congelado hasta otra lectura explícita: los cambios posteriores en Jira no actualizan silenciosamente el trabajo. Una revisión de trabajo obsoleta, desconexión, revocación, fallo de red, respuesta inválida o exceso de límites conserva la última comparación confirmada.

No se cargan comentarios, adjuntos, campos adicionales ni recursos enlazados. Los únicos POST al proveedor pertenecen al intercambio y refresco OAuth; no hay escrituras de negocio a Jira. La aplicación no convierte la propuesta de vuelta a ADF ni publica cambios.

## Contrato de captura

`POST /api/sources/jira` requiere una sesión web admitida, Origin exacto y CSRF; el Bearer de comparación no permite importar fuentes externas.

```json
{
  "destination": "jira-alpha",
  "reference": "PROJ-123",
  "expected_revision": 7
}
```

La misma referencia puede expresarse como `https://team-alpha.atlassian.net/browse/PROJ-123`, si ese sitio está autorizado por administración y la conexión personal lo incluye. Se normalizan las letras de la clave a mayúsculas. Se rechazan otros sitios, parámetros, fragmentos, credenciales, rutas API, escapes porcentuales, barras invertidas y segmentos de navegación. La entrada solo selecciona una clave: la URL de red se construye a partir del cloud ID administrado, nunca a partir de una URL libre del usuario.

El backend verifica las credenciales y el consentimiento del sitio con `accessible-resources`, y realiza esta lectura:

```text
GET https://api.atlassian.com/ex/jira/{cloudId}/rest/api/3/issue/{key}?fields=summary,description,updated
```

Conserva TLS, verificación y fijación DNS por solicitud, prohibición de proxies de entorno y redirecciones y los límites de las conexiones existentes. El token propio se refresca si caduca; la rotación se guarda antes de continuar, con un mutex por usuario/destino. La tarea de refresco mantiene su permiso de capacidad aunque se desconecte su solicitante HTTP.

La respuesta es el snapshot habitual. `base_source` añade los campos opcionales siguientes, conservados también por `GET /api/work` y MCP:

```json
{
  "provider": "jira",
  "destination": "jira-alpha",
  "resource": "PROJ-123",
  "url": "https://team-alpha.atlassian.net/browse/PROJ-123",
  "captured_at": 1793232000,
  "updated_at": "2026-10-29T00:00:00.000+0000",
  "title": "Título del issue",
  "converter": "adf-md-v1",
  "warnings": ["Nodo ADF no soportado; se conserva su tipo, atributos y contenido como JSON."]
}
```

`warnings` se omite si está vacío. El título admite hasta 1024 bytes sin controles; la fecha es opcional y acotada. Si Jira devuelve una clave canónica diferente por un cambio de referencia, se conserva esa clave y se añade un aviso explícito. La etiqueta del documento es `Jira · PROJ-123.md`.

## Alcance de ADF v1

La versión `adf-md-v1` es determinista para el mismo documento. El diff compara esta representación importada; no afirma que Jira almacene Markdown nativo.

Los fragmentos contiguos con las mismas marcas se fusionan antes de aplicar delimitadores Markdown. Las fronteras ambiguas de énfasis o código usan comentarios invisibles; cuentan contra el límite existente de 256 fragmentos HTML del visor. Una captura que excede esa cota se rechaza sin cambiar el trabajo.

| Contenido | Representación |
| --- | --- |
| Párrafos y encabezados 1–6 | Texto escapado y encabezados Markdown; se conserva el orden del documento. |
| Énfasis, negritas, tachado, código y enlaces | Marcas Markdown; enlaces HTTP/HTTPS/mailto seguros. Los enlaces no seguros o relativos se conservan como datos sin activarlos. |
| Listas, citas y separadores | Listas anidadas con orden inicial, citas y separadores Markdown. |
| Código y tablas rectangulares simples | Cercas más largas que los backticks del contenido; tablas con encabezado o encabezado vacío y celdas de párrafos. |
| Nodos o marcas no representables | Aviso visible y representación identificable de sus datos, sin omitirlos silenciosamente. |

Los nodos desconocidos —por ejemplo medios, extensiones o tarjetas— permanecen como JSON cercado, con tipo, atributos y contenido. Las marcas no compatibles se identifican junto al texto. Los atributos visuales o futuros que Markdown no reproduce se conservan como JSON junto al contenido legible y generan un aviso; solo se omiten los IDs locales del editor, que no forman parte del contenido ni de su presentación. Tablas con combinaciones de celdas, encabezados parciales/intermedios o contenido de celda complejo se conservan completas como JSON con aviso; no se aplanan fingiendo una tabla íntegra. No se descargan medios por representarlos.

La conversión acepta únicamente raíz ADF `doc`, versión `1`, con una lista de contenido. Tiene cotas de 64 niveles, 32 768 nodos, 32 marcas por texto, avisos acotados y 2 MiB de Markdown final. Las respuestas JSON se acotan a `6 × 2 MiB + 64 KiB`; esto no amplía el límite final del documento. La conversión se ejecuta fuera de los workers de red y comparte la cota de cómputo del diff; sus permisos se conservan hasta terminar aunque el solicitante se desconecte. Las cuotas y controles de complejidad del visor también se aplican antes de confirmar la captura.

## Fallos definidos

| Situación | Resultado |
| --- | --- |
| Descripción ausente o `null` | `422`, sin reemplazar el original. Un documento ADF vacío válido sí puede capturarse como descripción vacía. |
| Falta de permiso o issue no visible | `502` con mensaje de permiso del proveedor, o `404`; sin exponer el cuerpo privado de error. Un `403` propio de mddiff conserva su significado de autorización de la aplicación. |
| Rate limit | `429`; espera antes de una nueva lectura explícita. No se programa un reintento que pueda sobrescribir trabajo nuevo. |
| Respuesta excesiva, conversión excesiva o Markdown >2 MiB | `413`; nunca se trunca o guarda parcialmente. |
| ADF inválido, credencial revocada, redirección, red o proveedor no disponible | Error de proveedor; no cambia el trabajo vigente. |

El commit comprueba nuevamente la revisión del trabajo y la generación de la conexión dentro de la transacción. La sesión web se revalida después de la lectura. Una propuesta del agente recibida durante una descarga, una desconexión o un reemplazo de credenciales impiden que una lectura tardía restaure una base antigua.

## Verificar el contrato

Las fixtures usan sitios, cuentas y textos sintéticos. Con la fixture Keycloak aislada iniciada y el binario `release` actualizado:

```sh
cargo test --lib adf --locked
python3 scripts/test_jira_source.py
```

Las pruebas definen resultados concretos de Markdown para párrafos, marcas, listas anidadas, enlaces, tablas y código; también comprueban su significado con el renderizador. El contrato HTTP audita todos los campos y métodos para rechazar comentarios/adjuntos o escrituras, y cubre cuentas separadas, permisos, referencias, límites, avisos persistentes, refresco rotatorio, carreras, agente y reinicio.

Las pruebas sintéticas no acreditan una conexión personal real a Jira Cloud. Una lectura consentida requiere configurar el cliente OAuth y el sitio autorizados y completar la autorización personal; no se incorporan credenciales ni documentos reales a estas fixtures.

Verificado el 2026-10-29: pasan los seis tests unitarios de conversión ADF, la suite Rust completa, Clippy, la compilación `release` y la integración `scripts/test_jira_source.py` con Keycloak y proveedor sintéticos. La corrida final incluye marcas contiguas renderizadas, rechazo de complejidad sin cambiar el trabajo, renovación de credenciales, desconexión y cierre de sesión durante una captura, preservación de la propuesta y recuperación tras reiniciar. La [corrida de CI 40904080796](https://github.com/joalceco/mddiff/actions/runs/40904080796) aprobó `check` y `container`, incluidas 70 pruebas Rust, regresiones y diez usuarios sintéticos.

El commit `6bfccea841530e4cf8a49fc5ab7b1e5d9f670070` está desplegado con SQLite 4. La [evidencia operativa](beta-operations.md#actualización-del-2026-10-29) registra migración y rollback, cliente Codex público y los límites de cada comprobación. Ninguno de esos resultados acredita la lectura real de Jira.

## Validación real pendiente y distribución

Una vez configurado el secreto privado válido, la cuenta propietaria de la aplicación 3LO puede consentir una prueba sin habilitar Sharing. El código no exige distribuir la aplicación para esa prueba. No se ha registrado aún un consentimiento o una lectura reales.

La consola de Atlassian exige una atestación sobre **Personal Data Reporting API** para aplicaciones que almacenan datos personales antes de habilitar Sharing. Esa API no está implementada en mddiff. [#17](https://github.com/joalceco/mddiff/issues/17) conserva este bloqueo; no se ha emitido la atestación ni se habilita autorización para otras cuentas. Consulta [la configuración de consentimiento](personal-connections.md#consentimiento-de-jira-cloud) para separar la aplicación privada de su distribución.

## Fuentes primarias

Consultadas el 2026-10-29: [REST API Jira Cloud v3](https://developer.atlassian.com/cloud/jira/platform/rest/v3/), [Get issue y campos solicitados](https://developer.atlassian.com/cloud/jira/platform/rest/v3/api-group-issues/#api-rest-api-3-issue-issueidorkey-get), [estructura de ADF](https://developer.atlassian.com/cloud/jira/platform/apis/document/structure/), [marcas de enlace](https://developer.atlassian.com/cloud/jira/platform/apis/document/marks/link/) y [celdas combinadas](https://developer.atlassian.com/cloud/jira/platform/apis/document/nodes/table_cell/).
