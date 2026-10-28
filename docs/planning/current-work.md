# S04: conservar únicamente el trabajo actual

Fecha: 2026-10-27. Issue: [#5](https://github.com/joalceco/mddiff/issues/5). Rama: `feature/multiuser-sources`.

Este documento registra S04 y su API con sesión web. [S06: agentes remotos](remote-agents.md) añade acceso Bearer, `PATCH /api/work` y MCP al mismo trabajo persistente. [S08: captura de Memos](memos-source.md) añade procedencia en el esquema 3.

[S12: contenedor alojado](container-deployment.md) amplía la operación mediante un bind explícito con OIDC, volumen y secretos externos. Las referencias al listener limitado a loopback corresponden a la entrega S04; los límites de borrado y retención siguen vigentes.

## Resultado y alcance

El modo hosted conserva en SQLite el último original y su propuesta para cada identidad verificada. El trabajo sobrevive al cierre del navegador y al reinicio del proceso. Sustituirlo reemplaza el contenido vigente; no crea una biblioteca ni versiones recuperables.

La propiedad del trabajo se determina mediante `issuer` + `sub`, verificados por la [autenticación web](hosted-auth.md). No se acepta un propietario elegido en el cuerpo, los headers o la query de la solicitud. El modo local continúa en memoria y mantiene sus contratos de archivos y MCP.

Las sesiones web siguen siendo temporales: después de reiniciar mddiff hay que autenticarse de nuevo para recuperar el trabajo. La importación desde la interfaz pertenece al issue #6; el MCP alojado, al #7. Google/GitHub y el despliegue público siguen pendientes.

La historia se verificó el 2026-10-27 con una base temporal y usuarios sintéticos de Keycloak 26.8.1. La persistencia cubre reinicios del proceso; no constituye un backup del volumen.

## Preparar el almacenamiento

El modo hosted requiere `--data-dir`. Usa un directorio privado **fuera del repositorio**, aunque los documentos sean de prueba. La aplicación crea la carpeta final si falta; su carpeta padre debe existir.

1. Configura el archivo OIDC privado siguiendo [la guía de Keycloak](hosted-auth.md#inicio-y-configuración-privada).
2. Crea la carpeta padre del directorio de datos:

   ```sh
   mkdir -p "$HOME/.local/share"
   ```

3. Inicia la aplicación con ambas ubicaciones:

   ```sh
   cargo run --release --locked -- --mode hosted --port 3848 \
     --oidc-config "$HOME/.config/mddiff/oidc.json" \
     --data-dir "$HOME/.local/share/mddiff"
   ```

4. Entra desde el navegador con una cuenta admitida. Tras un reinicio, utiliza la misma identidad para recuperar su trabajo vigente.

El proceso sigue escuchando en loopback; `--data-dir` no configura TLS, ingress, volumen del cluster ni exposición pública. La configuración OIDC de producción requiere HTTPS; la excepción HTTP se reserva para la fixture con ambos orígenes de loopback.

| Recurso | Requisito |
| --- | --- |
| Directorio de datos | Carpeta normal, sin symlink; se crea con permisos `0700` en Unix. Una carpeta existente no debe conceder acceso a grupo u otros. |
| `work.sqlite3` | Archivo normal privado, creado con permisos `0600` en Unix. |
| WAL, SHM y journal | Los archivos auxiliares de SQLite también deben ser normales y privados. No los publiques ni adjuntes a issues. |
| Proceso | Una sola instancia de mddiff por base; la conexión utiliza bloqueo exclusivo. |
| Modo local | Rechaza `--data-dir`; no empieza a guardar sus documentos por activar esta historia. |

Utiliza almacenamiento local al host. No ubiques esta base WAL en NFS u otro sistema de archivos de red; SQLite documenta que ese modo depende de memoria compartida y no funciona sobre un filesystem de red. Véase [Write-Ahead Logging de SQLite](https://sqlite.org/wal.html).

Los permisos son control de acceso del sistema de archivos, **no cifrado**: el Markdown se almacena en la base en claro. Los administradores del equipo o del volumen pueden acceder a esos datos. La protección y recuperación del volumen de producción se definen en la historia de despliegue.

## Modelo de datos y confirmación de escrituras

La base utiliza [rusqlite 0.41](https://docs.rs/rusqlite/0.41.0/rusqlite/) con SQLite incorporado y una tabla `current_work` sin `ROWID`. Su clave primaria es `(issuer, subject)`; hay como máximo una fila por identidad que haya guardado o borrado un trabajo. S04 creó el esquema 1; S07 añadió conexiones cifradas en una tabla separada y S08 incorpora `base_source` para la procedencia del original en el esquema 3.

La fila contiene la revisión y cuatro campos de contenido: `base_label`, `base_markdown`, `candidate_label` y `candidate_markdown`. Puede conservar un original sin propuesta. No se guardan versiones anteriores, HTML renderizado o diffs históricos.

El almacenamiento utiliza WAL, `synchronous=FULL` y `secure_delete=ON`. Una mutación valida y prepara el resultado, comprueba su revisión y confirma la transacción durable **antes** de sustituir la caché en memoria o emitir una revisión SSE. Un fallo de almacenamiento devuelve `503` y conserva el último estado publicado; no anuncia un resultado que no pudo guardar.

La recuperación es por identidad y bajo demanda. Al acceder por primera vez tras reiniciar, se lee el contenido vigente y se recalculan HTML saneado y diff. Se conservan los strings Markdown, incluidos Unicode y saltos CRLF; una representación JSON escapada no cambia su contenido al decodificarla.

### Reemplazo y borrado

Cada mutación correcta incrementa una revisión monotónica. Tanto reemplazar como borrar requieren `expected_revision`, obtenido al leer el trabajo vigente. Un valor obsoleto produce `409` sin modificar el trabajo.

Borrar pone los cuatro campos de contenido en `NULL`, pero **conserva la identidad y aumenta su contador**. Esa fila vacía evita que una petición anterior vuelva a ser válida después de borrar y recrear el trabajo. No contiene el Markdown borrado ni representa una versión recuperable.

**Borrar trabajo**, en la barra de revisión, solicita una confirmación nativa que se puede cancelar. Después de confirmar el borrado durable, el evento de revisión hace que las pestañas conectadas de la misma persona vacíen su vista. No se alteran otros espacios ni se elimina la cuenta del proveedor de identidad.

## API del trabajo actual

`/api/work` existe únicamente en hosted. Requiere una sesión web verificada. Los métodos que cambian estado exigen Origin exacto, el `X-CSRF-Token` de esa sesión y `Content-Type: application/json`.

| Método | Entrada | Respuesta correcta |
| --- | --- | --- |
| `GET /api/work` | Sin cuerpo. | `{revision, base, candidate}` con el Markdown vigente, o ambos lados `null` si está vacío. |
| `PUT /api/work` | `expected_revision`, `base` y `candidate` opcional o `null`. | Snapshot renderizado con el mismo formato de `/api/state`. |
| `DELETE /api/work` | JSON con `expected_revision`. | Snapshot vacío con una revisión nueva. |

Un documento es `{"label":"original.md","markdown":"# Texto\n"}`. `label` es una etiqueta de presentación: nunca se abre como ruta del host. El original es obligatorio en `PUT`; una propuesta sin original no es un trabajo válido. Los esquemas rechazan campos adicionales.

### Ejemplo de revisión esperada

Estos números corresponden a un espacio nuevo. Un cliente real debe obtener siempre la revisión mediante `GET /api/work`, en lugar de asumir que empieza de nuevo al abrir sesión.

Respuesta inicial de lectura:

```json
{
  "revision": 0,
  "base": null,
  "candidate": null
}
```

Cuerpo de un reemplazo:

```json
{
  "expected_revision": 0,
  "base": {"label": "original.md", "markdown": "# Plan\n\nRevisar el documento.\n"},
  "candidate": {"label": "propuesta.md", "markdown": "# Plan\n\nRevisar el documento y su contexto.\n"}
}
```

Después del reemplazo correcto, el snapshot y la siguiente lectura tienen revisión `1`. Para borrar ese trabajo, el cuerpo de `DELETE` es:

```json
{
  "expected_revision": 1
}
```

La siguiente lectura devuelve revisión `2`, `base: null` y `candidate: null`. Reutilizar `expected_revision: 0` o `1` obtiene `409`; borrar no restablece el contador.

En una página del mismo origen, el navegador aporta Origin y la cookie de sesión. El cliente obtiene el token CSRF desde `/auth/session` y lo envía en `X-CSRF-Token`. No copies cookies ni tokens en issues, ejemplos públicos o logs.

### Límites y errores

Cada Markdown admite hasta **2 MiB en UTF-8**. Las etiquetas admiten de 1 a **255 bytes UTF-8**, sin caracteres de control ni un valor compuesto únicamente por espacios. Estos límites se comprueban después de decodificar JSON.

El cuerpo de `/api/work` admite hasta **24 MiB + 16 KiB**, para incluir dos documentos cuyo contenido necesite escaparse en JSON. Este margen de transporte no aumenta el límite de 2 MiB por documento. Las rutas locales y el resto de endpoints conservan sus propios límites.

| HTTP | Acción del cliente |
| --- | --- |
| `400`, `413` o `422` | Corrige etiquetas, tamaño o contrato JSON; el trabajo anterior se conserva. |
| `401` o `403` | Revisa sesión, admisión, Origin y CSRF; no cambies de propietario mediante campos de la solicitud. |
| `409` | Vuelve a leer `/api/work` y decide si reemplazas o descartas tu propuesta. No sustituyas automáticamente la revisión esperada para forzar la escritura. |
| `503` | Conserva el contenido de la solicitud y resuelve la indisponibilidad antes de reintentarlo. El servidor mantiene el último estado publicado. |

Si se pierde la respuesta o el cliente se desconecta durante una escritura, consulta `/api/work` antes de repetirla. El servidor puede haber confirmado la transacción y publicado una revisión aunque el cliente no recibiera la respuesta.

## Operación y recuperación de errores

| Situación | Qué hacer |
| --- | --- |
| El directorio padre no existe | Créalo antes de iniciar. mddiff crea únicamente la carpeta final de datos. |
| Permisos o tipo de archivo rechazados | Verifica propietario, directorio `0700` y archivos `0600`. Utiliza archivos y directorios normales; no amplíes permisos para evitar el error. |
| Otra instancia usa la misma base | Detén la instancia duplicada. No escales varias réplicas contra este directorio ni fuerces la eliminación de locks. |
| Escritura no disponible o disco lleno | Comprueba espacio y permisos del volumen. Recupera el almacenamiento y relee el estado antes de repetir una mutación. |
| Base dañada o esquema incompatible | Conserva el directorio y detén las escrituras. Usa un binario compatible y un procedimiento de recuperación revisado; no borres la base para hacer arrancar el proceso. |

No elimines WAL/SHM ni copies únicamente `work.sqlite3` mientras una instancia está abierta: los archivos auxiliares pueden formar parte del estado confirmado. Véase [el archivo WAL como parte del estado persistente](https://sqlite.org/wal.html#the_wal_file). Esta historia no implementa un mecanismo de backup ni una reparación automática de bases.

Una base con `user_version > 3` se rechaza al iniciar. No hay downgrade automático; no reduzcas manualmente ese número para abrirla con un binario anterior. Un binario anterior a S08 rechaza el esquema 3: conserva una copia coherente antes de actualizar si necesitas volver a ese binario.

## Borrado funcional, copias y retención

La aplicación conserva un único trabajo recuperable y permite borrar su contenido vigente. Esto es **borrado funcional**, no una garantía de eliminación forense de todos los bytes anteriores.

[`secure_delete=ON`](https://sqlite.org/pragma.html#pragma_secure_delete) mitiga contenido residual en páginas de las tablas ordinarias de la base. Aun así, WAL, backups ajenos a la aplicación, snapshots del volumen y características del SSD pueden conservar información previa. Eliminar una fila o un archivo no demuestra que esas copias desaparezcan.

El binario no crea backups automáticos ni fija una retención por omisión. La instalación [S14: beta por invitación](beta-operations.md) configura hasta siete copias diarias cifradas, con respaldo coherente del trabajo, IdP, configuraciones y claves. Una copia puede recuperar versiones reemplazadas o borradas después de su captura. La política de esa instalación no convierte el trabajo actual en una biblioteca ni cambia el comportamiento del modo local.

Los ejemplos de este documento son sintéticos. No se deben utilizar documentos reales como fixtures públicas.

## Verificación

Corrida local del 2026-10-27: **35 pruebas Rust**, formato, Clippy sin advertencias y compilación `release` correctos. Las integraciones del MCP local, separación de modos y autenticación Keycloak también pasaron.

Con la [fixture sintética](hosted-auth.md#comprobar-con-la-fixture-sintética) iniciada y el binario compilado, ejecuta:

```sh
python3 scripts/test_hosted_persistence.py
```

El arnés utiliza cuatro procesos de la aplicación con la misma base temporal y comprueba:

| Caso | Resultado |
| --- | --- |
| Dos usuarios | Original y propuesta exactos por propietario, incluidos Unicode y CRLF; SSE propio y ausencia de eventos ajenos. |
| Reemplazo y relogin | Solo se recupera el último par; cerrar sesión y volver a entrar conserva el contenido. |
| Reinicio real | Dos terminaciones con `SIGKILL` conservan el último trabajo confirmado y, después de borrar, el espacio vacío y su revisión. |
| Error de almacenamiento | Un trigger sintético hace fallar reemplazo y borrado: ambos devuelven `503`, sin cambiar snapshot, datos ni eventos. El trigger se crea y retira con la aplicación detenida. |
| Borrado y conflictos | El borrado no afecta a la otra persona; revisiones antiguas obtienen `409` antes y después de reiniciar. Recrear el trabajo usa la siguiente revisión. |
| Esquema y límites | Una fila por emisor/sujeto, sin tabla de historial; rechazo de documentos excesivos, etiquetas inválidas, JSON incorrecto y selección de propietario por el cliente. |

Las pruebas Rust añaden dos operaciones que compiten por la misma revisión, continuación del commit tras desconectar al solicitante, permisos privados, rechazo de una segunda instancia y de enlaces simbólicos, y rechazo de una versión de esquema futura sin reducirla.

En Chrome se verificaron el lector local, login real por Keycloak, dos pestañas del mismo usuario y cancelación/confirmación del borrado. Confirmarlo vacía filas y etiquetas en ambas pestañas; cancelar conserva el par. Se inspeccionó la vista móvil de 390 px y se corrigió el contador de cambios para mantenerlo en una línea. No hubo errores JavaScript ni desbordamiento horizontal. Las capturas y el arnés visual quedan ignorados en `target/`.

Una revisión independiente de transacciones, caché, identidad, revisiones y cancelación no encontró fallos bloqueantes. GitHub Actions queda configurado para ejecutar los dos runners alojados con una fixture sintética y retirarla al terminar; estos resultados corresponden a la ejecución local, no a una corrida remota de CI.
