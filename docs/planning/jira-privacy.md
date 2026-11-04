# Jira: inventario privado y distribución pendiente

Fecha: 2026-11-04. Issue: [#17](https://github.com/joalceco/mddiff/issues/17).

La rama usa SQLite 6 y dispone de inventario privado, reporte 3LO, publicación de decisiones y aplicación de borrados con una barrera externa. Hay pruebas locales del binario real y de recuperación de un checkpoint anterior. **Este flujo todavía no está desplegado ni integrado con los respaldos y la operación periódica del cluster.** No se habilita Sharing ni se acepta su declaración. La beta publicada conserva el binario `6bfccea` y SQLite 4.

## Evidencia pública y alcance

La [guía vigente de privacidad de Jira](https://developer.atlassian.com/cloud/jira/platform/user-privacy-developer-guide/) exige reportar las cuentas cuyos datos personales almacena una app, conservar la antigüedad de la copia más antigua y atender las respuestas de actualización o borrado. Publica un endpoint específico para OAuth 3LO y un ciclo predeterminado de siete días, sujeto al encabezado del servicio.

Las [aclaraciones de personal de Atlassian sobre datos almacenados](https://community.developer.atlassian.com/t/storing-personal-data-in-user-properties-is-gdpr-compliant/24926) distinguen identificadores, contenido generado por usuarios y nombres de perfil asociados a una cuenta. Son respuestas históricas, de 2018–2019; no constituyen una exención general para cualquier información personal escrita en un documento. Este proyecto no intenta inferir cuentas Atlassian a partir de nombres o correos en texto libre.

El [contrato de menciones ADF](https://developer.atlassian.com/cloud/jira/platform/apis/document/nodes/mention/) permite identificadores de cuenta o nombres de colección. La mención también puede conservar un nombre visible. Por ello, guardar el fallback JSON no evita el tratamiento de datos personales. Un identificador ambiguo no acredita por sí solo un inventario completo.

La documentación de Atlassian no determina específicamente cómo debe aplicar mddiff el borrado a sus copias restic. La [guía del ICO sobre borrado en respaldos](https://ico.org.uk/for-organisations/uk-gdpr-guidance-and-resources/individual-rights/individual-rights/right-to-erasure/) contempla retirar los datos del uso mientras las copias vencen según una política de retención, con información clara para las personas afectadas. Es una referencia para diseñar el control de restauración; no demuestra por sí sola que mddiff ya lo aplique ni que Atlassian lo haya validado. No se ha enviado una consulta a soporte.

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

La migración 5→6 añade `privacy_guard`, una tabla de metadatos de operación sin Markdown. Registra la generación aplicada y que el registro externo es obligatorio. Una base que haya activado la barrera rechaza una apertura que omita su archivo. Los binarios anteriores al esquema 6 rechazan la base nueva. Antes de desplegar esta rama es necesario adaptar y probar respaldo, restauración y rollback; no se debe reducir `user_version` manualmente.

## Herramienta de reporte 3LO

`scripts/jira_privacy_report.py` procesa checkpoints privados con el escritor detenido. `inventory` muestra únicamente cantidades; agrega la fecha más antigua de cada cuenta entre todos los originales, propuestas y checkpoints proporcionados. Deben incluirse la copia vigente y todas las copias retenidas. El comando no descubre snapshots restic ni demuestra que el operador haya incluido todos.

`report` envía únicamente identificadores y fechas a `https://api.atlassian.com/app/report-accounts/`. Rechaza inventarios pendientes de clasificación. Obtiene el token de un JSON privado con la clave `access_token`, sin argumentos de credenciales ni salida de identificadores. No utiliza proxies del entorno ni sigue redirecciones. El estado y su lock requieren una carpeta privada; las escrituras son atómicas, con permisos `0600` y sincronización a disco.

El reporte divide las cuentas vencidas en lotes de hasta 90. Guarda el avance por cuenta y conserva el ciclo al reiniciar. La [aclaración de Atlassian sobre `Cycle-Period`](https://community.developer.atlassian.com/t/cycle-period-header-example/26940/2) especifica días; el código mantiene el valor predeterminado vigente de siete días y usa segundos para `Retry-After`. Un `429` impide nuevos lotes hasta el plazo indicado. Un rechazo de credenciales permite reintentar solo los lotes que no habían sido aceptados.

Antes de enviar un lote, el estado registra que hay una respuesta pendiente. Una interrupción de transporte o una respuesta ambigua conserva esa marca y detiene nuevos envíos hasta reconciliar el resultado. No se pretende conseguir una entrega exactamente una vez mediante un endpoint sin clave de idempotencia. Un resultado `closed` o `updated` queda guardado como acción pendiente y detiene los lotes restantes. **Guardar la acción no elimina el contenido ni acredita su cumplimiento.**

La herramienta todavía no renueva el token ni tiene timer instalado. La publicación de decisiones y el binario permiten aplicar acciones a checkpoints detenidos, como se describe a continuación. Aún falta integrar el ciclo completo con los respaldos del cluster. Tampoco hay una orden automática para dar por resuelta una respuesta incierta; no basta con ejecutar un POST ni con obtener `204`.

Para comprobar el protocolo sin credenciales ni servicios reales:

```sh
python3 scripts/test_jira_privacy_report.py
```

La suite usa únicamente HTTP en loopback, bases temporales y tokens sintéticos. Las excepciones y respuestas del proveedor no imprimen documentos ni credenciales.

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

Si alguna fila carece de clasificación suficiente, se revierte la transacción completa de documentos. El registro externo sigue siendo obligatorio: no se permite omitirlo para volver a servir la base. Con la barrera activa también se rechazan nuevas entradas con inventario sin clasificar. Esto incluye menciones ambiguas cuyo tipo no puede establecerse todavía; su resolución es necesaria antes de dar por terminada la integración.

La aplicación conserva la generación aplicada y rechaza un registro anterior. Si el archivo, sus permisos o su directorio cambian mientras el proceso está activo, las nuevas lecturas del trabajo, escrituras y comprobaciones de salud fallan hasta reiniciar con el registro válido. Publicar una decisión requiere mantenimiento y escritor detenido; no se admite como actualización en caliente de vistas abiertas.

Después de aplicar decisiones, el proceso trunca el WAL, ejecuta `VACUUM` y vuelve a truncar el WAL. Esto limpia los archivos SQLite del estado borrado; no constituye borrado forense de SSD ni elimina snapshots cifrados anteriores.

## Recuperación y montaje pendientes de publicar

El overlay [`deploy/compose.privacy.yaml`](../../deploy/compose.privacy.yaml) monta un **directorio** privado externo en `/run/mddiff-privacy`, de solo lectura para la aplicación. El directorio, propiedad del UID del contenedor, requiere `0700` y el registro `0600`. La publicación conserva UID/GID y asigna al registro el propietario de su directorio antes del reemplazo, para que un job root no lo deje inaccesible al UID `10001`. Montar el directorio permite el reemplazo atómico; montar solo el archivo conservaría un inode anterior. `MDDIFF_PRIVACY_DIR` debe conservarse fuera de la restauración de documentos y secretos.

El estado de reporting y la credencial del operador deben estar en **otro directorio privado**, fuera de ese montaje. El contenedor solo necesita leer el registro de decisiones, no el token del reporter.

Una copia protegida de SQLite 6 se vuelve a filtrar al abrirla con el registro vigente. La prueba del binario real recupera un checkpoint previo al borrado y comprueba que no reaparecen los documentos ni aumenta otra vez la revisión tras repetir el proceso.

[`scripts/jira_privacy_recovery.py`](../../scripts/jira_privacy_recovery.py) prepara una copia de recuperación antes de sustituir los datos en uso. Los helpers privados de recuperación completa y rollback de la aplicación usan el mismo preparador. Verifica el registro externo vigente y su montaje de directorio de solo lectura, filtra la copia con la CLI y comprueba esquema, integridad, generación y piso de revisión. Si el registro cambia durante el proceso, falla sin sustituir el estado en uso.

Los checkpoints de esquema 3–5 se migran con la imagen compatible de esquema 6 ya instalada; su imagen y configuración antiguas no se vuelven a servir. Un checkpoint de esquema 6 puede seleccionar su propia imagen compatible, conservando el argumento y montaje de la barrera vigentes. Los documentos antiguos sin inventario suficiente siguen bloqueando la recuperación; no se descartan para hacerla pasar.

El operador debe verificar el archivo de respaldo, la imagen y la compatibilidad de la identidad y la clave de conexiones **antes** de llamar al preparador. Los helpers privados conservan la configuración OAuth vigente y el código actual de recuperación, para que un respaldo antiguo no los reemplace. El registro permanece fuera de los directorios restaurados. En un host nuevo se requiere instalar primero una versión compatible y recuperar el registro autorizado por separado; la herramienta no inventa decisiones perdidas.

**Los helpers instalados en el cluster aún no aplican esta política.** La preparación y las pruebas locales no acreditan su despliegue ni la recuperación completa de restic, IdP, secretos y configuración. La barrera tampoco sustituye el bloqueo de mantenimiento ni el bloqueo del operador que coordina respaldos y restauraciones.

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

Estas pruebas no acreditan el endpoint real de reporting de Atlassian, borrado de respaldos ni Sharing. Esos pasos siguen pendientes y el issue permanece abierto.
