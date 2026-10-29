# S11: coordinar pestañas y agentes por usuario

Fecha: 2026-10-29. Issue: [#12](https://github.com/joalceco/mddiff/issues/12). Rama: `feature/multiuser-sources`.

Estado: #12 cerrado. El código está desplegado en la beta con el commit `6bfccea841530e4cf8a49fc5ab7b1e5d9f670070` y SQLite 4. La [evidencia operativa](beta-operations.md#actualización-del-2026-10-29) registra la recuperación del trabajo nuevo y el recorrido real de Codex sobre esa imagen publicada.

La coordinación conserva un único trabajo por identidad verificada. API, MCP y pestañas propias comparten su revisión; una solicitud atrasada recibe `409` y no reemplaza el trabajo vigente. Los canales SSE pertenecen al mismo propietario que el contenido. Esta entrega no añade colaboración entre usuarios ni un historial de eventos.

## Contrato de coordinación

| Operación | Comportamiento |
| --- | --- |
| Leer o recibir eventos | El servidor verifica la sesión o el Bearer y obtiene `issuer` + `sub`. Ningún campo del cliente selecciona otro propietario. SSE solo admite sesión web. |
| Reemplazar, borrar o proponer | El commit comprueba `expected_revision` bajo el bloqueo del propietario y en almacenamiento. Solo una solicitud puede ganar una misma revisión. |
| Capturar un origen remoto | La petición conserva la revisión inicial y la generación de conexión. Al terminar la lectura vuelve a validar sesión y conexión; el commit rechaza revisiones superadas. |
| Desconectar y reconectar SSE | La nueva suscripción comienza con la revisión vigente del propietario. Recupera el estado actual; no reproduce cada evento perdido. |
| Reiniciar | Se recupera el trabajo persistente, incluida una revisión de borrado. Las sesiones web en memoria se vuelven a autenticar. |

El procesamiento mantiene un bloqueo por espacio, no un mutex global de todos los usuarios. Las cotas globales y por propietario limitan solicitudes, cómputo, actualizaciones y streams. Un cálculo que continúa tras desconectar al solicitante conserva sus permisos hasta terminar. Los límites y la medición de diez sesiones están en [S13](capacity-validation.md).

## Vincular cada pestaña a su sesión

Cambiar la cuenta desde otra pestaña cambia las cookies compartidas. Una pestaña antigua debe vaciarse; no debe adoptar el nuevo espacio mientras todavía muestra la identidad anterior.

El visor envía el nonce CSRF que ya tiene en sus lecturas `GET /api/*`. Si está presente, el backend lo compara con la sesión del cookie antes de leer el espacio. El valor no selecciona propietario ni reemplaza el cookie. Una discrepancia devuelve `403`; una lectura válida de otro cliente sin ese header sigue dependiendo de su identidad verificada.

Los eventos SSE llevan ese nonce en el campo `id`; el contenido `data` sigue siendo la revisión numérica. El visor descarta un evento cuyo identificador no corresponde a su sesión. No se introduce el nonce en la URL ni se utiliza `Last-Event-ID` para abrir otro espacio o recuperar un historial.

La comprobación se vincula a la solicitud y al evento reales. Consultar únicamente la sesión después de recibir un documento no cubre una secuencia Alice → Bob → Alice entre la petición y la respuesta: el último chequeo podría volver a coincidir con Alice aunque el documento viniera de Bob.

## Matriz de evidencia

| Criterio de la historia | Comprobaciones |
| --- | --- |
| API, SSE y MCP privados, incluida reconexión | `scripts/test_coordination.py`: lectura con cookie Bob/nonce Alice rechazada; identificador SSE ligado a su sesión; dos consumidores propios y otro usuario; revisión perdida recuperada. `scripts/test_hosted_mcp.py`: emisor, audiencia, scope y propietario no seleccionable. |
| Dos pestañas reflejan reemplazo y borrado | `tests/browser/coordination.mjs`: dos páginas reales del mismo contexto, actualización por API, reconexión retenida en una barrera y borrado desde la interfaz; otro contexto conserva su trabajo. |
| Importación lenta no sobrescribe una revisión posterior | `scripts/test_memos_source.py`: proveedor retenido mediante `Event`, actualización ganadora y liberación de la lectura; el resultado atrasado devuelve `409`. También prueba desconexión y logout durante la captura. |
| Sin mutex global y con cotas | Pruebas Rust `owners_have_separate_content_revisions_events_and_coordination`, `hosted_quotas_isolate_owners_and_release_closed_streams` y `disconnected_updates_retain_compute_and_queued_job_quotas`; el escenario de capacidad comprueba que otra persona puede leer mientras un proveedor está detenido. |
| Revocación, reinicio y agentes concurrentes | `scripts/test_hosted_auth.py`: logout, retirada de rol y revocación del IdP cierran un SSE existente. `scripts/test_coordination.py`: dos propuestas MCP arrancan en una barrera, una gana y la otra recibe `409`; luego reinicia el proceso con los mismos datos. |

El recorrido de navegador añade una prueba de doble cambio de cookie con dos sesiones reales independientes: retiene la respuesta de estado de Bob, restaura el cookie válido de Alice y entrega esa respuesta a su pestaña antigua. El nonce de la solicitud impide que el contenido de Bob llegue al visor de Alice. La prueba no imprime cookies, tokens ni formularios de autenticación.

## Ejecutar las comprobaciones

Con el binario `release`, las dependencias de navegador y la fixture Keycloak sintética ya preparados:

```sh
python3 scripts/test_coordination.py
```

Para utilizar el Chrome instalado, configura `MDDIFF_BROWSER_CHANNEL=chrome` en ese mismo comando. El runner no inicia ni elimina Docker; reutiliza la fixture privada de [autenticación](hosted-auth.md#comprobar-con-la-fixture-sintética), crea sus propios procesos y base temporal y restaura los clientes OAuth modificados al salir. Ejecuta las integraciones que comparten esos clientes de forma secuencial.

La prueba solo admite el issuer y las direcciones de loopback de la fixture. No la dirijas a la beta publicada ni uses credenciales personales. Las carreras usan barreras de threads o interceptores de red controlados; los timeouts sirven para detectar bloqueos y no para ordenar escrituras.

## Estado de verificación

La revisión estática y las comprobaciones de sintaxis de los runners nuevos pasaron. El 2026-10-29, la ejecución integrada de S11 aprobó las pruebas de coordinación y el recorrido real de Chrome, incluida la secuencia Alice → Bob → Alice. La corrida final local repitió el recorrido y comprobó que una propuesta MCP registra el origen `agent`. También pasaron autenticación, persistencia, API/MCP, conexiones y Memos con proveedores sintéticos, además del cliente Codex real con dos identidades y cero turnos de modelo.

La [corrida de CI 40904080796](https://github.com/joalceco/mddiff/actions/runs/40904080796) aprobó `check` y `container`: 70 pruebas Rust, regresiones, coordinación y capacidad de diez usuarios sintéticos. Codex real también completó OAuth y el recorrido API/MCP por HTTPS público sobre la imagen publicada `6bfccea`, con dos cuentas y cero turnos de modelo. El recorrido verificó procedencia, SSE y conflictos de revisión. La recuperación del trabajo nuevo y sus metadatos también pasó; después se retiraron sus documentos y las tres cuentas sintéticas creadas, sin modificar al propietario.

Como antecedente, la base previa aprobó los jobs `check` y `container` de [CI 40779128838](https://github.com/joalceco/mddiff/actions/runs/40779128838). Esa corrida histórica no se atribuye a los runners ni al vínculo de sesión añadidos en S11. Ninguna de estas comprobaciones acredita login Google/GitHub ni una lectura real de Jira o Memos.
