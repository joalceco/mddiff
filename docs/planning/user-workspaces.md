# S02: estado por usuario y separación de modos

Fecha: 2026-10-27. Issue: [#3](https://github.com/joalceco/mddiff/issues/3). Rama: `feature/multiuser-sources`.

Este documento registra el estado al terminar S02. La integración posterior de autenticación en el producto se describe en [S03: autenticación web alojada](hosted-auth.md); la entrada de propuestas mediante Bearer y MCP remoto, en [S06: agentes remotos](remote-agents.md).

## Resultado

El lector local conserva CLI, API, MCP y el formato de sus snapshots. El motor de diff no cambia. La lectura de archivos está separada del servicio de comparación y el estado deja de pertenecer a toda la aplicación: snapshot, bloqueo de actualización y canal de revisiones pertenecen a cada espacio.

El modo alojado se incorpora como base cerrada. Todavía no existe un adaptador de autenticación conectado al producto: las operaciones protegidas responden `401` cuando no hay identidad de confianza. Google/GitHub continúa pendiente; el prototipo de identidad del issue #2 sigue separado.

## Límites de los componentes

| Componente | Responsabilidad |
| --- | --- |
| `LocalReader` | Posee una raíz canonicalizada y el espacio local. Es el único adaptador de aplicación que abre archivos. |
| `Document` | Contenido Markdown ya leído y una etiqueta; la etiqueta nunca se interpreta como una ruta por el servicio de comparación. |
| `Workspace` | Conserva y reemplaza el snapshot, serializa sus aperturas y emite sus propias revisiones. |
| `Workspaces` | Obtiene o crea atómicamente el espacio asociado a la identidad. Libera el bloqueo del registro antes de leer o calcular el diff. |
| `VerifiedIdentity` | Par `issuer` + `subject`, con campos privados y sin deserialización desde solicitudes. Solo debe construirse después de verificar credenciales. |

La identidad tipada expresa un límite de confianza; su constructor no autentica tokens. La historia de autenticación debe implementar el adaptador que verifica credenciales y la inserta en las extensiones de la solicitud. Ningún header, cookie o campo del cuerpo se usa actualmente para construirla.

La librería Rust permite reutilizar el servicio de comparación con contenido y un espacio ya vinculado a un propietario. El binario mantiene los adaptadores HTTP/MCP locales. El servicio de comparación no recibe una raíz o rutas que abrir.

## Selección de modo

| Modo | CLI y rutas |
| --- | --- |
| `local` o sin `--mode` | Acepta `--root` y cero o dos archivos iniciales. Mantiene aperturas por API/MCP y escucha en loopback. |
| `hosted` | Rechaza `--root` incluso si se proporciona explícitamente como `.` y rechaza archivos posicionales. No construye `LocalReader`. |

La selección solo ocurre al arrancar. Una solicitud no puede cambiar el modo ni activar capacidades locales. En hosted no se registran las rutas de apertura/comparación por archivos ni `/mcp`; responden `404`. `/api/state` y `/api/events` necesitan la identidad tipada y responden `401` cuando falta. Se conservan los controles de Host, Origin, solicitudes entre sitios y tamaño.

El modo alojado sigue limitado a loopback. No incorpora una cuenta de prueba seleccionable por CLI, un header de identidad, un token compartido o un acceso anónimo a contenido para poder usarlo antes de implementar autenticación.

## Orden de las aperturas y aislamiento

La lectura local permanece dentro del bloqueo del espacio hasta completar el commit, igual que antes. Extraer el lector no mueve la lectura fuera de ese bloqueo ni permite que un resultado anterior y lento sustituya una apertura posterior ya completada.

Cada propietario tiene un bloqueo distinto. Una apertura de Alice no bloquea la de Bob; sus snapshots, contadores y suscriptores tampoco se comparten. El mismo sujeto emitido por dos emisores diferentes obtiene espacios diferentes. Las aperturas fallidas conservan el snapshot y la revisión anteriores.

No se añade control optimista de revisión a los contratos locales existentes: la coordinación entre agentes y pestañas y su contrato alojado se desarrolla en las historias correspondientes. Tampoco se añade persistencia; el estado continúa en memoria hasta S04.

## Verificación

Corrida del 2026-10-27: **23 pruebas Rust**, formato, Clippy y compilación `release` correctos. Ambas integraciones, MCP local y separación de modos, terminaron correctamente. Una revisión independiente de los componentes y sus callers no encontró regresiones bloqueantes.

Las pruebas Rust conservan los casos anteriores del motor, lectura y errores. Añaden comparaciones reales de dos propietarios, separación por emisor/sujeto, estabilidad del espacio para la misma identidad, bloqueo independiente, canales separados y conservación del estado ante contenido excesivo.

Una prueba de los handlers HTTP inserta identidades sintéticas mediante una capa Rust de prueba. Comprueba que la identidad del contexto decide el espacio aunque la solicitud declare otro usuario; estado y SSE quedan separados. Sin esa capa, los mismos handlers rechazan identidades inventadas con `401`.

`scripts/test_modes.py` usa procesos reales con puertos temporales y archivos sintéticos. Comprueba CLI local y hosted, rechazo de argumentos de archivos, endpoints locales ausentes, suplantación mediante headers/query, controles HTTP y archivos intactos. `scripts/test_mcp.py` conserva la integración del lector y MCP locales.

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
python3 scripts/test_mcp.py
python3 scripts/test_modes.py
```

GitHub Actions incluye ambas integraciones. No se despliega ni se conecta el proveedor de identidad en esta historia; tampoco se añaden conectores, cambios de interfaz o un nuevo algoritmo de diff.
