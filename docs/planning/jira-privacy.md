# Jira: inventario privado y distribución pendiente

Fecha: 2026-11-04. Issue: [#17](https://github.com/joalceco/mddiff/issues/17).

La rama incorpora un inventario privado por documento en SQLite 5. **No implementa todavía el reporte periódico ni la eliminación solicitada por Atlassian.** No se habilita Sharing ni se acepta su declaración. La beta publicada conserva el binario `6bfccea` y SQLite 4.

## Evidencia pública y alcance

La [guía vigente de privacidad de Jira](https://developer.atlassian.com/cloud/jira/platform/user-privacy-developer-guide/) exige reportar las cuentas cuyos datos personales almacena una app, conservar la antigüedad de la copia más antigua y atender las respuestas de actualización o borrado. Publica un endpoint específico para OAuth 3LO y un ciclo predeterminado de siete días, sujeto al encabezado del servicio.

Las [aclaraciones de personal de Atlassian sobre datos almacenados](https://community.developer.atlassian.com/t/storing-personal-data-in-user-properties-is-gdpr-compliant/24926) distinguen identificadores, contenido generado por usuarios y nombres de perfil asociados a una cuenta. Son respuestas históricas, de 2018–2019; no constituyen una exención general para cualquier información personal escrita en un documento. Este proyecto no intenta inferir cuentas Atlassian a partir de nombres o correos en texto libre.

El [contrato de menciones ADF](https://developer.atlassian.com/cloud/jira/platform/apis/document/nodes/mention/) permite identificadores de cuenta o nombres de colección. La mención también puede conservar un nombre visible. Por ello, guardar el fallback JSON no evita el tratamiento de datos personales. Un identificador ambiguo no acredita por sí solo un inventario completo.

La documentación consultada no determina específicamente cómo debe aplicar mddiff el borrado a sus copias restic. Las prácticas de recuperación de otros productos no sustituyen esa definición. No se ha enviado una consulta a soporte ni se condiciona el siguiente paso de implementación a obtener una respuesta externa.

## Comportamiento implementado en la rama

`src/privacy.rs` recorre el ADF, incluidos subárboles que el conversor conserva como JSON. Recoge identificadores estructurados de menciones y campos `accountId`, con límites de profundidad y tamaño. No añade nombres, correos ni texto del documento al inventario. Los documentos originales conservan el contenido recibido y sus avisos de conversión.

La columna privada `current_work.jira_privacy` conserva inventarios separados para original y propuesta. No se expone en `/api/work`, `/api/state` ni MCP; una entrada de documento tampoco puede proporcionarla. Cada registro contiene el identificador y la fecha de lectura del proveedor, en segundos Unix. No utiliza la fecha de modificación del issue.

| Cambio del trabajo | Efecto en el inventario |
| --- | --- |
| Capturar otro origen | Sustituye el inventario del original; conserva el de la propuesta existente. |
| Revisar una propuesta mediante API o agente | Conserva referencias del original y de la propuesta anterior; para una misma cuenta utiliza la fecha más antigua. |
| Reemplazar explícitamente un lado con texto o archivo | Retira el inventario sustituido; el flujo de propuesta conserva las referencias del original actual. |
| Rechazar una revisión antigua o fallar el guardado | Conserva documentos, inventario y revisión anteriores. |
| Borrar el trabajo | Vacía ambos inventarios junto con el contenido vigente. No elimina copias de respaldo. |

La conservación de referencias en propuestas derivadas es deliberadamente conservadora: el agente puede copiar nombres del original o de una propuesta anterior. El inventario no pretende comprobar si cada nombre sigue apareciendo literalmente.

La migración 4→5 añade una columna, sin cambiar documentos, revisiones, conexiones o procedencia. Los documentos preexistentes quedan pendientes de clasificación: versiones anteriores podían haber sustituido la procedencia Jira sin borrar su propuesta. No se inventan identificadores ni fechas. Las menciones ambiguas, los identificadores inválidos y `unknown` también mantienen una marca pendiente; `unknown` no se incorpora a la lista de cuentas.

Un binario compatible solo con SQLite 4 rechaza el esquema 5. Antes de desplegar esta rama es necesario adaptar y probar respaldo, restauración y rollback; no se debe reducir `user_version` manualmente.

## Trabajo necesario antes de Sharing

1. Agregar el inventario de todas las copias conservadas, incluidas las de respaldo, y resolver los documentos pendientes de clasificación.
2. Implementar el reporte 3LO con estado durable, ciclos, límites, reintentos y credencial de operación protegida.
3. Atender actualización y borrado sin depender de que la persona mencionada tenga cuenta en mddiff, incluida la protección frente a restaurar datos ya borrados.
4. Probar la recuperación y alinear la información pública de privacidad con el comportamiento verificado.
5. Cuando la declaración sea verdadera, completar la autorización de distribución y comprobar una conexión con otra cuenta Atlassian.

Google/GitHub permanece fuera de esta entrega por decisión del usuario. El acceso de lectura Jira ya verificado con la cuenta propietaria no demuestra distribución a invitados.

## Verificación

Las pruebas Rust cubren extracción bajo nodos desconocidos, identificadores ambiguos, límites, copia de referencias a propuestas, fecha más antigua, cambios de origen, rechazo de revisiones antiguas, aislamiento, reinicio, borrado y migración 4→5. `scripts/test_jira_source.py` verifica además la captura real del conector contra proveedores sintéticos, el inventario persistido por propietario y su ausencia en las respuestas públicas del usuario.

Estas pruebas no acreditan el endpoint de reporting de Atlassian, borrado de respaldos ni Sharing. Esos pasos siguen pendientes y el issue permanece abierto.
