# S08: capturar una nota de Memos como original

Fecha: 2026-10-28. Issue: [#9](https://github.com/joalceco/mddiff/issues/9). Rama: `feature/multiuser-sources`.

## Resultado y alcance

Una persona autenticada puede abrir una nota por ID o URL desde **Abrir Memos**, en la cabecera de **Original**. Elige una conexión personal de Memos ya configurada y conectada. La captura reemplaza únicamente el original y conserva la propuesta actual; la comparación se actualiza mediante el mismo canal privado de revisiones del visor.

Se conserva el Markdown recibido, sin convertirlo a texto plano, junto con la procedencia. La nota queda capturada en el último trabajo de la persona: no se sincroniza automáticamente. Para obtener cambios posteriores de Memos, hay que abrirla otra vez. Desconectar Memos conserva la captura ya guardada.

El contrato soportado es **Memos 0.31.0**, con `GET /api/v1/memos/{id}` y una respuesta `Memo` directa. Se verificaron la [definición de GetMemo de esa versión](https://github.com/usememos/memos/blob/v0.31.0/proto/api/v1/memo_service.proto) y su [ruta web `/memos/:uid`](https://github.com/usememos/memos/blob/v0.31.0/web/src/router/index.tsx). No se detecta ni se intenta automáticamente la API 26.10.

No se importan adjuntos, comentarios, relaciones o notas enlazadas. El contenido se procesa con el saneado del lector existente; no se ejecutan instrucciones incluidas en el Markdown ni se cargan sus imágenes. Jira como original pertenece a #10; el selector general de orígenes, a #11. Google/GitHub y el despliegue público siguen pendientes.

## Abrir una nota

1. Entra con tu cuenta y configura una conexión desde **Conexiones**, siguiendo [la guía de conexiones personales](personal-connections.md).
2. En la cabecera de **Original**, pulsa **Abrir Memos**.
3. Selecciona la instancia conectada e introduce el ID, el recurso `memos/ID` o una URL de nota de esa misma instancia.
4. Pulsa **Abrir**. Espera la captura; el original y su procedencia aparecen en el visor y la propuesta se conserva.
5. Revisa la comparación o envía la propuesta desde la interfaz, API o MCP como antes.

El diálogo toma la revisión del trabajo al abrirse. Si el trabajo cambia, devuelve conflicto `409` y no reintenta con una revisión nueva. Conserva la referencia introducida; cierra y vuelve a abrir el diálogo después de revisar el trabajo vigente para preparar un nuevo intento.

Los borradores de texto pendientes deben guardarse o cerrarse antes de iniciar la captura. Cerrar el diálogo, terminar sesión o cambiar de cuenta limpia sus referencias y descarta respuestas anteriores en la interfaz.

### Referencias permitidas

Para una instancia administrativa `https://notes.example`, estas entradas identifican la misma nota:

```text
note-123
memos/note-123
https://notes.example/memos/note-123
```

El identificador admite entre 1 y 128 caracteres ASCII: letras, dígitos, guion y guion bajo. El ID reservado `-` no se admite. Se rechazan URLs de otra instancia, credenciales en la URL, parámetros, fragmentos, escapes porcentuales, barras invertidas, rutas con segmentos `..` y enlaces de compartición. No se aceptan URLs arbitrarias de API.

La entrada solo determina un ID validado. El backend construye la petición desde el destino configurado por administración; nunca descarga directamente la URL del usuario. Conserva la verificación DNS, los destinos privados aprobados, los certificados TLS, los límites de tiempo y la prohibición de seguir redirecciones de S07.

## API de captura

`POST /api/sources/memos` requiere una sesión web admitida, Origin exacto, token CSRF y `Content-Type: application/json`. El Bearer de comparación recibe `403`; las credenciales de Memos se obtienen únicamente de la conexión propia en el backend.

```json
{
  "destination": "notes",
  "reference": "memos/note-123",
  "expected_revision": 7
}
```

Los campos adicionales se rechazan. El backend comprueba primero el trabajo vigente, descifra la conexión y verifica `GET /api/v1/auth/me` antes de leer la nota. Esa comprobación impide que un token inválido se convierta en acceso anónimo a una nota pública.

La respuesta es el snapshot habitual, con `base_source` cuando hay procedencia. `GET /api/work` y MCP `get_current_work` también incluyen ese campo opcional en el nivel superior:

```json
{
  "provider": "memos",
  "destination": "notes",
  "resource": "memos/note-123",
  "url": "https://notes.example/memos/note-123",
  "captured_at": 1793145600,
  "updated_at": "2026-10-28T00:00:00Z"
}
```

`captured_at` utiliza segundos Unix del servidor. `updated_at` conserva `updateTime` del proveedor cuando está presente y es un string acotado; se omite cuando no existe. Es información de procedencia, no una suscripción a cambios. La etiqueta del documento es `Memos · ID.md`; `Document {label, markdown}` mantiene su contrato.

Enviar solo una propuesta mediante `PATCH /api/work` o MCP `submit_proposal` conserva la procedencia. Pegar/subir la propuesta en el visor utiliza PATCH con ese mismo resultado. Un PUT que reemplaza el original manualmente o un borrado elimina su procedencia. El cliente no puede inventarla mediante campos `source` o `base_source` en los contratos de entrada.

### Fallos y límites

| Situación | Resultado |
| --- | --- |
| Referencia, destino o JSON no permitido | Rechazo sin abrir una URL arbitraria ni modificar el trabajo. |
| Token inválido, permisos insuficientes, nota inexistente o respuesta inválida | Error genérico del proveedor; conserva contenido, procedencia y revisión vigentes. |
| Markdown mayor de 2 MiB UTF-8 decodificados | `413`, sin truncar o guardar parcialmente. |
| Respuesta de red excesiva o redirección | Error de proveedor; no sigue la redirección. |
| Trabajo cambiado o conexión reemplazada/desconectada durante la lectura | `409`; no se publica snapshot ni revisión SSE de esa captura. |

La petición de lectura conserva el timeout de 8 segundos. Para la respuesta JSON de una nota se admiten hasta `6 × 2 MiB + 64 KiB`, de modo que el escape de caracteres de control no reduzca el límite real de Markdown. Las comprobaciones de identidad y OAuth mantienen su límite anterior de 256 KiB. El margen JSON no amplía el tamaño del documento.

La sesión web se vuelve a verificar después de la lectura. La transacción que guarda el original comprueba tanto la revisión esperada del trabajo como la generación de la conexión y que esta siga conectada. Así una lectura tardía no restaura una captura tras desconectar o reemplazar credenciales. Un error nunca sustituye el trabajo confirmado.

## Persistencia y privacidad

El esquema SQLite 3 añade `current_work.base_source` y conserva el contenido del trabajo y las conexiones cifradas. La migración desde el esquema 2 es transaccional. El contenido y su procedencia se guardan juntos antes de actualizar la caché o emitir el evento privado.

El Markdown y los metadatos de procedencia se almacenan en claro dentro del directorio privado de trabajo; las credenciales siguen cifradas aparte. Reiniciar recupera la captura exacta. Solo se conserva la procedencia del último original, sin historial de capturas ni sincronización.

Un binario anterior a S08 rechaza el esquema 3. Conserva una copia coherente de la base antes de actualizar si necesitas rollback; no reduzcas manualmente `user_version`. Las limitaciones de borrado físico y backups siguen en [la guía de persistencia](current-work.md#borrado-funcional-copias-y-retención).

## Reproducir las pruebas

Con el binario `release` compilado y la fixture sintética de Keycloak iniciada:

```sh
python3 scripts/test_memos_source.py
```

La fixture Memos utiliza una cuenta sintética por usuario, notas inventadas y un servidor de loopback. Audita cada solicitud y dispone de rutas trampa para detectar descargas de adjuntos. No necesita notas, URLs o tokens personales.

La prueba de interfaz utiliza Playwright y un contrato simulado:

```sh
MDDIFF_BROWSER_CHANNEL=chrome node tests/browser/memos.mjs
```

En CI se utiliza Chromium y se omite la variable de canal. La prueba incluye contenido exacto, propuesta conservada, procedencia, errores, revisión capturada al abrir, PATCH, móvil, foco, logout y cambio de cuenta. El runner de backend comprueba además límites JSON/Markdown, cuentas distintas, reinicio real y carreras de desconexión o sustitución del trabajo.

La validación no consulta notas privadas reales ni acredita una lectura contra una instancia de producción. El PR continúa como WIP y el login Google/GitHub permanece pendiente.

### Evidencia del 2026-10-28

Pasaron 52 pruebas Rust, formato, Clippy y compilación `release`. Las pruebas de captura y de pantalla terminaron correctamente; también las regresiones de lector/MCP local, modos, login, persistencia, importaciones en Chrome, conexiones y MCP remoto.

| Área | Resultado comprobado |
| --- | --- |
| Fidelidad y procedencia | BOM, CRLF, Unicode y NUL conservados; nombre, URL, captura y fecha opcional coinciden entre trabajo, snapshot y restauración tras SIGKILL. |
| Propietarios y permisos | PAT propio de Alice/Bob; token revocado rechazado incluso para notas públicas; errores no exponen credenciales o respuestas privadas del proveedor. |
| Referencias y límites | ID, recurso y URL permitidos; referencias inválidas rechazadas sin red; 2 MiB decodificados aceptados con unos 12 MiB de escapes JSON y exceso rechazado. |
| Coordinación | Trabajo o conexión cambiados durante descarga producen `409`; logout produce `401`; las propuestas de MCP/PATCH conservan procedencia. |
| Pantalla y lectura | Apertura explícita, revisión fija, errores, móvil, foco y cambio de cuenta; auditoría limitada a GET de identidad y nota, sin adjuntos ni escrituras. |

La validación utiliza exclusivamente servidores y cuentas sintéticos. Las suites nuevas están incorporadas en GitHub Actions; los resultados anteriores corresponden a ejecuciones locales, no a una corrida remota de este nuevo commit.
