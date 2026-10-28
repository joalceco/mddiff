# S05: pegar texto y subir Markdown en el visor alojado

Fecha: 2026-10-27. Issue: [#6](https://github.com/joalceco/mddiff/issues/6). Rama: `feature/multiuser-sources`.

Este documento registra S05 y la importación desde el navegador. La entrega posterior [S06: agentes remotos](remote-agents.md) incorpora propuestas por API y MCP con identidad de usuario.

Desde [S08: captura de Memos](memos-source.md), pegar o subir una propuesta utiliza `PATCH /api/work` para conservar la procedencia del original. Reemplazar el original manualmente mediante PUT elimina esa procedencia.

## Resultado y alcance

Una persona autenticada puede abrir el original y la propuesta desde las cabeceras del visor alojado mediante **Pegar texto** o **Subir .md**. La apertura y la comparación son automáticas. Se conserva el visor sin lateral y no se añade un botón para aplicar o comparar.

Estas entradas utilizan el [trabajo actual persistente](current-work.md): reemplazar un lado conserva el otro y guarda únicamente el par vigente de esa persona. No se crea un historial de importaciones ni se modifican los archivos seleccionados.

El modo local mantiene sus campos de rutas, API y MCP existentes. La importación alojada no abre rutas del host. Google/GitHub, Jira, Memos y el MCP alojado siguen pendientes; la API de trabajo utiliza la sesión web y CSRF, sin introducir un token de agente en esta historia.

La integración se verificó el 2026-10-27 con Playwright 1.62.1, Chrome y usuarios sintéticos de Keycloak 26.8.1. Los resultados están al final; Google/GitHub no forma parte de esta comprobación.

## Abrir un par de documentos

1. Entra en el visor alojado con una cuenta admitida.
2. En **Original**, selecciona **Pegar texto** o **Subir .md** y aporta el Markdown.
3. Espera el estado **Abierto y guardado**. Los controles de **Propuesta** se habilitan cuando existe un original, incluso si su contenido está vacío.
4. Aporta la propuesta desde su cabecera. El diff se muestra automáticamente.
5. Usa **Lectura**, **Comparación**, **Markdown** y **Fuente** para revisar el resultado.

La ruta elegida en el equipo no se envía como una ubicación que deba abrir el servidor. Un archivo aporta su nombre base como etiqueta; los textos pegados usan **Original pegado.md** o **Propuesta pegada.md**.

### Pegar texto

**Pegar texto** abre un textarea en la misma cabecera y pasa a llamarse **Cerrar texto**. Solo hay un campo de pegado activo: abrir el otro lo cierra. Mientras haya un cambio pendiente o guardándose, los controles de otros orígenes o del otro lado permanecen deshabilitados. No se solicita acceso programático al portapapeles; la persona pega mediante los controles habituales del navegador o del teclado.

El texto se abre y guarda después de una pausa de **350 ms** al escribir o pegar. El estado de la cabecera distingue contenido pendiente, guardado en curso, éxito y error. **Cerrar texto** guarda un cambio pendiente antes de ocultar el campo; no exige esperar esa pausa. Si el guardado falla, mantiene el campo abierto. Después de un error puedes copiar el borrador y cerrarlo para descartarlo o preparar un nuevo intento.

Si escribes mientras una solicitud está en curso, las ediciones se envían después con la revisión que confirmó la propia operación anterior. No se utiliza una actualización externa para forzar el guardado del borrador.

Un pegado completo en un campo vacío, o que sustituye todo su contenido seleccionado, conserva el Unicode y los saltos CRLF recibidos en el evento de pegado. Una edición manual o un pegado parcial pasa por el valor del textarea y puede normalizar saltos de línea a LF según el navegador y la plataforma. Para conservar con precisión los saltos de un archivo, usa **Subir .md**.

El Markdown vacío es válido. Borrar el contenido del campo no borra el trabajo completo: guarda un documento vacío en ese lado y conserva el otro. **Borrar trabajo** sigue siendo la acción explícita para eliminar ambos lados, con su confirmación cancelable.

### Subir un archivo

**Subir .md** abre el selector nativo. Se aceptan extensiones `.md` y `.markdown` sin distinguir mayúsculas y minúsculas; un archivo vacío es válido. Cada archivo debe ocupar como máximo **2 MiB** y contener UTF-8 válido.

El navegador lee el archivo mediante `File.arrayBuffer()` y lo decodifica con `TextDecoder` en [modo estricto](https://developer.mozilla.org/en-US/docs/Web/API/TextDecoder/fatal), [conservando el BOM](https://developer.mozilla.org/en-US/docs/Web/API/TextDecoder/ignoreBOM). No se sustituyen bytes inválidos por caracteres de reemplazo: se rechaza la entrada. Una decodificación válida conserva Unicode, BOM, saltos CRLF y la ausencia de salto final.

La etiqueta es el nombre base, con un máximo de 255 bytes UTF-8 y sin caracteres de control. No se conserva una ruta local del usuario ni se interpreta la etiqueta como una ruta del servidor.

Una extensión no admitida, UTF-8 inválido, un archivo excesivo o una etiqueta inválida produce un error visible y conserva el par válido anterior. Un fallo al leer el archivo se informa de forma distinta de un fallo de codificación. Para intentar otra entrada, vuelve a seleccionar el archivo correcto.

## Revisiones, errores y sesiones

Al abrir el campo o iniciar la selección de archivo se toma una revisión de referencia. La importación utiliza esa revisión para sustituir un lado mediante `PUT /api/work`, incluyendo el otro lado del mismo trabajo. La API confirma el cambio de forma durable antes de publicar el snapshot y el evento SSE.

Una actualización de otra pestaña puede cambiar la vista, pero no sustituye silenciosamente la revisión de un borrador pendiente. Si alguien modificó o borró el trabajo desde otra pestaña, la operación recibe `409` en lugar de sobrescribir el contenido nuevo.

| Caso | Qué ocurre y cómo continuar |
| --- | --- |
| Texto en conflicto | El visor recupera el trabajo vigente y conserva el borrador visible. Copia el texto que quieras conservar, cierra y vuelve a abrir el campo antes de volver a pegarlo. No hay reintento automático con una revisión nueva. |
| Archivo en conflicto | Se conserva el trabajo vigente. Vuelve a abrir el selector y selecciona el archivo para realizar un nuevo intento explícito. |
| Entrada inválida o fallo al guardar | La cabecera y la barra de error muestran el problema; el par válido anterior sigue disponible. El borrador de texto se mantiene mientras la sesión y el campo sigan abiertos. |
| Cierre o pérdida de sesión | Se vacían los borradores y el contenido del DOM y se cierra el SSE. Las respuestas tardías no vuelven a llenar la vista de una sesión terminada. |
| Desconexión durante una escritura autorizada | El servidor puede terminar un commit ya iniciado. Al volver a entrar, consulta el trabajo vigente antes de repetir la importación. |

La pérdida de sesión no implica borrar el trabajo guardado. Las sesiones y los borradores son temporales; el par persistente pertenece a la identidad verificada. Los límites del borrado físico y de las copias siguen siendo los de [S04](current-work.md#borrado-funcional-copias-y-retención).

Cada apertura de SSE vuelve a comprobar la sesión antes de procesar revisiones. Cambiar de cuenta desde otra pestaña también limpia el borrador anterior si ocurre durante la espera de reconexión; no se reutiliza la identidad de la vista anterior.

## Transporte y validación

El navegador transforma la entrada en `Document {label, markdown}` y utiliza los `GET` y `PUT /api/work` existentes con sesión, Origin y CSRF. No añade un endpoint de upload multipart ni crea un archivo temporal de importación en el servidor. El contenido sí se guarda en la base del trabajo actual después de una operación correcta.

El límite de cada Markdown es de 2 MiB en UTF-8, también para texto pegado. El margen de transporte de **24 MiB + 16 KiB** permite escapar dos documentos dentro del JSON; no aumenta el tamaño admitido por documento. La validación del navegador ayuda a informar errores y no sustituye las comprobaciones del backend.

Los controles están etiquetados por lado y son accesibles por teclado. El campo de texto conserva el comportamiento nativo de pegado y edición; los atajos de navegación de cambios no actúan mientras se escribe en él.

## Comprobar con un navegador real

La suite permanente vive en `tests/browser` y usa Playwright con Chromium como dependencia de desarrollo. **El binario y el despliegue de mddiff no requieren Node.js.** Para estas pruebas necesitas Node.js 20 o posterior con npm, Rust/Cargo, Python 3, Docker en ejecución y el puerto de la fixture Keycloak disponible.

Ejecuta desde la raíz del repositorio:

1. Instala las dependencias fijadas de la suite:

   ```sh
   npm ci --prefix tests/browser
   ```

2. Instala Chromium y sus dependencias de prueba:

   ```sh
   npx --prefix tests/browser playwright install --with-deps chromium
   ```

3. Inicia el proveedor sintético y espera a que esté disponible:

   ```sh
   python3 spikes/identity/keycloak.py up
   ```

4. Compila la aplicación principal:

   ```sh
   cargo build --release --locked
   ```

5. Ejecuta el runner de importaciones alojadas:

   ```sh
   python3 scripts/test_hosted_imports.py
   ```

Al terminar, retira únicamente el contenedor de la fixture:

```sh
python3 spikes/identity/keycloak.py down
```

La fixture y el runner utilizan usuarios, documentos y configuración de prueba; no requieren credenciales sociales ni documentos reales. Mantén los artefactos de ejecución fuera del contenido versionado, igual que en las integraciones de autenticación y persistencia.

Si ya tienes Chrome instalado, puedes usarlo con `MDDIFF_BROWSER_CHANNEL=chrome python3 scripts/test_hosted_imports.py`.

## Resultados de verificación

Corrida local del 2026-10-27: **35 pruebas Rust**, formato, Clippy y compilación `release` correctos. Las integraciones de autenticación, persistencia, modos y MCP local también pasaron. La nueva suite de navegador pasó con Playwright 1.62.1 y Chrome.

| Comprobación | Resultado |
| --- | --- |
| Entradas y fuente | Apertura automática por archivos y texto en ambos lados; conservación del otro documento; archivos vacíos; Unicode, BOM y CRLF exactos. Un evento de pegado completo también conserva CRLF. |
| Límites | Archivo de exactamente 2 MiB aceptado; exceso, UTF-8 inválido, extensión y nombres incorrectos rechazados sin escribir. Texto multibyte excesivo se rechaza según bytes UTF-8. |
| Guardado pendiente | Cerrar antes de la pausa guarda el texto; escribir B mientras se guarda A termina con B y dos revisiones consecutivas, sin perder el otro documento. |
| Conflictos y fallos | Conflictos desde API y otra pestaña conservan el par vigente y el borrador; no hay rebase automático. Cancelar el selector conserva el borrador. Un `500` inyectado mantiene texto y estado y permite corregir la entrada. |
| Sesión y privacidad | Dos cuentas conservan trabajos distintos. Rotar la sesión de Alice a Bob vacía filas y texto de la anterior, tanto con SSE activo como durante los tres segundos de reconexión. Una lectura de archivo que termina después del logout no envía un PUT tardío. |
| Visor | HTML inerte sin ejecución ni peticiones de imágenes; Fuente literal, temas, lectura/comparación, navegación y foco por teclado disponibles. |
| Móvil | Vistas de 390 px, con el campo cerrado y abierto, sin lateral ni desbordamiento horizontal. Se corrigió el ancho de la cabecera en modo claro. |

Las capturas revisadas contienen únicamente datos sintéticos y permanecen ignoradas bajo `target/browser/`. La revisión independiente detectó pérdida de un borrador al cancelar el selector y persistencia de un borrador al cambiar de cuenta; se corrigieron y se añadieron ambas regresiones antes de publicar.

GitHub Actions queda configurado para instalar la dependencia de desarrollo y Chromium y ejecutar esta suite con Keycloak sintético. La ejecución registrada fue local con Chrome; no se atribuye una corrida remota de CI ni validación en Safari, Firefox, Google/GitHub o producción.
