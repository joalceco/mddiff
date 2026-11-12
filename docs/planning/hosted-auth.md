# S03: autenticación web alojada con Keycloak

Fecha: 2026-10-27. Issue: [#4](https://github.com/joalceco/mddiff/issues/4). Rama: `feature/multiuser-sources`.

Este documento registra la entrega S03. S04 incorpora persistencia y exige también `--data-dir`; consulta [el trabajo actual y su borrado](current-work.md) para el contrato vigente. [S06: agentes remotos](remote-agents.md) incorpora Bearer y MCP alojado sobre esta autenticación. Las sesiones OIDC siguen en memoria.

Las referencias a loopback y capacidades pendientes describen el estado de S03. [S12: contenedor alojado](container-deployment.md) añade bind explícito con OIDC detrás de un proxy HTTPS, sin completar el despliegue público ni Google/GitHub.

## Resultado y alcance

La aplicación principal incorpora un adaptador OIDC para autenticar sesiones web contra Keycloak. Después de verificar la sesión y la admisión, el servidor obtiene el espacio privado asociado al par estable `issuer` + `sub`. El nombre visible y el email no deciden la propiedad del espacio.

Este avance continúa limitado a loopback. **Google/GitHub siguen pendientes**, al igual que el despliegue del cluster. El trabajo y las sesiones permanecen en memoria: la persistencia pertenece a S04. La importación de texto y archivos pertenece a S05; autenticar una sesión no habilita las rutas locales ni un MCP alojado.

La integración se verificó el 2026-10-27 contra Keycloak 26.8.1 con cuentas sintéticas. Google/GitHub quedan pendientes; esta validación no completa el alcance social del issue #4.

## Inicio y configuración privada

El modo local sigue siendo el predeterminado y no consume configuración OIDC. Para activar el adaptador web:

```sh
cargo run --release --locked -- --mode hosted --port 3848 --oidc-config "$HOME/.config/mddiff/oidc.json" --data-dir "$HOME/.local/share/mddiff"
```

El JSON debe estar fuera del repositorio, en un archivo normal, sin enlaces simbólicos, con permisos `0600` en Unix y un máximo de 16 KiB. No lo adjuntes a issues, logs o capturas. El proceso rechaza configuraciones mal formadas o campos desconocidos.

Ejemplo de estructura con dominios y secreto de sustitución:

```json
{
  "issuer": "https://identity.example/realms/mddiff",
  "public_origin": "https://review.example",
  "client_id": "mddiff-hosted-web",
  "introspection_client_id": "mddiff-resource",
  "introspection_client_secret": "REEMPLAZAR_POR_EL_SECRETO_DEL_CLIENTE",
  "audience": "mddiff-resource"
}
```

| Campo | Uso |
| --- | --- |
| `issuer` | Emisor OIDC exacto del realm autorizado. |
| `public_origin` | Origen del navegador, con esquema y host, sin ruta, query o fragmento. Debe coincidir con Host y Origin. |
| `client_id` | Cliente OIDC del flujo web. |
| `client_secret` | Opcional: secreto del cliente web si se configura como confidencial. No es el secreto del cliente de introspección. |
| `introspection_client_id`, `introspection_client_secret` | Cliente confidencial usado por el backend para verificar el access token. |
| `audience` | Audiencia que el backend exige en la respuesta de introspección. |
| `allow_insecure_loopback` | `false` por defecto. La excepción HTTP solo se acepta cuando tanto `issuer` como `public_origin` son de loopback. |

Fuera de la fixture local, los orígenes deben usar HTTPS. Los endpoints descubiertos de autorización, tokens, claves, introspección y revocación deben pertenecer al mismo origen y quedar bajo la ruta del issuer. El cliente HTTP no sigue redirecciones y tiene un timeout de cinco segundos.

`public_origin` describe el origen esperado; no crea un listener TLS ni cambia el enlace del proceso, que sigue escuchando en `127.0.0.1`. La terminación HTTPS, el proxy y la exposición del cluster corresponden a la historia de despliegue. Este ejemplo no constituye una configuración lista para publicación.

Sin `--oidc-config`, el modo hosted continúa cerrado: `/api/state` y `/api/events` devuelven `401`. La CLI rechaza configuración OIDC en local y rechaza `--root` o archivos posicionales en hosted.

## Configurar Keycloak

Configura un realm dedicado o uno autorizado para la aplicación. La fixture reproducible utiliza Keycloak 26.8.1; no se deben trasladar su modo `start-dev`, contraseñas sintéticas o excepciones HTTP a un despliegue público.

1. Crea el cliente OIDC web `mddiff-hosted-web` con **Standard flow** y PKCE `S256`. Registra exactamente `PUBLIC_ORIGIN/auth/callback` como URI de redirección. Desactiva password grant y evita callbacks con comodines.
2. Crea el cliente confidencial `mddiff-resource`, sin login web ni password grant. Su secreto se utiliza exclusivamente en el backend para introspección.
3. Incluye `mddiff-resource` en `aud` del access token del cliente web y en su respuesta de introspección. Configura un audience mapper con `included.client.audience = mddiff-resource`, habilitado para access token e introspección.
4. Incluye el sujeto y los roles de realm en introspección. La aplicación exige `active`, `iss`, `sub`, `aud`, `exp`, cliente emisor (`client_id`, o `azp` si falta) y el rol `mddiff-user` dentro de `realm_access.roles`.
5. Concede el rol `mddiff-user` únicamente a las identidades aprobadas. Mantén desactivado el registro abierto. Poder autenticarse en el IdP no basta para entrar en mddiff.

El cliente de introspección debe ser audiencia del token para poder inspeccionarlo con esta configuración. No elimines la comprobación de audiencia si la introspección falla. Los mappers y scopes deben exponer los claims requeridos también en introspección, no únicamente en el JWT.

La admisión implementada es una asignación administrativa de rol a una identidad concreta. No es una invitación por correo de un solo uso ni un flujo de enrolamiento social. Desactivar el registro del realm no sustituye la política de admisión de futuros brokers Google/GitHub. Tampoco se fusionan cuentas por coincidencia de email.

Para retirar acceso, quita el rol y cierra las sesiones de esa persona en Keycloak. La aplicación vuelve a comprobar los tokens anteriores; no confía solamente en la autorización que tenían al iniciar sesión. Véase el [experimento de identidad](identity-spike.md) para los antecedentes de audiencias y revocación.

## Sesión, autorización y cierre

El login usa Authorization Code con PKCE `S256`, estado ligado al flujo del navegador y nonce. La biblioteca OIDC verifica el ID token; el backend verifica además el access token mediante introspección y exige que ambos correspondan al mismo sujeto.

El navegador recibe un identificador de sesión opaco. Los tokens del proveedor y el secreto del cliente permanecen en el backend y no se guardan en `localStorage` ni se devuelven desde `/auth/session`.

| Propiedad | Contrato |
| --- | --- |
| Cookie en HTTPS | `__Host-mddiff-session`, `HttpOnly`, `Secure`, `SameSite=Lax`, `Path=/`, sin `Domain`. El flujo temporal usa `__Host-mddiff-flow`. |
| Excepción de fixture HTTP | Nombres `mddiff-session` y `mddiff-flow`, sin `Secure`, únicamente al habilitar explícitamente ambos orígenes de loopback. |
| Identidad | `issuer` + `sub` verificados. Headers, query y etiquetas enviados por el cliente no seleccionan al propietario. |
| Vigencia | La menor entre la vida del access token y una hora. No hay renovación automática de sesión. |
| Solicitudes protegidas | Introspección en cada solicitud. Un proveedor no disponible impide autorizarla; no se utiliza una identidad almacenada como sustitución. |
| SSE | Revalidación antes de emitir y cada dos segundos durante la espera. Con timeout de introspección de cinco segundos, el presupuesto de detección y cierre es de hasta siete segundos. |
| Escrituras por cookie | Origin exacto y `X-CSRF-Token` asociado a la sesión, además de JSON para los POST. |

La navegación OIDC permite llegar desde el proveedor a `/auth/callback`; esa excepción no habilita solicitudes entre sitios a la API. El estado del callback, PKCE y nonce siguen siendo obligatorios.

La sesión puede terminar antes de su máximo si el proveedor revoca el acceso, la admisión deja de ser válida o el proceso se reinicia. El refresh token, cuando existe, se conserva solo para intentar revocarlo al cerrar sesión; no amplía la vida de la sesión web.

### Endpoints web

| Endpoint | Comportamiento |
| --- | --- |
| `GET /auth/login` | Inicia el flujo con el proveedor configurado. |
| `GET /auth/callback` | Verifica el retorno y crea la sesión admitida. |
| `GET /auth/session` | Devuelve `user_label` y `csrf_token` después de verificar la sesión. |
| `POST /auth/logout` | Requiere JSON, Origin exacto y CSRF; invalida el identificador local y elimina la cookie. |
| `GET /api/state`, `GET /api/events` | Acceden únicamente al espacio del propietario verificado. |

Una navegación anónima al visor redirige al login. La API sin sesión válida devuelve `401`; una identidad autenticada sin admisión obtiene `403`. Una caída de introspección produce un error de disponibilidad en las solicitudes y termina el SSE protegido.

El cierre local es autoritativo: el SID anterior deja de ser válido aunque falle la petición de revocación del refresh token. La revocación remota es un intento adicional. **No se promete un logout global del SSO**, de Google/GitHub o de otras aplicaciones del realm; una nueva entrada puede reutilizar una sesión vigente del proveedor.

## Comprobar con la fixture sintética

Requisitos: Docker en ejecución, Rust/Cargo, Python 3 y el puerto local 8848 disponible. No necesitas cuentas reales, credenciales sociales o el gateway independiente del experimento para esta prueba de la aplicación principal.

Ejecuta desde la raíz del repositorio:

1. Inicia el Keycloak sintético y espera a que anuncie disponibilidad:

   ```sh
   python3 spikes/identity/keycloak.py up
   ```

2. Compila la aplicación principal:

   ```sh
   cargo build --release --locked
   ```

3. Ejecuta la integración alojada:

   ```sh
   python3 scripts/test_hosted_auth.py
   ```

4. Retira únicamente el contenedor de la fixture al terminar:

   ```sh
   python3 spikes/identity/keycloak.py down
   ```

El arnés inicia mddiff con un puerto temporal libre, registra un cliente web propio y restaura o retira ese cliente al terminar. Los secretos aleatorios y la configuración temporal se guardan con permisos privados bajo `target/identity-spike/`, ignorado por Git; no se imprimen. `down` retira el contenedor, pero deja esos archivos privados ignorados. Esta ubicación dentro de `target/` es exclusiva del arnés; la configuración de un entorno administrado se mantiene fuera del checkout.

La prueba está diseñada para comprobar admisión de Alice/Bob y rechazo de Mallory, identidades separadas, suplantación rechazada, callback inválido, límites de Origin/CSRF, expiración, logout y replay, revocación administrativa y cierre del SSE. También verifica que una sesión admitida no active `/api/open`, `/api/compare` ni `/mcp` en hosted.

Los usuarios y documentos son sintéticos. La fixture no valida login Google/GitHub, invitaciones por correo, persistencia tras reinicios, TLS de producción o un despliegue público.

## Evidencia de validación

Corrida del 2026-10-27: formato, Clippy sin advertencias, **29 pruebas Rust** y compilación `release` correctos. Las integraciones del MCP local y de separación de modos también pasaron.

La integración contra Keycloak real comprobó estos casos con usuarios sintéticos:

| Caso | Resultado |
| --- | --- |
| Alice y Bob admitidos; Mallory sin rol | Los dos invitados acceden con sesiones distintas; Mallory obtiene `403` y no recibe sesión de aplicación. |
| Callback entre navegadores y replay | Trasplantar un callback real a otro flujo obtiene `400`; el navegador legítimo puede completarlo una vez. El replay obtiene `400`. |
| Cambio de cuenta | Iniciar como Bob después de Alice rota SID y CSRF; el SID anterior obtiene `401`. |
| Logout | Origin/CSRF ausentes o incorrectos obtienen `403`; el cierre válido elimina la cookie, rechaza replay y termina el SSE en menos de ocho segundos. La sesión de otra persona continúa válida. |
| Caducidad | Un access token de prueba con vigencia de dos segundos deja de autorizar estado y sesión. |
| Retirada del rol | Quitar únicamente `mddiff-user` bloquea el acceso antes de ejecutar logout del proveedor y termina el SSE. Restaurar el rol no resucita el SID invalidado. |
| Logout administrativo | Una cuenta que conserva su rol pierde acceso y SSE después de cerrar sus sesiones desde Keycloak. |

La comprobación de retirada del rol exige el rechazo en la fixture fijada; no recurre a un logout adicional para hacer pasar la prueba. La operación administrativa recomendada sigue siendo retirar admisión y cerrar sesiones en el proveedor.

Las pruebas Rust también vinculan cookies de dos identidades con sus espacios: Alice lee su documento sintético y Bob recibe su propio espacio vacío, incluso enviando un header o query que declara a Alice. Las pruebas previas conservan la separación de revisiones y eventos.

Una comprobación local con Chrome y Playwright recorrió el diff local, login real por la pantalla de Keycloak, espacio privado y logout. Se inspeccionaron las vistas de escritorio y móvil de 390 px; no hubo errores JavaScript ni desbordamiento horizontal. Al cerrar sesión se vacían filas, rutas y nombre del usuario. Las capturas y el arnés de revisión visual permanecen ignorados bajo `target/`.

Una revisión independiente no encontró fallos bloqueantes en los límites de sesión, CSRF o SSE. Detectó que la prueba original mezclaba retirada del rol y logout administrativo; se separaron ambos casos antes de registrar estos resultados. El issue sigue abierto por Google/GitHub.
