# Jira: inventario privado y distribución pendiente

Fecha: 2026-11-05. Issue: [#17](https://github.com/joalceco/mddiff/issues/17).

La rama usa SQLite 6 y dispone de inventario privado, reporte 3LO, publicación de decisiones y aplicación de borrados con una barrera externa. Hay pruebas locales del binario real y de recuperación de un checkpoint anterior. **Este flujo todavía no está desplegado ni integrado con los respaldos y la operación periódica del cluster.** No se habilita Sharing ni se acepta su declaración. La beta publicada conserva el binario `6bfccea` y SQLite 4.

## Evidencia pública y alcance

La [guía vigente de privacidad de Jira](https://developer.atlassian.com/cloud/jira/platform/user-privacy-developer-guide/) exige reportar las cuentas cuyos datos personales almacena una app, conservar la antigüedad de la copia más antigua y atender las respuestas de actualización o borrado. Publica un endpoint específico para OAuth 3LO y un ciclo predeterminado de siete días, sujeto al encabezado del servicio.

Las [aclaraciones de personal de Atlassian sobre datos almacenados](https://community.developer.atlassian.com/t/storing-personal-data-in-user-properties-is-gdpr-compliant/24926) distinguen identificadores, contenido generado por usuarios y nombres de perfil asociados a una cuenta. Son respuestas históricas, de 2018–2019; no constituyen una exención general para cualquier información personal escrita en un documento. Este proyecto no intenta inferir cuentas Atlassian a partir de nombres o correos en texto libre.

El [contrato de menciones ADF](https://developer.atlassian.com/cloud/jira/platform/apis/document/nodes/mention/) permite identificadores de cuenta o nombres de colección. La mención también puede conservar un nombre visible. Por ello, guardar el fallback JSON no evita el tratamiento de datos personales. Un identificador ambiguo no acredita por sí solo un inventario completo.

La documentación de Atlassian no determina específicamente cómo debe aplicar mddiff el borrado a sus copias restic. La [guía del ICO sobre borrado en respaldos](https://ico.org.uk/for-organisations/uk-gdpr-guidance-and-resources/individual-rights/individual-rights/right-to-erasure/) contempla retirar los datos del uso mientras las copias vencen según una política de retención, con información clara para las personas afectadas. Es una referencia para diseñar el control de restauración; no demuestra por sí sola que mddiff ya lo aplique ni que Atlassian lo haya validado. No se ha enviado una consulta a soporte.

## Comportamiento implementado en la rama

`src/privacy.rs` recorre el ADF, incluidos subárboles que el conversor conserva como JSON. Recoge identificadores estructurados de menciones y campos `accountId`, con límites de profundidad y tamaño. No añade nombres, correos ni texto del documento al inventario. Los documentos originales conservan el contenido recibido y sus avisos de conversión.

La captura resuelve los identificadores válidos sin tipo mediante una [consulta acotada a Jira](jira-source.md#clasificación-de-referencias-de-cuenta), con la credencial personal. Usa solo el tipo de cuenta devuelto, sin enriquecer el ADF ni guardar perfiles. Las cuentas app y las colecciones declaradas quedan fuera del inventario de personas. Los tipos explícitos desconocidos y las cuentas que no se pueden resolver siguen sin clasificar; una captura nueva se rechaza conservando el trabajo anterior.

El 2026-11-05 se configuró y consintió `read:jira-user` con autorización del propietario. Pasaron una captura Jira real con la barrera activa en un lector local aislado y una consulta del endpoint de usuarios con la propia cuenta que consintió. El documento real no contenía cuentas estructuradas; la resolución de menciones ambiguas mantiene sus pruebas sintéticas. Se retiraron las copias y el callback temporales, y la credencial previa del operador renovó después sin reemplazarla. [Evidencia y límites](jira-source.md#validación-real-del-permiso-de-usuarios). El servicio publicado sigue en SQLite 4; estos resultados no activan el ciclo de privacidad ni Sharing.

La columna privada `current_work.jira_privacy` conserva inventarios separados para original y propuesta. No se expone en `/api/work`, `/api/state` ni MCP; una entrada de documento tampoco puede proporcionarla. Cada registro contiene el identificador y la fecha de lectura del proveedor, en segundos Unix. No utiliza la fecha de modificación del issue.

| Cambio del trabajo | Efecto en el inventario |
| --- | --- |
| Capturar otro origen | Sustituye el inventario del original; conserva el de la propuesta existente. |
| Revisar una propuesta mediante API o agente | Conserva referencias del original y de la propuesta anterior; para una misma cuenta utiliza la fecha más antigua. |
| Reemplazar explícitamente un lado con texto o archivo | Retira el inventario sustituido; el flujo de propuesta conserva las referencias del original actual. |
| Rechazar una revisión antigua o fallar el guardado | Conserva documentos, inventario y revisión anteriores. |
| Borrar el trabajo | Vacía ambos inventarios junto con el contenido vigente. No elimina copias de respaldo. |

La conservación de referencias en propuestas derivadas es deliberadamente conservadora: el agente puede copiar nombres del original o de una propuesta anterior. El inventario no pretende comprobar si cada nombre sigue apareciendo literalmente.

La migración 4→5 añade una columna, sin cambiar documentos, revisiones, conexiones o procedencia. Los documentos preexistentes quedan pendientes de clasificación: versiones anteriores podían haber sustituido la procedencia Jira sin borrar su propuesta. No se inventan identificadores ni fechas. El lookup de nuevas capturas no reclasifica trabajos ni respaldos antiguos. Los identificadores inválidos y `unknown` mantienen una marca pendiente; `unknown` no se incorpora a la lista de cuentas.

La migración 5→6 añade `privacy_guard`, una tabla de metadatos de operación sin Markdown. Registra la generación aplicada y que el registro externo es obligatorio. Una base que haya activado la barrera rechaza una apertura que omita su archivo. Los binarios anteriores al esquema 6 rechazan la base nueva. Antes de desplegar esta rama es necesario adaptar y probar respaldo, restauración y rollback; no se debe reducir `user_version` manualmente.

## Herramienta de reporte 3LO

`scripts/jira_privacy_report.py` procesa checkpoints privados sin un escritor sobre esas copias. La captura del proceso permite mantener abierto el lector, como se describe a continuación. `inventory` muestra únicamente cantidades; agrega la fecha más antigua de cada cuenta entre todos los originales, propuestas y checkpoints proporcionados. Deben incluirse la copia vigente y todas las copias retenidas. El comando no descubre snapshots restic ni demuestra que el operador haya incluido todos.

El inventario admite esquemas 3–6 y comprueba la integridad sin migrar ni escribir la copia. Las copias 3 y 4 no tienen inventario privado: cada documento presente cuenta como pendiente de clasificación, aunque existan datos remotos en otras columnas. Las filas vacías cuentan como cero documentos. Esto permite agregar respaldos antiguos vacíos; un solo documento sin clasificar en cualquier copia sigue bloqueando el reporte antes de abrir la credencial o enviar datos. Un esquema desconocido o una base inválida detienen el proceso.

`report` envía únicamente identificadores y fechas a `https://api.atlassian.com/app/report-accounts/`. Rechaza inventarios pendientes de clasificación. Obtiene el token de un JSON privado: conserva compatibilidad con el formato estático `{"access_token":"…"}` y admite la credencial renovable descrita a continuación. No acepta tokens como argumentos ni imprime identificadores. No utiliza proxies del entorno ni sigue redirecciones. El estado y su lock requieren una carpeta privada; las escrituras son atómicas, con permisos `0600` y sincronización a disco.

El reporte divide las cuentas vencidas en lotes de hasta 90. Guarda el avance por cuenta y conserva el ciclo al reiniciar. La [aclaración de Atlassian sobre `Cycle-Period`](https://community.developer.atlassian.com/t/cycle-period-header-example/26940/2) especifica días; el código mantiene el valor predeterminado vigente de siete días y usa segundos para `Retry-After`. Un `429` impide nuevos lotes hasta el plazo indicado. Un rechazo de credenciales permite reintentar solo los lotes que no habían sido aceptados.

Antes de enviar un lote, el estado registra que hay una respuesta pendiente. Una interrupción de transporte o una respuesta ambigua conserva esa marca y detiene nuevos envíos hasta reconciliar el resultado. No se pretende conseguir una entrega exactamente una vez mediante un endpoint sin clave de idempotencia. Un resultado `closed` o `updated` queda guardado como acción pendiente y detiene los lotes restantes. **Guardar la acción no elimina el contenido ni acredita su cumplimiento.**

El operador dispone de una credencial real renovable, probada contra Atlassian el 2026-11-05. Todavía no hay un timer de reporting instalado. La publicación de decisiones y el binario permiten aplicar acciones a checkpoints detenidos, como se describe a continuación. Aún falta activar el ciclo completo con los respaldos del cluster. Tampoco hay una orden automática para dar por resuelta una respuesta incierta del **reporte de cuentas**; no basta con ejecutar un POST ni con obtener `204`.

Para comprobar el protocolo sin credenciales ni servicios reales:

```sh
python3 scripts/test_jira_privacy_report.py
python3 scripts/test_jira_report_credential.py
```

La suite usa únicamente HTTP en loopback, bases temporales y tokens sintéticos. Las excepciones y respuestas del proveedor no imprimen documentos ni credenciales.

### Credencial renovable del operador

El formato versionado reúne `client_id`, `client_secret`, `access_token`, `refresh_token`, `expires_at` y el estado de la renovación. Se provisiona desde una autorización OAuth de la misma app con `offline_access`; no sirve un token de API de Jira ni un grant de otra app. Este es el esquema, con valores de sustitución:

```json
{
  "version": 1,
  "client_id": "REPLACE_WITH_APP_CLIENT_ID",
  "client_secret": "REPLACE_WITH_APP_CLIENT_SECRET",
  "access_token": "REPLACE_WITH_ACCESS_TOKEN",
  "refresh_token": "REPLACE_WITH_REFRESH_TOKEN",
  "expires_at": 1,
  "refresh_started_at": null,
  "retry_after": 0,
  "reauthorize": false
}
```

`expires_at` es la fecha de vencimiento del access token, en segundos Unix; el `1` del esquema representa un token vencido. El archivo requiere `0600` y su directorio `0700`, ambos propiedad del operador que ejecuta la herramienta. Debe quedar fuera de repositorios —incluidos directorios ignorados—, del montaje de la aplicación y de la restauración de documentos. No copies una credencial rotatoria que otro proceso o una conexión del visor siga renovando: el bloqueo protege un archivo, no dos copias del mismo grant. Si se pierde, obtén una autorización nueva; recuperar un refresh token antiguo desde un checkpoint puede reutilizar uno invalidado.

Para verificar o renovar la credencial sin abrir bases de documentos:

```sh
python3 scripts/jira_privacy_report.py renew-credential \
  --credential /srv/operator-private/jira.json
```

El comando devuelve solo `{"credential_ready":true}`. Reutiliza un access token con más de 60 segundos de vigencia o lo renueva contra `https://auth.atlassian.com/oauth/token`. `report` hace la misma comprobación antes de enviar cuentas. La renovación no solicita scopes adicionales.

La [documentación OAuth de Atlassian](https://developer.atlassian.com/cloud/jira/platform/oauth-2-3lo-apps/#use-a-refresh-token-to-get-another-access-token-and-refresh-token-pair) describe la rotación y un margen de reutilización de diez minutos para fallos de red o concurrencia. Antes de solicitar la renovación, la herramienta sincroniza a disco la hora del intento. Si la respuesta se pierde o es ambigua, conserva ese intento y permite recuperarlo durante **cinco minutos desde la primera solicitud**, dejando margen para tránsito y desfase de reloj. Los reintentos no amplían el plazo. Fuera de esa ventana exige una autorización nueva; no reinicies el marcador para reutilizar el token.

Una respuesta válida reemplaza y sincroniza ambos tokens antes de permitir el reporte. Si el access token ya venció durante el tránsito, conserva el refresh token nuevo para la siguiente renovación. `429` conserva `Retry-After` en segundos; `400`, `401` y `403` dejan la credencial bloqueada hasta obtener una nueva autorización. No se guardan cuerpos de error del proveedor. La recuperación de una renovación OAuth y la reconciliación de una respuesta incierta del reporte de cuentas son operaciones distintas.

### Validación real del protocolo

El 2026-11-05 se obtuvo una autorización nueva de la misma app con los permisos de lectura y renovación ya aprobados. El flujo mantuvo el mutex del respaldo desde antes del consentimiento hasta retirar la copia de la conexión del visor. Un exportador privado reutilizó el descifrado del SDK sobre una copia detenida de SQLite; la base en uso conservó su esquema. La retirada de la conexión verificó propietario, destino, generación y contenido cifrado antes de cambiar únicamente esa fila. Se conservaron los documentos y los contenedores del IdP.

La credencial del operador quedó en otro SSD, fuera de repositorios, del montaje de la aplicación y de la recuperación de documentos, con archivo `0600` y directorio `0700`. Después de retirar las copias de exportación y los usuarios sintéticos, se adelantó de forma conservadora el vencimiento local del access token para ejecutar una renovación real. El refresh token cambió y ambos tokens nuevos quedaron sincronizados a disco. No se ampliaron permisos.

El reporte usó exclusivamente las dos [cuentas de prueba publicadas por Atlassian](https://developer.atlassian.com/cloud/jira/platform/user-privacy-developer-guide/#testing). El endpoint real devolvió `200`; la acción `closed` de la cuenta de prueba cerrada quedó guardada en un estado aislado, sin respuestas pendientes de interpretación. La beta recuperó salud `204` por HTTPS y el timer del respaldo siguió activo, con su mutex libre.

Esta prueba verifica autenticación, rotación y respuesta real del protocolo. **No reportó el inventario de producción, no aplicó borrados a documentos reales y no instaló el reporting periódico.** El estado de prueba permanece separado del futuro estado de operación. Sharing sigue desactivado.

### Checkpoint del operador sin reiniciar

Un servidor hosted sobre Unix admite `--operator-checkpoint-dir CARPETA`. La carpeta debe existir con `0700`, pertenecer al mismo propietario que los datos y quedar fuera de estos y de repositorios Git. La opción no se admite en local ni junto con `--apply-jira-privacy`. Sin la opción, el servidor no instala el receptor de `SIGUSR1` para esta función.

Al recibir `SIGUSR1`, el proceso usa su conexión SQLite y su bloqueo de almacenamiento para crear una copia transaccional mediante [`VACUUM INTO`](https://www.sqlite.org/lang_vacuum.html). No modifica el trabajo, publica revisiones ni reinicia el proceso. Verifica la barrera antes y después de la captura, sincroniza la copia y la publica como `work.sqlite3` con `0600`, sin sobrescribir un archivo existente. Las sesiones y eventos continúan abiertos.

Con la opción ya habilitada en el contenedor, el operador puede usar `docker compose kill -s SIGUSR1 mddiff`. Especifica siempre la señal: el valor predeterminado de [ese comando Docker](https://docs.docker.com/reference/cli/docker/compose/kill/) es `SIGKILL`. No envíes `SIGUSR1` a un binario que no tenga habilitado este receptor.

El operador debe esperar `OPERATOR_CHECKPOINT_READY` en el log del proceso antes de consumir una captura nueva. `OPERATOR_CHECKPOINT_FAILED` indica que esa solicitud no quedó confirmada; la presencia de un archivo por sí sola no acredita éxito. Un checkpoint anterior, un destino inseguro o una barrera cambiada rechazan la solicitud sin reemplazar la copia anterior. Un archivo `.work.sqlite3.pending` tras una caída requiere inspección bajo el bloqueo del operador antes de retirarlo; no se debe tratar como una captura terminada.

Esta copia contiene datos de **todas las identidades** y conexiones cifradas, sin copiar la clave externa ni los JSON de acceso. No hay endpoint HTTP/MCP ni control de la carpeta desde documentos o peticiones de usuario. Incluye el checkpoint en el inventario y en la política de retención mientras exista; retíralo después de consumirlo. La copia no sustituye un respaldo de claves, configuración o IdP, ni permite dar por cumplidas acciones pendientes sobre otras copias.

## Barrera externa y borrado

`stage-erasure` requiere un único `--database` vigente, `--state` y `--journal`. Adquiere un bloqueo exclusivo de SQLite antes de publicar las decisiones; si el escritor está activo, falla sin cambiar el registro. Conserva las decisiones anteriores, no reduce su fecha y no convierte una cuenta cerrada en una simple actualización. Repetir la publicación de las mismas decisiones no cambia el archivo ni su generación.

`--create-journal` crea el registro vacío solo antes de la primera activación y antes de tener historial de reporting. No sirve para recuperar un archivo perdido. El registro y el estado deben quedar fuera de los datos y del repositorio. El registro se conserva independientemente de los checkpoints de documentos; no debe reemplazarse por una versión de un respaldo antiguo.

El binario usa `--jira-privacy-file` en modo hosted. Aplica las decisiones sobre todas las filas **antes de abrir el listener**. `--apply-jira-privacy`, junto con `--mode hosted`, `--data-dir` y el registro, realiza esa operación sin iniciar servidor ni cargar configuración OIDC o conexiones.

| Decisión y coincidencia | Resultado sobre el trabajo vigente |
| --- | --- |
| `closed` | Retira cualquier documento con referencias a esa cuenta, con independencia de cuándo se capturó. Impide guardar nuevas capturas estructuradas de esa cuenta. |
| `updated` | Retira copias cuya fecha de lectura sea anterior o igual a la recepción de la decisión. Permite una nueva captura posterior; no cambia automáticamente el original congelado. |
| Original afectado | **Borra original y propuesta completos.** El modelo no admite una propuesta sin original. También retira su procedencia e inventarios. |
| Solo propuesta afectada | Borra la propuesta y su inventario; conserva exactamente el original y su procedencia. |
| Sin coincidencia | Conserva documentos, revisión y conexiones. |

El borrado es deliberadamente por documento: una IA puede haber copiado el nombre fuera de una mención. No se promete localizar todas sus apariciones mediante reemplazos de texto. Antes de activar este flujo con trabajos reales debe quedar revisada esta política de pérdida de contenido; las pruebas actuales solo borran datos sintéticos.

Cada trabajo modificado aumenta su revisión y conserva su fila vacía cuando corresponde, para rechazar solicitudes anteriores. La aplicación no necesita que la persona mencionada tenga cuenta en mddiff. Las conexiones personales y las cuentas del IdP no cambian.

Las decisiones nuevas reservan rangos de revisión que no se solapan con el período anterior. Un borrado puede hacer saltar el contador: así, una propuesta emitida antes de ese borrado no vuelve a ser válida al recuperar un checkpoint antiguo. Los números siguen siendo enteros seguros para JavaScript; los clientes deben leer la revisión del servidor y tratarla como opaca. Los cambios ordinarios siguen aumentando en uno. Cada período permite hasta mil millones de escrituras después de su piso; al acercarse a ese límite se necesita una operación de avance de período, aún no implementada.

Si alguna fila carece de clasificación suficiente, se revierte la transacción completa de documentos. El registro externo sigue siendo obligatorio: no se permite omitirlo para volver a servir la base. Con la barrera activa también se rechazan nuevas entradas con inventario sin clasificar. La resolución de menciones sin tipo está implementada para nuevas capturas; no permite inventar una clasificación si el proveedor falla o devuelve un tipo desconocido.

La aplicación conserva la generación aplicada y rechaza un registro anterior. Si el archivo, sus permisos o su directorio cambian mientras el proceso está activo, las nuevas lecturas del trabajo, escrituras y comprobaciones de salud fallan hasta reiniciar con el registro válido. Publicar una decisión requiere mantenimiento y escritor detenido; no se admite como actualización en caliente de vistas abiertas.

Después de aplicar decisiones, el proceso trunca el WAL, ejecuta `VACUUM` y vuelve a truncar el WAL. Esto limpia los archivos SQLite del estado borrado; no constituye borrado forense de SSD ni elimina snapshots cifrados anteriores.

## Recuperación y montaje pendientes de publicar

El overlay [`deploy/compose.privacy.yaml`](../../deploy/compose.privacy.yaml) monta un **directorio** privado externo en `/run/mddiff-privacy`, de solo lectura para la aplicación. El directorio, propiedad del UID del contenedor, requiere `0700` y el registro `0600`. La publicación conserva UID/GID y asigna al registro el propietario de su directorio antes del reemplazo, para que un job root no lo deje inaccesible al UID `10001`. Montar el directorio permite el reemplazo atómico; montar solo el archivo conservaría un inode anterior. `MDDIFF_PRIVACY_DIR` debe conservarse fuera de la restauración de documentos y secretos.

El overlay también habilita el checkpoint operativo en `/run/mddiff-checkpoint`, con un montaje independiente de lectura/escritura desde `MDDIFF_CHECKPOINT_DIR`. Prepara esa carpeta como `10001:10001`, `0700`, separada de los datos y del registro de decisiones. El operador debe consumir la copia y esperar su confirmación sin reiniciar el servicio; este montaje temporal tampoco debe incorporarse sin control a un archivo de restauración.

El 2026-11-05 se probó una autoridad alojada fuera del invitado, montada de solo lectura a través de LXC y Docker, con UID mapeado. La sustitución atómica fue visible en un contenedor ya abierto. El binario real aplicó un borrado a datos sintéticos y el preparador filtró un checkpoint anterior, conservó el trabajo del otro propietario y rechazó la autoridad ausente. El clon temporal y su disco se retiraron. El servicio publicado no cambió de binario, esquema ni configuración durante ese ensayo.

El estado de reporting y la credencial del operador deben estar en **otro directorio privado**, fuera de ese montaje. El contenedor solo necesita leer el registro de decisiones, no el token del reporter.

Una copia protegida de SQLite 6 se vuelve a filtrar al abrirla con el registro vigente. La prueba del binario real recupera un checkpoint previo al borrado y comprueba que no reaparecen los documentos ni aumenta otra vez la revisión tras repetir el proceso.

[`scripts/jira_privacy_recovery.py`](../../scripts/jira_privacy_recovery.py) prepara una copia de recuperación antes de sustituir los datos en uso. Los helpers privados de recuperación completa y rollback de la aplicación usan el mismo preparador. Verifica el registro externo vigente y su montaje de directorio de solo lectura, filtra la copia con la CLI y comprueba esquema, integridad, generación y piso de revisión. Si el registro cambia durante el proceso, falla sin sustituir el estado en uso.

Los checkpoints de esquema 3–5 se migran con la imagen compatible de esquema 6 ya instalada; su imagen y configuración antiguas no se vuelven a servir. Un checkpoint de esquema 6 puede seleccionar su propia imagen compatible, conservando el argumento y montaje de la barrera vigentes. Si el servicio usa el receptor de checkpoints, la imagen elegida también debe admitirlo: un checkpoint anterior sin esa capacidad conserva la imagen actual compatible en lugar de arrancar un binario que rechazaría la nueva opción. Una imagen anterior que sí admite el protocolo conserva el rollback de código. Los documentos antiguos sin inventario suficiente siguen bloqueando la recuperación; no se descartan para hacerla pasar.

La imagen anuncia `io.mddiff.operator-checkpoint=1`; el operador valida esa etiqueta sobre la imagen inmutable y conserva `checkpoint_protocol: 1` en sus metadatos de recuperación. El preparador exige el protocolo actual y un montaje de checkpoint independiente de lectura/escritura cuando el argumento está habilitado. No debe inferirse esa capacidad únicamente del número de esquema SQLite. Los helpers de esa validación se instalaron el 2026-11-05 y pasaron self-checks y un respaldo completo con siete copias verificadas. Admiten que Compose herede el comando de la imagen mediante `command: null`. El lector publicado mantiene su binario y esquema anteriores; instalar los helpers no activa la captura ni la barrera.

El operador debe verificar el archivo de respaldo, la imagen y la compatibilidad de la identidad y la clave de conexiones **antes** de llamar al preparador. Los helpers privados conservan la configuración OAuth vigente y el código actual de recuperación, para que un respaldo antiguo no los reemplace. El registro permanece fuera de los directorios restaurados. En un host nuevo se requiere instalar primero una versión compatible y recuperar el registro autorizado por separado; la herramienta no inventa decisiones perdidas.

El 2026-11-04 se instalaron los helpers de operación después de tres recuperaciones completas en frío en un clon aislado. Se verificaron IdP, claves de firma, conexiones cifradas con proveedor sintético, trabajo/procedencia y conservación de decisiones y código vigentes. Se corrigió la espera de PostgreSQL para evitar su servidor temporal de inicialización. El clon se retiró y pasó el respaldo completo con siete copias y sin bloqueos de repositorio. Estos ensayos usaron loopback; **la beta sigue en SQLite 4, con la barrera y el reporting periódico sin activar**. La barrera tampoco sustituye el bloqueo de mantenimiento ni el bloqueo del operador que coordina respaldos y restauraciones.

Los snapshots cifrados anteriores no se reescriben ni se eliminan por publicar el registro. El borrado de un checkpoint de prueba tampoco acredita que se hayan tratado todas las copias retenidas. Por eso `stage-erasure` y el binario **no vacían las acciones pendientes** del reporter: falta verificar el conjunto de copias y los controles de restauración antes de confirmar cumplimiento.

Para probar el flujo con bases y decisiones sintéticas, después de compilar release:

```sh
python3 scripts/test_jira_erasure.py
python3 scripts/test_jira_privacy_recovery.py
```

El CI de contenedor ejecuta además `scripts/test_privacy_container.py` como operador root, usando solo datos sintéticos. Comprueba lectura como UID `10001`, publicación atómica desde root sin perder propietario/permisos, lectura del registro nuevo y rechazo de escritura sobre el montaje. No requiere credenciales de Atlassian.

## Trabajo necesario antes de Sharing

1. Agregar el inventario de todas las copias conservadas, incluidas las de respaldo, y resolver los documentos pendientes de clasificación.
2. Integrar la herramienta de reporte 3LO con renovación de credenciales, planificación periódica y monitoreo.
3. Integrar los borrados y la barrera probados con el conjunto real de respaldos y restauraciones, y verificar su aplicación antes de vaciar acciones pendientes.
4. Probar la recuperación y alinear la información pública de privacidad con el comportamiento verificado.
5. Cuando la declaración sea verdadera, completar la autorización de distribución y comprobar una conexión con otra cuenta Atlassian.

Google/GitHub permanece fuera de esta entrega por decisión del usuario. El acceso de lectura Jira ya verificado con la cuenta propietaria no demuestra distribución a invitados.

## Verificación

Las pruebas Rust cubren extracción bajo nodos desconocidos, identificadores ambiguos, límites, copia de referencias a propuestas, fecha más antigua, cambios de origen, rechazo de revisiones antiguas, aislamiento, reinicio, borrado y migración 4→5. `scripts/test_jira_source.py` verifica además la captura real del conector contra proveedores sintéticos, el inventario persistido por propietario y su ausencia en las respuestas públicas del usuario.

Las pruebas del reporte cubren lotes, ciclos por cuenta, antigüedad agregada, reinicio tras éxito parcial, `429`, respuestas de actualización/borrado, interrupciones, respuestas inválidas, redirecciones, permisos y exclusión mutua. El [CI del reporte `27f2037`](https://github.com/joalceco/mddiff/actions/runs/41899586497) terminó con código y contenedor aprobados, incluida capacidad de diez usuarios sintéticos.

Las pruebas de la barrera cubren varios propietarios, borrado de original o solo propuesta, actualización frente a captura posterior, conservación de conexiones, idempotencia, limpieza SQLite/WAL, rollback de la transacción ante inventario incompleto y rechazo de registro ausente, antiguo, enlazado, público o inválido. La CLI real valida publicación→borrado→recuperación filtrada; en la comprobación local también se ejecutó un binario anterior, que rechazó SQLite 6.

Las pruebas del checkpoint cubren copia de todas las identidades, inventario privado y conexiones cifradas, contenido Unicode/CRLF, permisos, rechazo de destinos inseguros y conservación de una copia existente. La prueba de persistencia usa señales sobre el proceso real con dos usuarios Keycloak y comprueba que las sesiones, SSE y revisiones continúan. La prueba de contenedor verifica UID `10001`, `init`, raíz de solo lectura y confirmaciones de captura sin cambiar el PID. La recuperación distingue imágenes con y sin el protocolo, sin modificar el estado en uso ni confirmar acciones de otros respaldos.

Las 13 pruebas de renovación cubren persistencia de tokens, recuperación de una respuesta perdida dentro del plazo, rechazo de intentos antiguos, límites de respuesta, permisos, locks y compatibilidad con credenciales estáticas. El [CI de `2e0943c`](https://github.com/joalceco/mddiff/actions/runs/42045971311) aprobó código y contenedor.

La validación real descrita arriba acredita el endpoint de Atlassian con sus cuentas de prueba. El inventario de producción, la aplicación de acciones al conjunto de respaldos y Sharing siguen pendientes; el issue permanece abierto.
