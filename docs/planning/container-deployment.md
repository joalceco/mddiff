# S12: contenedor alojado con datos y secretos separados

Issue: [#13](https://github.com/joalceco/mddiff/issues/13). Rama: `feature/multiuser-sources`.

Esta entrega prepara una instancia de mddiff para ejecutarse detrás de un proxy HTTPS. Incluye imagen, Compose, ejemplos de configuración, endpoint de salud y comprobaciones sintéticas. No publica el servicio ni configura DNS, certificados, router o cluster. El despliegue por invitación pertenece a [S14](https://github.com/joalceco/mddiff/issues/15); Google/GitHub y la prueba de capacidad de diez usuarios siguen pendientes.

Todos los dominios y rutas de este documento son ejemplos. Mantén la configuración de la instalación, sus credenciales y datos fuera del repositorio y del contexto de compilación.

## Contrato del contenedor

| Recurso | Contrato |
| --- | --- |
| Imagen | Compilación Rust con `Cargo.lock`; imágenes base fijadas por digest. La imagen final Debian incorpora el binario, certificados CA y `curl` para salud. No necesita Node.js. |
| Proceso | UID `10001`, GID `10001`, sistema raíz de solo lectura, `cap_drop: ALL` y `no-new-privileges`. Compose limita a 128 procesos, 512 MiB y una CPU. |
| Listener | Modo hosted en `0.0.0.0:3847` dentro del contenedor. Fuera de `127.0.0.1` exige hosted con OIDC y una IPv4 privada o `0.0.0.0`. |
| Trabajo | Un directorio local persistente en `/var/lib/mddiff`, `0700`, con `work.sqlite3` y auxiliares SQLite privados. Una instancia por base. |
| Secretos | Archivos normales `0600`, propiedad de `10001:10001`, montados en solo lectura bajo `/run/secrets/mddiff/`. La clave no pertenece al directorio de datos. |

El [`Dockerfile`](../../Dockerfile) compila el visor dentro del binario. La [lista permitida de `.dockerignore`](../../.dockerignore) limita el contexto a `Cargo.toml`, `Cargo.lock`, `Dockerfile`, `src/**/*.rs` y los tres assets `web/index.html`, `web/app.js` y `web/style.css`. Excluye los demás archivos, incluso dentro de `src/` y `web/`; no incorpora bases, documentos personales, fixtures, `.git` o secretos. Docker documenta cómo filtra ese contexto en [archivos `.dockerignore`](https://docs.docker.com/build/concepts/context/#dockerignore-files).

El builder usa Rust `1.95.0` sobre Bookworm y un solo trabajo de compilación (`--jobs 1`) para reducir la memoria necesaria. Las bases Rust y Debian Bookworm están fijadas por digest; `apt` instala las versiones disponibles de `ca-certificates` y `curl` para recibir sus actualizaciones. Esa fijación no promete imágenes idénticas byte por byte entre compilaciones. Conserva la referencia de la imagen que vayas a ejecutar.

[`deploy/compose.yaml`](../../deploy/compose.yaml) inicia solo mddiff en una red Docker privada, sin publicar puertos del host. El proxy puede compartir esa red y usar `mddiff:3847` como upstream. La aplicación necesita salida HTTPS hacia el IdP y los destinos autorizados; la ausencia de puertos publicados no sustituye las reglas de acceso a la red.

Compose usa `restart: unless-stopped`, `init: true` y 15 segundos de margen de parada. Sus límites de CPU, memoria y procesos son una configuración inicial; la capacidad para diez usuarios debe medirse en S13. Los bind mounts tienen `create_host_path: false`: prepara los archivos y directorios antes de arrancar, en lugar de esperar que Docker cree una ruta ausente.

Si el proxy se ejecuta en el host, el override [`deploy/compose.loopback.yaml`](../../deploy/compose.loopback.yaml) publica únicamente `127.0.0.1:3847`. No uses una publicación en todas las interfaces ni el modo de red del host para simplificar esa conexión. Las opciones de usuario, montajes, salud y protección del servicio se describen en la [referencia de servicios Compose](https://docs.docker.com/reference/compose-file/services/).

## Preparar datos y archivos privados en Linux

Necesitas Docker Engine con Compose, OpenSSL y un IdP Keycloak operativo con HTTPS. Este paquete no inicia Keycloak en modo `start-dev` ni crea cuentas reales. La fixture de `spikes/identity/` se utiliza únicamente en pruebas.

Los comandos de permisos siguientes corresponden a Docker Linux sin remapeo de usuarios. Si utilizas Docker rootless o user namespaces, ajusta la propiedad del host al UID/GID mapeado; conserva los permisos privados y verifica el acceso desde el contenedor.

1. Define ubicaciones fuera del checkout. Estas variables contienen solo rutas y una referencia de imagen:

   ```sh
   export MDDIFF_IMAGE=mddiff:staging
   export MDDIFF_DATA_DIR=/srv/mddiff/data
   export MDDIFF_SECRETS_DIR=/srv/mddiff/secrets
   ```

   [`deploy/examples/staging.env.example`](../../deploy/examples/staging.env.example) enumera estas mismas variables, sin credenciales; no añade opciones a la CLI ni necesita completarse con secretos.

2. Crea los directorios de datos y secretos para el usuario del contenedor:

   ```sh
   sudo install -d -m 0700 -o 10001 -g 10001 "$MDDIFF_DATA_DIR" "$MDDIFF_SECRETS_DIR"
   ```

3. Prepara copias privadas de los JSON en una carpeta del operador fuera del repositorio:

   ```sh
   install -d -m 0700 "$HOME/.config/mddiff-deploy"
   install -m 0600 deploy/examples/oidc.json.example "$HOME/.config/mddiff-deploy/oidc.json"
   install -m 0600 deploy/examples/connections.json.example "$HOME/.config/mddiff-deploy/connections.json"
   ```

   Edita esas copias privadas con los valores del entorno. Los ejemplos [OIDC](../../deploy/examples/oidc.json.example) y [conexiones](../../deploy/examples/connections.json.example) no deben completarse con secretos reales en el checkout. El segundo empieza con una lista Memos vacía para que agregues únicamente destinos aprobados.

4. Genera la clave de conexiones **una sola vez**, solo para una instalación nueva sin clave previa:

   ```sh
   sudo sh -euC -c '
     umask 077
     openssl rand 32 > "$1"
     chown 10001:10001 "$1"
     chmod 0600 "$1"
   ' sh "$MDDIFF_SECRETS_DIR/connections.key"
   ```

   `-C` impide sobrescribir un archivo existente. La clave contiene 32 bytes binarios, sin codificación hex o Base64. Si hay una clave existente, consérvala: generar otra no recupera las credenciales cifradas. Si el comando falla, revisa el archivo y el error antes de repetirlo; no lo reemplaces a ciegas.

5. Instala las configuraciones ya completadas con la propiedad y permisos que exige el proceso:

   ```sh
   sudo install -m 0600 -o 10001 -g 10001 "$HOME/.config/mddiff-deploy/oidc.json" "$MDDIFF_SECRETS_DIR/oidc.json"
   sudo install -m 0600 -o 10001 -g 10001 "$HOME/.config/mddiff-deploy/connections.json" "$MDDIFF_SECRETS_DIR/connections.json"
   ```

No pases tokens, contraseñas o la clave en variables de entorno, argumentos, instrucciones del Dockerfile o comandos de construcción. Compose recibe únicamente `MDDIFF_IMAGE`, `MDDIFF_DATA_DIR` y `MDDIFF_SECRETS_DIR`. Los tres archivos privados se montan en solo lectura; modificar un archivo del host para una actualización requiere reiniciar el servicio y volver a comprobarlo.

### Configuración OIDC y callbacks

En `oidc.json`, configura el emisor HTTPS exacto y `public_origin`, por ejemplo `https://review.example`. Incluye `client_id`, el `client_secret` web si el cliente es confidencial, `introspection_client_id`, `introspection_client_secret` y `audience`. Conserva `allow_insecure_loopback` desactivado. Los campos y mappers se explican en [autenticación web](hosted-auth.md#configurar-keycloak).

Registra estos retornos con el mismo origen canónico, sin comodines:

| Uso | Valor de ejemplo |
| --- | --- |
| Login web OIDC | `https://review.example/auth/callback` |
| Consentimiento personal Jira | `https://review.example/connections/jira/callback` |
| Recurso y audiencia MCP | `https://review.example/mcp` |

El recurso MCP utiliza su propio cliente OAuth y el scope `mddiff:compare`; no recibe los secretos de Jira/Memos. Conserva el mapper de audiencia del cliente de introspección además del mapper del recurso MCP. Consulta [la configuración de agentes remotos](remote-agents.md#preparar-el-cliente-oauth-en-keycloak).

La administración concede el rol `mddiff-user` a las identidades admitidas y mantiene desactivado el registro abierto. La disponibilidad del contenedor no crea una política de invitados ni habilita login social por sí sola.

### Configuración de conexiones

En la copia privada de `connections.json`, `key_file` debe ser **`/run/secrets/mddiff/connections.key`**, la ruta dentro del contenedor. Los destinos Memos y Jira se fijan en administración; no se configura una descarga arbitraria de URLs.

Memos admite el contrato `0.31.0`; configura `memos[].id`, `label`, `url` y, si requiere una instancia privada, las direcciones exactas aprobadas en `addresses`. No añadas rangos de red completos. Jira utiliza `client_id`, `client_secret` y `sites` con `id`, `label`, `url` y `cloud_id`. Puedes omitir `jira` o dejar vacía la lista Memos. No uses `jira_test_origin` ni la excepción HTTP de la fixture en producción. Consulta [los límites de destinos y credenciales](personal-connections.md#preparar-la-configuración-privada).

El ejemplo Compose habilita conexiones mediante ese archivo. La clave de cifrado debe conservarse por separado del volumen; perderla impide recuperar las conexiones, aunque siga disponible el Markdown guardado.

## Construir e iniciar una instancia

Desde la raíz del repositorio y con las variables no secretas anteriores:

1. Construye la imagen local:

   ```sh
   docker build --tag "$MDDIFF_IMAGE" .
   ```

2. Revisa la configuración resuelta de Compose, que no debe contener el contenido de secretos:

   ```sh
   docker compose -f deploy/compose.yaml config
   ```

3. Inicia el servicio en su red privada:

   ```sh
   docker compose -f deploy/compose.yaml up -d
   ```

   Si el proxy está en el host, utiliza este comando en su lugar y mantén el mismo par de archivos en las operaciones posteriores:

   ```sh
   docker compose -f deploy/compose.yaml -f deploy/compose.loopback.yaml up -d
   ```

4. Comprueba el estado del contenedor y la respuesta interna de salud:

   ```sh
   docker compose -f deploy/compose.yaml ps
   docker compose -f deploy/compose.yaml exec -T mddiff curl --noproxy '*' --fail --silent --show-error --head http://127.0.0.1:3847/healthz
   ```

5. Conecta el proxy HTTPS y verifica login, trabajo y MCP mediante el origen canónico antes de habilitar acceso a invitados.

Un archivo requerido ausente, inválido, con permisos incorrectos o de otro propietario impide arrancar esta configuración. El listener no loopback también exige OIDC inicializado; no se convierte en un servicio anónimo al faltar su configuración. Los logs sirven para diagnosticar el arranque, pero no deben incluir documentos, JSON privados, cookies, tokens o parámetros del callback.

No escales el servicio a varias réplicas sobre la misma base. SQLite WAL requiere almacenamiento local al host y no funciona sobre un filesystem de red; véase [Write-Ahead Logging](https://sqlite.org/wal.html). La persistencia del volumen permite recuperar el trabajo tras recrear el contenedor, pero no protege contra la pérdida de ese volumen.

## Proxy HTTPS

[`deploy/nginx.conf.example`](../../deploy/nginx.conf.example) es una plantilla para `https://review.example`. Sustituye dominio, certificados y upstream en la configuración privada del operador; este archivo no obtiene certificados ni publica el servicio. Para un proxy en la misma red usa `mddiff:3847`; para un proxy en el host con el override usa `127.0.0.1:3847`.

[`deploy/cloudflared-ingress.yml.example`](../../deploy/cloudflared-ingress.yml.example) muestra una regla alternativa para un túnel existente que comparte la red privada de Docker. Fija el Host canónico y conserva una regla final `404`. Completarla o añadirla a la configuración privada del túnel pertenece al despliegue; esta entrega no crea túneles ni guarda sus credenciales.

| Condición | Configuración requerida |
| --- | --- |
| Origen | Conserva el Host canónico y el Origin del cliente; deben coincidir con `public_origin`. No inventes Origin para eludir la validación. |
| Credenciales | Conserva Authorization, Cookie y `X-CSRF-Token`. No los registres ni los añadas a URLs. |
| Forwarding | La plantilla Nginx elimina `Forwarded`, `X-Forwarded-Host`, `X-Forwarded-Proto` y `X-Forwarded-For` aportados por el cliente; la identidad y el origen no dependen de esos headers. |
| Tamaño | Permite cuerpos de hasta **25 182 208 bytes**, equivalentes a 24 MiB + 16 KiB; cada documento sigue limitado a 2 MiB. |
| SSE y logs | Desactiva buffering para `/api/events`, permite conexiones largas y evita registrar queries OAuth o contenido. |

La terminación TLS no sustituye la autenticación. El proxy debe alcanzar el backend por la red privada o loopback; la aplicación sigue comprobando Host, Origin, cookies/CSRF o Bearer según el endpoint. La plantilla no habilita acceso sin rol ni acepta rutas de archivos del host.

## Salud y terminación

`GET /healthz` y `HEAD /healthz` son públicos únicamente en hosted y no devuelven contenido. Obtienen `204` cuando OIDC está inicializado y una lectura `SELECT 1` en SQLite puede ejecutarse; una condición no disponible devuelve `503`. En local, la ruta responde `404`.

La salud no hace una introspección nueva ni una petición continua al IdP. Un `204` no demuestra que el login, el proveedor o una API externa estén disponibles en ese instante: las solicitudes protegidas siguen verificando sus credenciales y pueden fallar. El healthcheck interno usa loopback para no depender del proxy y no expone documentos o identidades.

Fuera de Compose, hosted sin OIDC puede arrancar cerrado en loopback para compatibilidad: contenido con `401` y salud con `503`. Ese modo no cumple la preparación del contenedor y no puede usar un bind externo.

`SIGTERM` y `SIGINT` terminan el proceso con código `0`, incluso con un SSE abierto. Utiliza `docker compose -f deploy/compose.yaml stop` para detenerlo. Las sesiones web se pierden al reiniciar; cada persona vuelve a autenticarse y recupera su trabajo persistente.

## Datos, actualización y recuperación

El volumen contiene Markdown en claro, procedencia y credenciales de conexiones cifradas. Los permisos Unix y el cifrado de credenciales no protegen contra un administrador con acceso al proceso, a la clave o al host. No adjuntes SQLite, WAL, SHM, archivos privados o logs con contenido a issues.

Antes de actualizar, conserva la referencia de imagen actual y revisa las migraciones de esquema. Un binario antiguo puede rechazar una base migrada; cambiar únicamente la etiqueta de imagen no garantiza un rollback. El procedimiento de copia coherente, retención, recuperación de clave e IdP y restauración debe cerrarse en S14 antes de publicar. Esta entrega no crea backups automáticos ni fija una retención nueva.

| Problema | Acción |
| --- | --- |
| Arranque rechazado | Revisa archivos requeridos, `0600`, UID/GID, JSON, clave binaria y conectividad al issuer HTTPS. No relajes permisos o la validación TLS. |
| Base ocupada o sin escritura | Detén la instancia duplicada y revisa directorio, propietario y espacio. No borres WAL/SHM para forzar el arranque. |
| Salud `503` | Comprueba inicialización OIDC y lectura SQLite; después verifica login por separado. |
| Proxy devuelve `403` | Revisa Host/Origin y el origen canónico sin quitar controles de la aplicación. |
| Conexiones no se recuperan | Conserva la base y la clave original. Si no existe una clave recuperable, las cuentas deberán autorizarse de nuevo. |

## Verificar el empaquetado

La comprobación sintética necesita Docker, OpenSSL y Python 3 en el host; no necesita Rust/Cargo instalado fuera del builder. Utiliza archivos temporales de prueba y la fixture Keycloak local, sin documentos privados ni credenciales de proveedores reales. El runner descarga, si falta, una imagen oficial Python fijada por digest para su cliente de pruebas.

1. Construye la imagen **antes** de iniciar la fixture:

   ```sh
   docker build --tag mddiff:issue13 .
   ```

2. Ejecuta la integración con retirada de la fixture también ante un error:

   ```sh
   (
     set -e
     trap 'python3 spikes/identity/keycloak.py down' EXIT
     python3 spikes/identity/keycloak.py up
     python3 scripts/test_container.py --image mddiff:issue13
   )
   ```

El arnés está en [`scripts/test_container.py`](../../scripts/test_container.py) y [`scripts/container_client.py`](../../scripts/container_client.py). El primero audita el contexto y la imagen y coordina los contenedores; el segundo verifica los contratos de la aplicación con datos sintéticos.

### Evidencia del 2026-10-28

La construcción y la suite Docker completa terminaron con código `0` en Linux arm64. El recurso HTTPS utilizó una CA temporal verificada; la prueba negativa rechazó esa CA sin confianza explícita.

| Comprobación | Resultado observado |
| --- | --- |
| Imagen y contexto | Auditoría de la imagen y lista permitida; los canarios sintéticos privados quedaron fuera del contexto de construcción. |
| Recorrido por usuario | Dos cuentas, captura de Memos, MCP con Bearer y SSE, con separación del trabajo entre identidades. |
| Continuidad | Reiniciar y recrear el contenedor conserva trabajo, procedencia y credencial cifrada en los volúmenes. |
| Terminación | `SIGTERM` termina con código `0` y cierra un SSE abierto. |
| Rechazos | Permisos o propietario incorrectos y una segunda instancia sobre la misma SQLite se rechazan. |

También pasaron las 56 pruebas Rust y Clippy. Estas comprobaciones del código se ejecutaron por separado; no añaden Rust como requisito del host para reproducir la suite Docker.

GitHub Actions aprobó los jobs `check` y `container` en Linux amd64 para el código de empaquetado `c4c9382`: [corrida de CI](https://github.com/joalceco/mddiff/actions/runs/40741776582). Incluye las regresiones locales/alojadas, controles de navegador y construcción e integración del contenedor.

No se validaron un IdP real con HTTPS, el cluster, el despliegue público o la carga de diez usuarios. El IdP y los proveedores de esta suite son sintéticos. S13 cubre límites y capacidad; S14, publicación y recuperación operativa.
