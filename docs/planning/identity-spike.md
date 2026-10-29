# S01: prueba de Keycloak y Codex

Fecha: 2026-10-27. Issue: [#2](https://github.com/joalceco/mddiff/issues/2). Rama: `feature/multiuser-sources`.

Estado: validación sintética de OIDC web y MCP. **Google/GitHub quedan pendientes por decisión explícita del usuario.** El issue no se cierra como validación completa de login social o beta pública.

## Decisión provisional

Keycloak sigue siendo el primer candidato. La prueba demuestra que puede autenticar el gateway Rust y un cliente Codex real, con autorización y estado separados por usuario. No se encontró un bloqueo que exija cambiar de proveedor para este alcance sintético.

No se modificó la aplicación principal. El prototipo es un paquete aislado en [spikes/identity](../../spikes/identity/README.md), con almacenamiento de prueba en memoria. La persistencia del trabajo, los conectores y la migración del visor siguen perteneciendo a sus historias.

## Entorno y alcance

| Componente | Versión o condición |
| --- | --- |
| Keycloak | `quay.io/keycloak/keycloak:26.8.1`, publicado el 2026-10-13. |
| Imagen descargada | Digest `sha256:a78d6b79572168cb25886b80a8b5b8d4ed4390743e3eecb810538683e209dd6b`. |
| Gateway | Rust 1.95.0, `openidconnect` 4.0.1, `rmcp` 3.7.0; dependencias fijadas en el lockfile del prototipo. |
| Cliente | Codex CLI 0.163.0, app-server con llamadas MCP directas, sin turnos de modelo. |
| Equipo | macOS arm64; Docker sobre VM local de dos CPU y 2 GiB. |
| Exposición | Loopback exclusivamente; sin despliegue del cluster, túneles o cuentas reales. |

La fixture genera credenciales aleatorias y privadas para Alice, Bob y Mallory. Alice/Bob reciben el rol `mddiff-user`; Mallory puede autenticarse ante el proveedor, pero no usar mddiff. Ninguna identidad de prueba recibe admisión por coincidencia de email.

## Resultados verificados

| Caso | Resultado |
| --- | --- |
| Login web Authorization Code + PKCE | Sesión correcta; firma, emisor, audiencia, nonce y estado verificados con la biblioteca OIDC. |
| Misma identidad web/MCP | Ambos caminos resuelven el `sub` del usuario en el único issuer autorizado. |
| Codex OAuth | Discovery, cliente prerregistrado, callback fijo, scopes y PKCE S256 completados. |
| MCP real | Codex descubre y llama `open_comparison` para Alice y Bob; no se simula el cliente MCP. |
| Aislamiento | Cada cuenta conserva su propietario y revisión; escribir en la otra no altera su estado. |
| Actualización obsoleta | Rechazada, conservando la revisión vigente. |
| Acceso anónimo | API/MCP devuelven `401`; MCP anuncia metadatos del recurso protegido. |
| Usuario sin admisión | Token válido de Mallory rechazado con `403`, también en el transporte MCP. |
| Scope insuficiente | `403`. |
| Audiencia incorrecta | `401`. |
| Emisor incorrecto | Token emitido por otro realm real rechazado con `401`. |
| Token inválido o caducado | `401`. La expiración se prueba con un cliente sintético de vida corta. |
| PKCE omitido | Keycloak rechaza la autorización por falta de `code_challenge`. |
| Identificador de propietario inyectado | Rechazado por el esquema; no selecciona el espacio de otra persona. |
| CSRF de la sesión web | Callback con estado ajeno y escritura por cookie sin Origin permitido rechazados. |

La suite contiene ocho pruebas integradas contra Keycloak y el gateway, y una prueba Rust de los requisitos de autorización. El harness adicional de Codex comprueba dos clientes OAuth simultáneos, llamadas reales y conflictos. Formato y Clippy del paquete pasan sin advertencias.

## Ajustes encontrados durante la prueba

### Audiencia e introspección de Keycloak

El cliente confidencial que consulta introspección debe estar incluido en la audiencia del access token. La fixture usa dos mappers independientes: uno incluye el cliente introspector y otro la URI del recurso MCP. En 26.8.1, poner ambas opciones en un único mapper no añade ambas audiencias: la de cliente tiene prioridad.

El gateway exige además issuer, sujeto, URI de audiencia, scope y rol. No se deshabilitó la comprobación de audiencia del proveedor para hacer pasar la prueba. La definición explícita de scopes incluye el mapper de `sub`; un token activo sin propietario se rechaza.

Fuentes: [administración de audiencias](https://www.keycloak.org/docs/latest/server_admin/index.html#_audience) y [implementación 26.8.1 de AudienceProtocolMapper](https://github.com/keycloak/keycloak/blob/26.8.1/services/src/main/java/org/keycloak/protocol/oidc/mappers/AudienceProtocolMapper.java).

### Discovery y configuración de Codex

En la corrida con Codex 0.163.0, configurar `oauth_resource` explícitamente mientras el servidor también lo anuncia mediante Protected Resource Metadata produjo dos parámetros `resource` iguales. Keycloak rechazó esa autorización con `invalid_request: duplicated parameter`.

La solución probada fue omitir el valor redundante del TOML temporal y usar discovery. El harness sigue exigiendo el recurso esperado, el cliente prerregistrado, el callback fijo, PKCE y scopes. No modifica a mano la URL de autorización ni elimina la validación del recurso.

El cliente usa `mcpServer/oauth/login`, inventario MCP y `mcpServer/tool/call` del app-server. No requiere una llamada a un modelo para demostrar la interoperabilidad.

Fuentes: [MCP en Codex](https://learn.chatgpt.com/docs/extend/mcp?surface=cli), [app-server](https://learn.chatgpt.com/docs/app-server) y [autorización MCP](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization).

### Cookies del arnés local

Keycloak emitió cookies `Secure` en su origen HTTP de localhost. El `CookieJar` estándar de Python las almacenaba pero no las devolvía por HTTP; el formulario fallaba con cookie de reinicio ausente.

El harness emula la excepción de localhost únicamente para el origen exacto de la fixture, su dominio y nombres de cookies Keycloak. No modifica cookies del servidor ni permite devolver cookies seguras a destinos arbitrarios. Hay una prueba de los límites de esa excepción. El servicio real debe usar HTTPS y no necesita esta excepción del cliente sintético.

## Admisión, revocación y comparación con authentik

La admisión probada es una **aprobación administrativa del `sub` mediante rol**. No es una invitación por correo de un solo uso ni una prueba de enrolamiento social. Desactivar `registrationAllowed` no basta para impedir crear identidades mediante un broker social.

Con los mappers e introspección probados, retirar el rol dio `403` para el token anterior. Cerrar las sesiones del usuario dio `401` y la renovación del token quedó rechazada. Estos son resultados de esta configuración; no se extrapolan a validación JWT sin consulta al proveedor. La política recomendada para retirar acceso es quitar admisión y cerrar sesiones.

| Punto | Keycloak | authentik |
| --- | --- | --- |
| Gateway OIDC + Codex | Probado con usuarios sintéticos. | No ejecutado; no se atribuye compatibilidad empírica. |
| Admisión elegida | Rol asignado a identidad concreta; acceso sin rol rechazado. | Puede usar políticas y flujos de enrolamiento. |
| Invitación previa de un solo uso | No implementada ni probada en este experimento. | Tiene una etapa nativa documentada de invitación; la integración social concreta todavía necesitaría prueba. |
| Coste operativo | Instancia IdP y almacenamiento propios; producción exige configuración distinta de `start-dev`. | Sus servicios y PostgreSQL también requieren operación; no elimina el aislamiento dentro de mddiff. |

La diferencia de invitaciones justifica mantener authentik como alternativa si la invitación previa resulta un requisito estricto. El problema de `resource` se resolvió en la configuración del cliente, por lo que no justifica por sí solo cambiar de IdP.

Fuentes: [administración de Keycloak](https://www.keycloak.org/docs/latest/server_admin/index.html), [API administrativa](https://www.keycloak.org/docs-api/latest/rest-api/index.html#_users), [invitaciones authentik](https://docs.goauthentik.io/add-secure-apps/flows-stages/stages/invitation/) y [operación authentik](https://docs.goauthentik.io/install-config/install/docker-compose/).

## Recursos observados

Dos muestras locales de Docker durante el experimento mostraron aproximadamente **562–583 MiB** para Keycloak, con límite de **768 MiB** y una CPU asignada. Una muestra del gateway Rust mostró **18 368 KiB de RSS**. Son muestras del experimento, no máximos, una prueba de diez usuarios o una recomendación de producción. La guía oficial menciona un límite de 2 GB para un despliegue pequeño de producción; también requiere revisar su base de datos y recursos externos.

Fuente: [memoria del contenedor Keycloak](https://www.keycloak.org/server/containers#_specifying_different_memory_settings).

## Datos, backups y límites pendientes

El experimento guarda el trabajo en memoria y no resuelve la historia de persistencia. Mantiene cerrada la decisión de producto: un solo par activo por persona, sin biblioteca o versiones recuperables.

Para la beta hay que decidir si el trabajo activo entra en backups y por cuánto tiempo. Excluirlo permite no conservar copias antiguas, pero implica perderlo si se pierde el volumen; incluirlo exige declarar retención y eliminación de copias. No se promete borrado inmediato de SQLite/WAL o backups. Cuentas, conexiones, claves e IdP necesitan su propia recuperación.

No se considera aprobado todavía: login Google/GitHub, enrolamiento y vinculación social sin fusión por email, invitación previa, recursos de producción, persistencia, política definitiva de copias ni despliegue público. El prototipo y las evidencias no contienen credenciales, configuración privada del cluster o documentos reales.
