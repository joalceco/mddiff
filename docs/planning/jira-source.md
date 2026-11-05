# S09: capturar una descripción de Jira Cloud

Issue: [#10](https://github.com/joalceco/mddiff/issues/10). La captura usa la conexión personal Jira Cloud existente, no una credencial compartida entre usuarios.

Estado al **2026-11-05**: la beta usa `0e1c4a3`/SQLite 6 con barrera externa activa y un primer ciclo manual verificado sobre el estado vigente y siete respaldos. Se verificaron OAuth, lectura real de una descripción Jira y comparación mediante Codex/MCP con la cuenta propietaria; el nuevo permiso de usuarios pasó además una prueba local aislada. La app sigue privada y el timer de privacidad pendiente; [#17](https://github.com/joalceco/mddiff/issues/17) bloquea la distribución a otras cuentas Atlassian.

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

## Clasificación de referencias de cuenta

La rama clasifica las referencias estructuradas antes de guardar una captura. Las menciones ADF con `userType: DEFAULT` identifican personas; `APP` y `SPECIAL` quedan fuera del inventario de personas. Si falta el tipo o es `null`, consulta únicamente los identificadores válidos presentes en el ADF mediante el endpoint del sitio aprobado:

```text
GET https://api.atlassian.com/ex/jira/{cloudId}/rest/api/3/user/bulk?startAt=0&maxResults=100&accountId=…
```

Los campos estructurados `accountId` siguen el mismo criterio. Se deduplican las cuentas y se aceptan solo `accountType` conocidos: `atlassian` y `customer` son personas; `app` se excluye. Un tipo explícito desconocido, una cuenta ausente o `unknown` impiden confirmar la captura. No se deducen cuentas a partir de nombres o correos del texto.

El lookup admite hasta 500 identificadores, lotes de 100, 16 páginas y 20 segundos en total; cada respuesta conserva el límite de 256 KiB. La aplicación construye la paginación y nunca sigue `nextPage`. Rechaza cuentas no solicitadas, duplicados y respuestas inconsistentes. Las consultas usan la credencial personal y las mismas restricciones de red; no existe una caché compartida entre usuarios.

Solo se usa la correspondencia temporal entre identificador y tipo. El ADF, las etiquetas originales de sus menciones y el Markdown convertido no reciben datos del perfil consultado. El inventario conserva la fecha de lectura de la descripción, anterior a las consultas de clasificación. Un error de permiso devuelve `502`; límites, `413`; cuentas sin resolver, `422`; tiempo total agotado, `504`. Un rate limit conserva `429`. Todos mantienen documentos, inventarios, revisiones y eventos anteriores.

El endpoint de [consulta de usuarios por identificador](https://developer.atlassian.com/cloud/jira/platform/rest/v3/api-group-users/#api-rest-api-3-user-bulk-get) requiere `read:jira-user` y Atlassian lo marca **Experimental** (consultado el 2026-11-05). Un grant antiguo puede seguir leyendo descripciones sin consultas adicionales; si estas son necesarias y carece de permiso, debe reconectarse. El permiso real ya está configurado y consentido; la rama todavía no está desplegada en la beta.

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

## Validación real y distribución

El 2026-11-04, con el secreto externo privado configurado, la cuenta propietaria completó OAuth y una cuenta desechable de mddiff abrió la descripción de un issue real autorizado. El visor mostró referencia, captura, `adf-md-v1` y avisos. Codex real leyó ese trabajo y envió una propuesta sintética por MCP: el original y su procedencia permanecieron iguales, otra cuenta quedó vacía y una revisión antigua recibió `409`. No hubo turnos de modelo ni escrituras en Jira. Esta prueba acredita un caso real, no todo ADF ni otra cuenta Atlassian; esos contratos conservan sus fixtures sintéticas.

La consola de Atlassian exige una atestación sobre **Personal Data Reporting API** para aplicaciones que almacenan datos personales antes de habilitar Sharing. La rama incluye el reporter, pero su operación periódica y la barrera de recuperación todavía no están activas en la beta. [#17](https://github.com/joalceco/mddiff/issues/17) conserva este bloqueo; no se ha emitido la atestación ni se habilita autorización para otras cuentas. Consulta [la configuración de consentimiento](personal-connections.md#consentimiento-de-jira-cloud) para separar la aplicación privada de su distribución.

La rama incorpora el inventario privado de cuentas y antigüedad, la herramienta de reporte y una [barrera externa de borrado](jira-privacy.md) en SQLite 6, todavía sin desplegar. Ese avance no acredita la operación periódica, la recuperación del cluster con la barrera ni la distribución a invitados.

Verificado el 2026-11-05: pasan las 88 pruebas Rust, Clippy y las integraciones de Jira y conexiones con Keycloak sintético. La captura se prueba con la barrera externa activa, menciones sin tipo, paginación, permisos antiguos y cuentas app/persona diferentes por usuario. También se verifican errores de clasificación, límite de 500 cuentas, desconexión y cierre de sesión durante el lookup, propuestas MCP y recuperación del inventario tras reiniciar. Estas pruebas sintéticas no acreditan por sí solas el nuevo scope ni el lookup contra Jira real.

## Validación real del permiso de usuarios

El 2026-11-05, con autorización del propietario, se configuró `read:jira-user` y se repitió el consentimiento de la app real. `accessible-resources` confirmó ambos permisos de lectura y el único sitio seleccionado. El binario `0e1c4a3`, servido en loopback con Keycloak sintético y un registro externo vacío activo, capturó una descripción real del sitio autorizado. Una propuesta enviada por MCP conservó exactamente el original y su procedencia; una revisión antigua recibió `409` y otra identidad quedó sin trabajo ni conexiones.

El documento elegido no contenía cuentas estructuradas, por lo que esa captura no ejercitó la clasificación de menciones sin tipo. Para validar el contrato real del endpoint, el operador consultó la propia cuenta que dio el consentimiento y obtuvo `200`, una página de una cuenta y `accountType: atlassian`. Los perfiles se descartaron sin incorporarlos al Markdown ni al inventario del documento. Los casos ambiguos, la paginación y los fallos del conector conservan sus pruebas sintéticas; no se atribuyen al documento real.

Se retiraron el callback temporal, las copias locales de documentos y claves, la copia local del grant temporal y la identidad sintética. El contenedor de Keycloak volvió a su estado inicial detenido. La credencial anterior y exclusiva del operador renovó después del nuevo consentimiento y mantuvo acceso al sitio, sin reemplazarla ni enviar un reporte de producción durante esa prueba. Después se activó `0e1c4a3`/SQLite 6 y pasó el primer [ciclo manual productivo](jira-privacy.md#activación-productiva-del-2026-11-05). Sharing sigue deshabilitado.

El [CI completo de `0e1c4a3`](https://github.com/joalceco/mddiff/actions/runs/42101018303) aprobó código y contenedor, incluidas las regresiones de navegador y capacidad de diez usuarios. La imagen candidata del mismo commit pasó además las pruebas de privacidad y 21 casos del coordinador en Linux, con un ciclo de captura, borrado y recuperación sintéticos. La política `findmnt` y Nginx se simularon en ese último ensayo; no acredita la activación del ciclo en el servicio publicado.

## Fuentes primarias

Consultadas el 2026-10-29: [REST API Jira Cloud v3](https://developer.atlassian.com/cloud/jira/platform/rest/v3/), [Get issue y campos solicitados](https://developer.atlassian.com/cloud/jira/platform/rest/v3/api-group-issues/#api-rest-api-3-issue-issueidorkey-get), [estructura de ADF](https://developer.atlassian.com/cloud/jira/platform/apis/document/structure/), [marcas de enlace](https://developer.atlassian.com/cloud/jira/platform/apis/document/marks/link/) y [celdas combinadas](https://developer.atlassian.com/cloud/jira/platform/apis/document/nodes/table_cell/).

Tras verificar la comparación visible, se borraron original y propuesta de prueba, se desconectó su credencial Jira y se retiraron las cuentas desechables antes del respaldo. No se publican el sitio, la clave del issue, su contenido, las capturas privadas ni los secretos. El trabajo del propietario de mddiff no se modificó.
