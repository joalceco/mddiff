# Experimento de identidad: Keycloak y Codex

Prototipo del [issue #2](https://github.com/joalceco/mddiff/issues/2). Comprueba OIDC web y MCP con Keycloak real y el cliente Codex real, usando únicamente usuarios sintéticos. No requiere credenciales OpenAI ni ejecuta turnos de modelo.

La aplicación principal y su puerto 3847 no cambian. Este paquete Rust es independiente: no modifica las dependencias del lector ni añade autenticación al producto existente.

## Requisitos

- Docker en ejecución y capacidad disponible para un contenedor con límite de 768 MiB y una CPU.
- Rust/Cargo, Python 3 y Codex CLI; la corrida registrada usó Rust 1.95.0, Python 3.14 y Codex 0.163.0 en macOS arm64.
- Puertos locales libres: 8848 para Keycloak, 3850 para el gateway y 43210 para el callback OAuth de Codex.

No instalar este prototipo en un servidor público. Usa `start-dev`, HTTP de loopback, usuarios con contraseñas de prueba, almacenamiento en memoria y registro de clientes de prueba con password grant. El cliente web y el cliente Codex sí exigen Authorization Code + PKCE S256; no tienen password grant habilitado.

## Reproducir

Ejecuta los comandos desde la raíz del repositorio.

1. Crea la instancia sintética y espera el mensaje de disponibilidad:

   ```sh
   python3 spikes/identity/keycloak.py up
   ```

2. Inicia el gateway Rust, espera el mensaje **Identity experiment** y déjalo abierto:

   ```sh
   cargo run --manifest-path spikes/identity/Cargo.toml --locked
   ```

3. En otra terminal, ejecuta las comprobaciones web/API y los casos de rechazo:

   ```sh
   python3 spikes/identity/test_spike.py
   ```

4. Ejecuta el cliente Codex real con dos identidades:

   ```sh
   python3 spikes/identity/codex_probe.py
   ```

5. Detén el gateway con `Ctrl-C` y retira únicamente el contenedor del experimento:

   ```sh
   python3 spikes/identity/keycloak.py down
   ```

El comando `up` crea un entorno nuevo. Si ya existe, pide ejecutar `down` para evitar confundir estado anterior con una corrida limpia. Las credenciales de prueba se generan aleatoriamente en `target/identity-spike/runtime.json`, con permisos `0600`, y nunca se imprimen. Los archivos de ejecución quedan ignorados por Git. La plantilla privada `social.env` está reservada para una prueba posterior; este experimento no la consume ni configura Google/GitHub.

El harness Codex usa directorios `CODEX_HOME` temporales fuera del repositorio y almacenamiento OAuth en archivo dentro de ellos. Se eliminan al terminar; no modifica la configuración global ni el Keychain. La automatización de formularios acepta únicamente el issuer sintético de localhost.

## Qué valida cada componente

| Componente | Evidencia |
| --- | --- |
| `keycloak.py` | Keycloak 26.8.1, realm propio y otro realm para probar emisor incorrecto; Alice/Bob admitidos y Mallory sin admisión. |
| Gateway Rust | Discovery OIDC, PKCE, estado ligado a cookie, nonce/firma/audiencia del ID token, hash del access token y autorización por introspección. |
| `test_spike.py` | Acceso anónimo, admisión, scopes, emisor/audiencia, caducidad, revocación, PKCE, sesión web, aislamiento y conflictos. |
| `codex_probe.py` | OAuth del app-server, descubrimiento, llamada real a `open_comparison`, propietarios separados y rechazo de revisiones antiguas; cero turnos de modelo. |

El estado sintético vive en memoria y se muestra en `http://127.0.0.1:3850/api/state` tras autenticarse. `open_comparison` almacena los textos de prueba y devuelve propietario/revisión. No ejecuta el motor de diff ni representa el futuro visor multiusuario: esta prueba valida el límite de identidad.

### Pruebas del paquete

```sh
cargo fmt --manifest-path spikes/identity/Cargo.toml --check
cargo clippy --manifest-path spikes/identity/Cargo.toml --locked --all-targets -- -D warnings
cargo test --manifest-path spikes/identity/Cargo.toml --locked
```

Los resultados y límites están en [el informe del experimento](../../docs/planning/identity-spike.md).
