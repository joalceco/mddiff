# S14: operar la beta por invitación

Issue: [#15](https://github.com/joalceco/mddiff/issues/15). Rama: `feature/multiuser-sources`.

Esta guía describe la operación de una instancia alojada y sus límites. El runbook de la instalación, sus dominios, direcciones, accesos y archivos privados se conservan fuera del repositorio. Los comandos de las guías públicas utilizan ejemplos; no constituyen un instalador del cluster.

## Arquitectura y admisión

La beta ejecuta una instancia Rust con SQLite en disco local, Keycloak en modo de producción con PostgreSQL y un proxy HTTPS. El túnel verifica el certificado del origen con una CA privada; no se desactiva la validación TLS. El proxy presenta únicamente el visor, sus endpoints y las rutas de autenticación necesarias. La administración del IdP, su realm administrativo, PostgreSQL, métricas del IdP y puertos de la aplicación permanecen fuera del acceso público. Un filtro de entrada dentro del guest restringe el proxy y el exporter a sus consumidores; no depende de habilitar el firewall global del cluster.

El entorno de aplicación usa un contenedor de sistema sin privilegios, dos CPU y 4 GiB de RAM. mddiff conserva el límite de 512 MiB y una CPU de [S12](container-deployment.md); Keycloak tiene 2 GiB y PostgreSQL, 512 MiB. Estos límites dejan margen al sistema operativo y al proxy. El disco de trabajo es local; los respaldos usan otro disco. Una reserva virtual de disco no garantiza espacio físico: hay que observar también el almacenamiento del host.

Los invitados reciben una cuenta creada por administración, con contraseña temporal y cambio obligatorio en el primer acceso. No hay autorregistro. La cuenta debe tener el rol `mddiff-user`; una contraseña válida sin ese rol no permite leer el trabajo. No se envían invitaciones por correo automáticamente ni se presupone un servidor SMTP. Google/GitHub sigue pendiente.

Para admitir a una persona:

1. Crea su usuario desde el acceso privado de administración, con una contraseña temporal única.
2. Asígnale el rol `mddiff-user` y verifica que el registro abierto permanece deshabilitado.
3. Entrega el acceso por un canal privado autorizado; no guardes la contraseña en notas o issues.
4. Comprueba el cambio de contraseña y que el espacio nuevo esté vacío.
5. Para revocar acceso, deshabilita la cuenta o retira el rol y cierra sus sesiones. La aplicación vuelve a comprobar la identidad en solicitudes protegidas y SSE.

Las conexiones personales se autorizan por separado. Habilitar un destino Memos no importa notas ni copia credenciales desde otro servicio. En la instalación del 2026-10-28, la lectura de Jira y Google/GitHub conservaron sus historias pendientes. [S09](jira-source.md) registra el avance posterior de Jira; Google/GitHub sigue pendiente.

Al 2026-10-29, Jira aún no tiene una lectura real verificada: falta configurar un secreto OAuth válido del operador. La cuenta propietaria puede probar la aplicación privada; autorizar otras cuentas requiere **Sharing**, que permanece deshabilitado por [#17](https://github.com/joalceco/mddiff/issues/17). La consola exige una atestación sobre Personal Data Reporting API para aplicaciones que almacenan datos personales y mddiff aún no implementa esa API. No se ha emitido esa atestación ni se considera resuelto el requisito mediante este runbook.

## Trabajo actual y siete copias diarias

La aplicación mantiene únicamente el último original y la última propuesta por persona. **La instalación beta conserva hasta siete copias diarias exitosas cifradas.** Una copia puede recuperar documentos reemplazados o borrados después de su captura; borrar el trabajo actual no elimina de inmediato esas copias. El timer crea una copia al día; una ejecución manual también ocupa uno de los siete lugares. Si se omiten días por un fallo, una copia recuperable puede tener más de siete días.

La aplicación y Compose no programan backups por sí mismos. En la instalación se configura un timer externo, con esta secuencia:

1. Activa mantenimiento y detén las escrituras del visor y del IdP.
2. Crea una copia consistente de SQLite mediante su API de backup y un `pg_dump` de Keycloak; conserva configuraciones, claves externas, certificados y referencia inmutable de la imagen. Cierra explícitamente las conexiones SQLite antes de reiniciar.
3. Reinicia IdP y visor, comprueba salud y restablece el acceso anterior. Un fallo de recuperación mantiene el visor en mantenimiento.
4. Cifra la copia con restic, verifica el repositorio y solo entonces aplica `forget --keep-last 7 --group-by '' --prune` en el repositorio exclusivo de la beta.
5. Publica resultado y fecha de éxito en las métricas. Un fallo no reemplaza la fecha del último respaldo correcto.

El respaldo interrumpe brevemente las sesiones; después de reiniciar el visor se requiere autenticación. La contraseña de restic tiene una copia privada fuera del servidor y fuera del repositorio de respaldos. Perderla hace irrecuperables las copias. Se debe recuperar también la clave AES externa: conservar SQLite sin esa clave no recupera las credenciales de conexiones.

El almacenamiento de respaldos está en otro disco del mismo host. Protege frente a la pérdida del volumen de aplicación, pero **no constituye una copia fuera del sitio ni una garantía ante la pérdida completa del host y sus discos**. La beta tiene una instancia; un fallo del nodo interrumpe el servicio.

Consulta la [API de backup de SQLite](https://sqlite.org/backup.html), [`pg_dump`](https://www.postgresql.org/docs/17/app-pgdump.html) y la [política de retención de restic](https://restic.readthedocs.io/en/stable/060_forget.html). No copies únicamente el archivo SQLite mientras el escritor esté abierto ni borres WAL/SHM para forzar la recuperación.

## Restauración y rollback

Para una restauración, cierra primero el acceso y conserva el estado existente. Verifica y descifra una copia elegida, recupera SQLite, la base del IdP, configuraciones y claves, y utiliza una imagen compatible con el esquema. Conserva permisos y propietarios: UID `10001` para mddiff, UID del servicio para cada archivo privado del IdP y PostgreSQL. Verifica login con identidades recuperadas y el trabajo exacto de dos usuarios antes de reabrir.

El manifiesto de la entrega fija commit, identidad de imagen y esquema SQLite. Antes de actualizar, conserva la imagen conocida y un checkpoint coherente. Un despliegue fallido debe poder volver a esa imagen y su configuración sin borrar el directorio de trabajo.

El ensayo del 2026-10-28 recuperó la imagen verificada tras un candidato que no arranca, con almacenamiento de esquema 3. No demostró un cambio entre esquemas. El ensayo del 2026-10-29 migró de 3 a 4, restauró únicamente la aplicación y su checkpoint compatible con 3 y repitió la migración a 4, preservando IdP y PostgreSQL. Esto no es una conversión de 4 a 3 en el mismo archivo ni admite reducir manualmente `user_version`.

El binario anterior no puede abrir los datos migrados a 4. Restaurar un checkpoint vuelve al estado de la copia y no conserva escrituras posteriores. Mantén mantenimiento durante la actualización y sus verificaciones. Después de reabrir el servicio y aceptar escrituras públicas, no recuperes el checkpoint anterior como un rollback rutinario: perdería esas escrituras.

Un rollback limitado al visor debe conservar el IdP y su PostgreSQL vigentes. Restaurarlos desde una copia anterior podría revertir cambios de contraseña o sesiones que ocurrieron durante el mantenimiento de la aplicación. Una recuperación completa del IdP exige cerrar también sus escrituras y revisar esa consecuencia.

## Observabilidad sin documentos

| Señal | Verificación |
| --- | --- |
| Salud y acceso público | Sondas HTTPS con TLS verificado: salud `204`, discovery del IdP `200`, API/MCP anónimos `401` y administración pública `404`. |
| Consumo | Estado, memoria, límite, CPU, reinicios y OOM por contenedor; espacio disponible del volumen y del host. |
| Respaldo y TLS | Último resultado y antigüedad del respaldo; fecha de caducidad del certificado de origen y timer de renovación. |
| Errores y cuotas | Conteos `429` y `5xx` y latencia p95 sobre las últimas 1000 respuestas del proxy. Es una ventana reciente, no un contador histórico completo ni una medida de disponibilidad. |
| Logs | Solo fecha, categoría fija de ruta, estado, duración y bytes; sin URI, query, IP, headers, cookies, tokens o Markdown. Rotación limitada. |

Un dashboard privado reúne estas señales. No se configuraron envíos de alertas a personas. Revisa un respaldo de más de 26 horas, un certificado con menos de 14 días, contenedores caídos, memoria sostenida cerca del límite y espacio físico insuficiente. La salud del visor no sustituye una prueba de login ni comprueba continuamente al IdP.

## Evidencia de instalación

### Instalación histórica del 2026-10-28

Verificado el **2026-10-28** en Linux amd64, con la aplicación del commit `2dcede7` y esquema SQLite 3. La instalación y las credenciales permanecen en el runbook privado; no se incorporaron al repo ni a la imagen.

| Comprobación | Resultado |
| --- | --- |
| HTTPS y admisión | HTTP redirige a HTTPS; dos invitados acceden y una cuenta sin rol queda bloqueada. Cookies seguras, API/MCP anónimos `401`, administración pública `404` y cinco sondas HTTPS correctas. |
| Cliente real | Codex CLI 0.163.0 completó OAuth PKCE, descubrimiento de herramientas, propuestas, SSE y rechazo de revisiones obsoletas con dos cuentas; cero turnos de modelo. Chrome verificó login, comparación, contenido exacto, importación y destino personal, sin errores JavaScript. |
| Recuperación | Restic verificó todos los datos y restauró el respaldo cifrado. Después de borrar ambos trabajos sintéticos se recuperaron sus pares exactos, cuentas y claves de firma del IdP. La clave externa recuperada descifró un canario AES-GCM. |
| Ciclo de vida | Reiniciar el contenedor de sistema conserva los trabajos. Un candidato que falla al arrancar vuelve a la imagen verificada; una copia con esquema futuro 4 se rechaza sin reducir el número de esquema ni modificar los datos activos. |
| Operación | Timer diario, cifrado y límite de siete copias, métricas privadas, revisión de logs y renovación del certificado de origen comprobados. Una invitación nueva obligó a cambiar contraseña y abrió un espacio vacío. Las cuatro cuentas de prueba se retiraron. |

El respaldo coherente completo medido tardó aproximadamente **25 segundos**, incluidos reinicio y verificación; no es una garantía para volúmenes mayores. La primera ejecución detectó una conexión SQLite sin cerrar: se corrigió antes de publicar, y los respaldos y la restauración posteriores pasaron.

Las suites de código y capacidad previas están en [S13](capacity-validation.md); sus diez sesiones sintéticas no equivalen a una prueba prolongada de diez navegadores a través de internet. La evidencia del 2026-10-28 no valida login social, proveedores personales reales, alta disponibilidad ni cambios entre esquemas.

### Actualización del 2026-10-29

La beta ejecuta el commit `6bfccea841530e4cf8a49fc5ab7b1e5d9f670070`, con esquema SQLite 4. Esta actualización incorpora el selector y la procedencia de orígenes, la conversión ADF de Jira y el vínculo entre sesión y eventos de [S11](coordination.md). [#11](https://github.com/joalceco/mddiff/issues/11) y [#12](https://github.com/joalceco/mddiff/issues/12) están cerrados.

| Comprobación | Resultado registrado |
| --- | --- |
| CI | [Corrida 40904080796](https://github.com/joalceco/mddiff/actions/runs/40904080796): jobs `check` y `container` correctos, incluidas 70 pruebas Rust, regresiones y capacidad de diez usuarios sintéticos. |
| Migración y rollback | SQLite 3→4, rollback de aplicación a su checkpoint de esquema 3 y nueva migración a 4 correctos. Los hashes de los registros anteriores coinciden. Keycloak y PostgreSQL permanecieron vigentes durante el ensayo. |
| Cliente público | Codex real completó OAuth, API/MCP, procedencia, SSE y conflictos de revisión por HTTPS público con dos cuentas sintéticas y cero turnos de modelo sobre la imagen publicada `6bfccea`. |
| Operación | Cinco sondas públicas correctas, métricas comprobadas y timer de respaldos activo. Se conserva la política de hasta siete copias cifradas exitosas. |
| Persistencia nueva | Recuperación del trabajo nuevo y sus metadatos verificada en `6bfccea`. La conservación de registros previos durante la migración también está verificada. |

La limpieza posterior borró los documentos sintéticos y retiró únicamente las tres cuentas creadas para esta prueba; el propietario permaneció intacto. El respaldo final cifrado se recuperó desde restic y se verificaron manifiesto, commit, imagen, esquema 4 e integridad SQLite. Se retiraron las copias temporales del checkpoint; las copias cifradas y los datos vigentes se conservaron. El monitor confirmó las cinco sondas HTTPS y la recolección de métricas con muestras actuales.

La lectura real de [Jira #10](https://github.com/joalceco/mddiff/issues/10), la integración personal real de Memos y Google/GitHub siguen pendientes. [#17](https://github.com/joalceco/mddiff/issues/17) bloquea habilitar Sharing para otras cuentas de Atlassian. Los resultados de proveedores sintéticos no sustituyen esas validaciones, y la documentación pública no contiene accesos, secretos ni documentos privados.
