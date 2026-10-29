# S07: conexiones personales de lectura

Fecha: 2026-10-27. Issue: [#8](https://github.com/joalceco/mddiff/issues/8). Rama: `feature/multiuser-sources`.

Este documento registra S07. La entrega posterior [S08: captura de Memos](memos-source.md) permite abrir una nota como original con estas conexiones y añade el esquema SQLite 3 para su procedencia.

Estado al **2026-10-29**: la beta ejecuta el commit `6bfccea841530e4cf8a49fc5ab7b1e5d9f670070` con SQLite 4 y conexiones cifradas conservadas tras la migración. Las pruebas de proveedores siguen siendo sintéticas. Jira real requiere un secreto OAuth válido del operador; [#17](https://github.com/joalceco/mddiff/issues/17) bloquea habilitar Sharing para otras cuentas de Atlassian. Google/GitHub y una integración personal real de Memos siguen pendientes.

## Resultado y alcance

Cada persona autenticada puede conectar, comprobar y desconectar sus propias cuentas desde **Conexiones**, junto a **Cerrar sesión**. Administración define los destinos disponibles; el usuario no introduce URLs. La conexión pertenece a la identidad verificada `issuer` + `sub`, igual que el trabajo actual.

Jira Cloud utiliza consentimiento personal OAuth 2.0 (3LO). Memos utiliza un token personal y el contrato de **Memos 0.31.0**, con `GET /api/v1/auth/me` para comprobarlo. La API 26.10 cambia las rutas y no se admite mediante detección automática o fallback.

S07 administra conexiones. [S08](memos-source.md) y [S09](jira-source.md) incorporan la captura de notas y descripciones como original. Desconectar conserva el último original y propuesta. El despliegue público se registra por separado en [la operación de la beta](beta-operations.md); no demuestra una conexión personal real a esos proveedores.

## Preparar la configuración privada

Necesitas una sesión web con Keycloak, un directorio de datos y una clave de cifrado externa. Sin `--connections-config`, las conexiones permanecen deshabilitadas y el lector continúa funcionando.

1. Crea una clave nueva de **32 bytes binarios** en una carpeta privada fuera del repositorio y del directorio de datos. Hazlo una sola vez; conserva la clave existente si ya hay conexiones:

   ```sh
   mkdir -p "$HOME/.config/mddiff"
   (set -C; umask 077; openssl rand 32 > "$HOME/.config/mddiff/connections.key")
   ```

2. Crea un JSON privado con permisos `0600`, por ejemplo `connections.json`, con los destinos aprobados. Sustituye los valores del ejemplo:

   ```json
   {
     "key_file": "/run/secrets/mddiff-connections.key",
     "memos": [
       {
         "id": "notes",
         "label": "Mis notas",
         "url": "https://notes.example",
         "addresses": []
       }
     ],
     "jira": {
       "client_id": "ID_DE_LA_APLICACION",
       "client_secret": "REEMPLAZAR_POR_EL_SECRETO",
       "sites": [
         {
           "id": "issues",
           "label": "Jira del equipo",
           "url": "https://your-site.atlassian.net",
           "cloud_id": "ID_DEL_SITIO"
         }
       ]
     }
   }
   ```

3. Inicia el modo alojado con los archivos privados existentes:

   ```sh
   cargo run --release --locked -- --mode hosted --port 3848 \
     --oidc-config "$HOME/.config/mddiff/oidc.json" \
     --data-dir "$HOME/.local/share/mddiff" \
     --connections-config "$HOME/.config/mddiff/connections.json"
   ```

4. Abre **Conexiones** desde una sesión web e inicia la conexión personal al destino elegido.

`key_file` debe apuntar al archivo creado en el primer paso; el JSON muestra una ruta de montaje de ejemplo. No añadas estos archivos, claves o datos al repositorio. El modo local rechaza `--connections-config`; el modo alojado exige además `--oidc-config`. Puedes omitir `jira` o dejar `memos` vacío. Se admiten hasta 32 destinos con identificadores únicos.

Las URLs deben ser orígenes HTTPS sin credenciales, rutas, query o fragmento. Los sitios Jira deben estar bajo `.atlassian.net`; su `cloud_id` se fija en administración. Para una instancia privada de Memos, `addresses` debe contener las IP exactas aprobadas. Si se configura esa lista, todas las direcciones DNS resueltas deben pertenecer a ella. Una IP privada aprobada no habilita el resto de su subred.

Cada petición fija sus resultados DNS comprobados, desactiva proxies del entorno y no sigue redirecciones. Se bloquean loopback, link-local, direcciones de metadatos y otros rangos especiales; solo las IP privadas ordinarias aprobadas explícitamente pueden utilizarse en producción. La resolución tiene un límite de 2 segundos; la petición, 8 segundos y 256 KiB de respuesta. No existe un proxy genérico ni un endpoint que reciba URLs del usuario.

## Consentimiento de Jira Cloud

Administración registra una aplicación 3LO de mddiff en Atlassian; cada usuario concede acceso a esa aplicación con su propia cuenta. No se pide un API token de Jira ni un secreto de aplicación a cada usuario. Elige acceso **Resource-level** para limitar el consentimiento al sitio seleccionado. La aplicación es privada por defecto; su propietario puede probarla con su propia cuenta una vez configurado el secreto OAuth válido.

**Sharing permanece deshabilitado.** La consola exige una atestación sobre Personal Data Reporting API para aplicaciones que almacenan datos personales y mddiff aún no implementa esa API. [#17](https://github.com/joalceco/mddiff/issues/17) bloquea autorizar otras cuentas de Atlassian. No se ha emitido esa atestación. Habilitar Sharing en una entrega futura permitiría que otras personas autoricen sus conexiones; no les concedería acceso automático a datos de otros usuarios ni publicaría la aplicación en Marketplace.

1. Registra exactamente `https://review.example/connections/jira/callback` usando el origen público real de mddiff.
2. Añade el scope clásico `read:jira-work`. El flujo solicita además `offline_access` para renovar el acceso. No añadas permisos de escritura para este conector.
3. Configura el sitio y `cloud_id` autorizados en el JSON privado.
4. Pulsa **Conectar** y acepta el consentimiento personal de Atlassian.
5. Pulsa **Comprobar** para verificar el acceso al sitio aprobado.

El flujo usa Authorization Code con cliente confidencial. El estado dura cinco minutos, se consume una sola vez y queda ligado al usuario, la sesión web concreta, el destino y su revisión. Otra cuenta o una sesión distinta del mismo usuario no puede completar ese flujo. Un callback posterior a una desconexión no restaura la credencial.

Configura el proxy de publicación para no registrar queries del callback, cabeceras Authorization o cookies. La aplicación devuelve errores genéricos y no imprime respuestas OAuth ni valores rechazados por el parser JSON.

La comprobación consulta `GET https://api.atlassian.com/oauth/token/accessible-resources` y exige coincidencia del sitio, su URL y los scopes de lectura previstos. No solicita perfil ni `read:jira-user`. Los únicos POST salientes son los intercambios y renovaciones OAuth en `https://auth.atlassian.com/oauth/token`.

Los refresh tokens rotativos se cifran y guardan antes de continuar con otras solicitudes. La renovación se serializa por usuario y destino; desconectar puede invalidar una comprobación en curso. Cortar la conexión HTTP del cliente no cancela una renovación ya iniciada.

Las respuestas de intercambio y renovación admiten que `token_type` esté ausente, como muestran los ejemplos de Atlassian. Si aparece, debe ser una cadena `Bearer`; un tipo distinto o inválido se rechaza. Este comportamiento no amplía los scopes ni los sitios permitidos.

La [guía oficial de Jira 3LO](https://developer.atlassian.com/cloud/jira/platform/oauth-2-3lo-apps/), consultada el 2026-10-29, describe consentimiento, sitios accesibles, distribución y refresh tokens. Los scopes de la API se detallan en [scopes de Jira Cloud](https://developer.atlassian.com/cloud/jira/platform/scopes-for-oauth-2-3LO-and-forge-apps/).

## Token personal de Memos

Crea un token separado para mddiff desde los ajustes de tu usuario en Memos. El contrato de 0.31.0 permite definir descripción y caducidad, pero no permisos de solo lectura por token. La interfaz exige aceptar este aviso antes de enviarlo:

> El token de Memos puede permitir cambios. mddiff solo realiza lecturas.

El conector solo comprueba la identidad mediante GET y no implementa operaciones de escritura en Memos. El alcance real del token sigue siendo el de tu usuario en el proveedor. No uses un token de administrador si no lo necesitas. Consulta el [contrato de tokens de Memos 0.31.0](https://github.com/usememos/memos/blob/v0.31.0/proto/api/v1/user_service.proto) y la [guía de acceso API y versiones](https://usememos.com/docs/integrations/api-access).

El campo de contraseña se vacía al enviar, cancelar, cerrar el diálogo o terminar/cambiar de sesión. El secreto nunca se devuelve en las respuestas ni se guarda en localStorage. Si la comprobación falla, vuelve a pegarlo para reintentar.

## Cifrado, borrado y recuperación

La biblioteca `ring` cifra las credenciales con AES-256-GCM y un nonce aleatorio nuevo de 12 bytes en cada escritura. Los datos autenticados vinculan el cifrado a su versión, emisor, sujeto, proveedor, destino y generación. Mover un blob a otra identidad o alterar esos campos impide descifrarlo.

La clave externa requiere un archivo regular `0600`, sin enlace simbólico y de 32 bytes exactos, fuera del directorio de datos. Se mantiene en memoria durante el proceso; no se copia a SQLite. Las credenciales tampoco se exponen mediante MCP, la API de propuestas, logs o las respuestas de conexiones. Los administradores con acceso al proceso y a la clave conservan acceso a los secretos; este cifrado protege el almacenamiento, no un host comprometido.

S07 incorporó el esquema SQLite 2 para `encrypted_connections`, conservando `current_work` mediante una migración transaccional desde 1. El almacenamiento actual usa el esquema 4, que añade procedencia opcional sin reemplazar las conexiones cifradas. El ensayo real del 2026-10-29 verificó 3→4, rollback de la aplicación desde un checkpoint compatible con 3 y nueva migración a 4, con hashes de los registros previos idénticos e IdP/PostgreSQL conservados. Un binario antiguo no puede abrir un esquema incompatible; no reduzcas manualmente `user_version`. Consulta [la recuperación y sus límites](beta-operations.md#restauración-y-rollback).

**Desconectar** pone nonce y ciphertext en NULL e incrementa la generación. El registro vacío impide reutilizar una revisión antigua; no conserva el token recuperable. No borra el trabajo capturado ni llama a escrituras del proveedor. Para revocar también en origen, elimina el token en Memos o retira el acceso de mddiff en las aplicaciones conectadas de Atlassian. No se inventa un endpoint de revocación 3LO.

Las limitaciones de WAL, backups y borrado físico siguen siendo las de [la persistencia del trabajo](current-work.md#borrado-funcional-copias-y-retención). Conserva un respaldo separado y protegido de la clave: sin ella no puedes recuperar las conexiones cifradas; será necesario desconectarlas y autorizar de nuevo. Esta entrega no añade rotación automática de claves. Cambiar URL, cloud ID o client ID invalida el uso de la credencial previa y requiere reconectar.

## API de administración personal

Estas rutas requieren **sesión web**. Las mutaciones también requieren Origin exacto, `X-CSRF-Token` y JSON. El Bearer de comparación recibe `403`; MCP no expone herramientas para administrar secretos.

| Método | Contrato |
| --- | --- |
| `GET /api/connections` | `{enabled, destinations}`; cada destino incluye `id`, `provider`, `label`, `url`, `connected` y `revision`, sin secretos. |
| `POST /api/connections/{id}/memos` | `{expected_revision, token, acknowledge_full_access:true}`; valida la identidad antes de guardar. |
| `POST /api/connections/{id}/authorize` | `{expected_revision}`; devuelve `authorization_url` para el consentimiento Jira. |
| `POST /api/connections/{id}/check` | `{expected_revision}`; comprueba la conexión y puede renovar Jira. |
| `DELETE /api/connections/{id}` | `{expected_revision}`; desconecta conservando el trabajo. |

Guardar, comprobar y desconectar devuelven `{revision, connected}`. Una renovación puede aumentar la revisión. Una revisión antigua produce `409`, sin sobrescribir la conexión vigente. Tras una mutación o un error, relee la lista antes de reintentar. Los cuerpos con propietarios, rutas o campos adicionales se rechazan. El callback redirige a un resultado genérico sin incluir códigos o respuestas del proveedor en el mensaje.

## Reproducir las pruebas

Con el binario `release` compilado y la fixture Keycloak iniciada:

```sh
python3 scripts/test_connections.py
```

El runner crea proveedores HTTP sintéticos en puertos efímeros. La configuración privada de prueba utiliza `allow_insecure_loopback:true` y `jira_test_origin`; ambos quedan restringidos a un origen público de loopback. Esta excepción no es una configuración de despliegue.

La prueba de navegador usa Playwright instalado en `tests/browser`:

```sh
node tests/browser/connections.mjs
```

En un equipo con Chrome instalado puedes usar `MDDIFF_BROWSER_CHANNEL=chrome`. GitHub Actions incorpora ambos runners. La suite de proveedores audita métodos, rutas y principal de cada solicitud de negocio: permite GET de comprobación y POST de OAuth, sin escrituras de documentos.

La validación de los conectores utiliza usuarios y proveedores sintéticos; no acredita consentimiento con cuentas reales de Jira o Memos ni login social. El despliegue del visor sí está registrado en [S14](beta-operations.md#actualización-del-2026-10-29), como evidencia separada. Para probar Jira real falta un secreto OAuth válido del operador y el consentimiento de la cuenta propietaria; distribuir la autorización a otras cuentas conserva el bloqueo de #17.

### Evidencia histórica del 2026-10-27

Las 46 pruebas Rust, formato, Clippy y compilación `release` terminaron correctamente. Pasaron la integración de conexiones y la prueba de contrato en Chrome; también las regresiones de login, persistencia, importación de Markdown, modos y MCP local/remoto.

| Área | Evidencia |
| --- | --- |
| Propietarios | Alice y Bob usan sus propias credenciales; se audita el principal enviado al proveedor. |
| OAuth | Scopes mínimos, consentimiento, estado de un solo uso, SID distinto del mismo usuario, cuenta cruzada y callback tardío rechazados. |
| Secretos | Cifrado sin tokens en BD/journal; cambio de nonce; claves o datos alterados fallan; errores de proveedor y JSON no devuelven secretos. |
| Recuperación | Reinicio tras SIGKILL, migración de esquema, refresh rotativo, desconexión durante comprobación y trabajo capturado conservado. |
| Pantalla | Aviso y aceptación de alcance, limpieza de token, foco/Escape, móvil, errores, logout y respuestas de cuentas anteriores. |

La auditoría saliente permitió únicamente GET de comprobación y POST de OAuth. GitHub Actions incluye ambas suites nuevas; esta evidencia corresponde a ejecuciones locales, sin atribuir una corrida remota de CI.

### Evidencia del 2026-10-29

La [corrida de CI 40904080796](https://github.com/joalceco/mddiff/actions/runs/40904080796) aprobó `check` y `container`, con 70 pruebas Rust, regresiones de conexiones y capacidad de diez usuarios sintéticos. La beta desplegada usa SQLite 4; sus métricas y cinco sondas públicas pasaron y el timer de respaldos está activo. Sobre la imagen publicada `6bfccea` se verificaron la recuperación del trabajo nuevo y el recorrido real de Codex por HTTPS público con dos cuentas y cero turnos de modelo. No se atribuye a esta evidencia una lectura real de Jira o Memos.
