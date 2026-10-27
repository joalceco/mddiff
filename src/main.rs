mod auth;
mod mcp;

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{
        Html, IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use futures_util::stream;
use mddiff::{
    AppError,
    local::LocalReader,
    workspace::{Document, Snapshot, VerifiedIdentity, Work, Workspace, Workspaces},
};
use rmcp::schemars;
use serde::Deserialize;
use std::{convert::Infallible, env, ffi::OsString, path::PathBuf, sync::Arc, time::Duration};
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'none'; font-src 'self'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
const CONTENT_BODY_LIMIT: usize = 24 * 1024 * 1024 + 16 * 1024;

enum Mode {
    Local(Arc<LocalReader>),
    Hosted(Workspaces),
}

struct App {
    port: u16,
    mode: Mode,
    auth: Option<Arc<auth::Authenticator>>,
}

impl App {
    fn local_reader(&self) -> Result<&Arc<LocalReader>, AppError> {
        match &self.mode {
            Mode::Local(reader) => Ok(reader),
            Mode::Hosted(_) => Err(AppError(
                StatusCode::NOT_FOUND,
                "Las rutas locales no están disponibles en modo alojado.".into(),
            )),
        }
    }

    async fn workspace(
        &self,
        identity: Option<Extension<VerifiedIdentity>>,
    ) -> Result<Arc<Workspace>, AppError> {
        match &self.mode {
            Mode::Local(reader) => Ok(reader.workspace()),
            Mode::Hosted(store) => {
                let Extension(identity) = identity.ok_or_else(|| {
                    AppError(
                        StatusCode::UNAUTHORIZED,
                        "El modo alojado requiere una identidad autenticada.".into(),
                    )
                })?;
                store.for_user(&identity).await
            }
        }
    }
}

fn validate_request(method: &Method, headers: &HeaderMap, port: u16) -> Result<(), AppError> {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if host != format!("127.0.0.1:{port}") && host != format!("localhost:{port}") {
        return Err(AppError(
            StatusCode::FORBIDDEN,
            "Host no autorizado.".into(),
        ));
    }
    if let Some(origin) = headers.get(header::ORIGIN)
        && origin.to_str().ok() != Some(format!("http://{host}").as_str())
    {
        return Err(AppError(
            StatusCode::FORBIDDEN,
            "Origen no autorizado.".into(),
        ));
    }
    if headers
        .get("sec-fetch-site")
        .is_some_and(|value| value == "cross-site")
    {
        return Err(AppError(
            StatusCode::FORBIDDEN,
            "Solicitud entre sitios no autorizada.".into(),
        ));
    }
    require_json_body(method, headers)
}

fn require_json_body(method: &Method, headers: &HeaderMap) -> Result<(), AppError> {
    if matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) {
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .unwrap_or("")
            .trim();
        if !content_type.eq_ignore_ascii_case("application/json") {
            return Err(AppError(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Usa Content-Type: application/json.".into(),
            ));
        }
    }
    Ok(())
}

async fn security(
    State(app): State<Arc<App>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let validation = if let Some(auth) = &app.auth {
        auth.validate_boundary(request.method(), request.uri().path(), request.headers())
    } else {
        validate_request(request.method(), request.headers(), app.port)
    };
    let mut response = match validation {
        Ok(()) => next.run(request).await,
        Err(error) => error.into_response(),
    };
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenRequest {
    path: PathBuf,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CompareRequest {
    /// Ruta del Markdown original, relativa a la carpeta autorizada o absoluta dentro de ella.
    base_path: PathBuf,
    /// Ruta del Markdown propuesto, relativa a la carpeta autorizada o absoluta dentro de ella.
    candidate_path: PathBuf,
}

fn json_payload<T>(payload: Result<Json<T>, JsonRejection>) -> Result<T, AppError> {
    payload.map(|Json(value)| value).map_err(|error| {
        AppError(
            error.status(),
            format!("Solicitud JSON inválida: {}", error.body_text()),
        )
    })
}

async fn get_state(
    State(app): State<Arc<App>>,
    identity: Option<Extension<VerifiedIdentity>>,
) -> Result<Json<Arc<Snapshot>>, AppError> {
    Ok(Json(app.workspace(identity).await?.snapshot().await))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceWork {
    expected_revision: u64,
    base: Document,
    candidate: Option<Document>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClearWork {
    expected_revision: u64,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProposeRequest {
    expected_revision: u64,
    candidate: Document,
}

async fn propose_work(
    State(app): State<Arc<App>>,
    identity: Option<Extension<VerifiedIdentity>>,
    payload: Result<Json<ProposeRequest>, JsonRejection>,
) -> Result<Json<Arc<Snapshot>>, AppError> {
    let input = json_payload(payload)?;
    app.workspace(identity)
        .await?
        .propose_if_revision(input.expected_revision, input.candidate)
        .await
        .map(Json)
}

async fn get_work(
    State(app): State<Arc<App>>,
    identity: Option<Extension<VerifiedIdentity>>,
) -> Result<Json<Arc<Work>>, AppError> {
    Ok(Json(app.workspace(identity).await?.work().await))
}

async fn replace_work(
    State(app): State<Arc<App>>,
    identity: Option<Extension<VerifiedIdentity>>,
    payload: Result<Json<ReplaceWork>, JsonRejection>,
) -> Result<Json<Arc<Snapshot>>, AppError> {
    let request = json_payload(payload)?;
    app.workspace(identity)
        .await?
        .replace_if_revision(request.expected_revision, request.base, request.candidate)
        .await
        .map(Json)
}

async fn clear_work(
    State(app): State<Arc<App>>,
    identity: Option<Extension<VerifiedIdentity>>,
    payload: Result<Json<ClearWork>, JsonRejection>,
) -> Result<Json<Arc<Snapshot>>, AppError> {
    let request = json_payload(payload)?;
    app.workspace(identity)
        .await?
        .clear_if_revision(request.expected_revision)
        .await
        .map(Json)
}

async fn open_document(
    State(app): State<Arc<App>>,
    payload: Result<Json<OpenRequest>, JsonRejection>,
) -> Result<Json<Arc<Snapshot>>, AppError> {
    let request = json_payload(payload)?;
    app.local_reader()?.open(request.path, None).await.map(Json)
}

async fn compare_documents(
    State(app): State<Arc<App>>,
    payload: Result<Json<CompareRequest>, JsonRejection>,
) -> Result<Json<Arc<Snapshot>>, AppError> {
    let request = json_payload(payload)?;
    app.local_reader()?
        .open(request.base_path, Some(request.candidate_path))
        .await
        .map(Json)
}

async fn events(
    State(app): State<Arc<App>>,
    identity: Option<Extension<VerifiedIdentity>>,
    session: Option<Extension<auth::SessionHandle>>,
) -> Result<impl IntoResponse, AppError> {
    let workspace = app.workspace(identity).await?;
    let authorization = match (&app.auth, session) {
        (Some(auth), Some(Extension(session))) => Some((Arc::clone(auth), session)),
        (Some(_), None) => {
            return Err(AppError(
                StatusCode::UNAUTHORIZED,
                "La sesión no es válida.".into(),
            ));
        }
        (None, _) => None,
    };
    let events = stream::unfold(
        (workspace.subscribe(), true, authorization),
        |(mut receiver, first, authorization)| async move {
            if !first {
                loop {
                    if let Some((auth, session)) = &authorization {
                        tokio::select! {
                            changed=receiver.changed()=>{if changed.is_err(){return None;}break;},
                            _=tokio::time::sleep(Duration::from_secs(2))=>{
                                if auth.revalidate(session).await.is_err(){return None;}
                            }
                        }
                    } else {
                        if receiver.changed().await.is_err() {
                            return None;
                        }
                        break;
                    }
                }
            }
            if let Some((auth, session)) = &authorization
                && auth.revalidate(session).await.is_err()
            {
                return None;
            }
            let revision = *receiver.borrow_and_update();
            let event = Event::default()
                .event("revision")
                .data(revision.to_string());
            Some((Ok::<_, Infallible>(event), (receiver, false, authorization)))
        },
    );
    Ok(Sse::new(events).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}

fn router(app: Arc<App>) -> Router {
    let router = Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../web/index.html")) }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/app.js"),
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../web/style.css"),
                )
            }),
        )
        .route("/api/state", get(get_state))
        .route("/api/events", get(events));
    let router = match &app.mode {
        Mode::Local(reader) => router
            .route("/api/open", post(open_document))
            .route("/api/compare", post(compare_documents))
            .route_service("/mcp", mcp::service(Arc::clone(reader), app.port)),
        Mode::Hosted(_) => router.route(
            "/api/work",
            get(get_work)
                .put(replace_work)
                .patch(propose_work)
                .delete(clear_work)
                .layer(DefaultBodyLimit::max(CONTENT_BODY_LIMIT)),
        ),
    };
    let router = if app.auth.is_some() {
        router
            .merge(auth::routes())
            .route("/mcp", axum::routing::any(mcp::hosted_service))
            .layer(middleware::from_fn_with_state(
                Arc::clone(&app),
                auth::authenticate,
            ))
    } else {
        router
    };
    router
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn_with_state(Arc::clone(&app), security))
        .with_state(app)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CliMode {
    Local,
    Hosted,
}

struct Arguments {
    mode: CliMode,
    root: Option<PathBuf>,
    oidc_config: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    port: u16,
    documents: Vec<PathBuf>,
}

fn parse_arguments(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<Option<Arguments>, String> {
    let mut arguments = arguments.into_iter();
    let mut options = Arguments {
        mode: CliMode::Local,
        root: None,
        oidc_config: None,
        data_dir: None,
        port: 3847,
        documents: Vec::new(),
    };
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--help" | "-h") => return Ok(None),
            Some("--mode") => {
                options.mode = match arguments.next().as_deref().and_then(|s| s.to_str()) {
                    Some("local") => CliMode::Local,
                    Some("hosted") => CliMode::Hosted,
                    _ => return Err("Usa --mode local o --mode hosted.".into()),
                };
            }
            Some("--oidc-config") => {
                options.oidc_config = Some(
                    arguments
                        .next()
                        .ok_or("Falta el archivo después de --oidc-config.")?
                        .into(),
                );
            }
            Some("--data-dir") => {
                options.data_dir = Some(
                    arguments
                        .next()
                        .ok_or("Falta la carpeta después de --data-dir.")?
                        .into(),
                );
            }
            Some("--root") => {
                options.root = Some(
                    arguments
                        .next()
                        .ok_or("Falta la ruta después de --root.")?
                        .into(),
                )
            }
            Some("--port") => {
                let port = arguments
                    .next()
                    .ok_or("Falta el puerto después de --port.")?;
                options.port = port
                    .to_str()
                    .and_then(|value| value.parse().ok())
                    .ok_or("Puerto inválido.")?;
            }
            Some(value) if value.starts_with('-') => {
                return Err(format!("Opción desconocida: {value}"));
            }
            _ => options.documents.push(argument.into()),
        }
    }
    if options.mode == CliMode::Hosted && (options.root.is_some() || !options.documents.is_empty())
    {
        return Err("El modo alojado no admite --root ni rutas de archivos locales.".into());
    }
    if options.mode == CliMode::Local && options.oidc_config.is_some() {
        return Err("--oidc-config solo está disponible en modo alojado.".into());
    }
    if (options.mode == CliMode::Hosted) != options.data_dir.is_some() {
        return Err(
            "El modo hosted requiere --data-dir CARPETA; el modo local no admite esa opción."
                .into(),
        );
    }
    if !matches!(options.documents.len(), 0 | 2) {
        return Err("Indica dos archivos para comparar, o ninguno para abrir el lector.".into());
    }
    Ok(Some(options))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(arguments) = parse_arguments(env::args_os().skip(1)).map_err(std::io::Error::other)?
    else {
        println!(
            "Uso: mddiff [--mode local|hosted] [--root CARPETA] [--port PUERTO] [--oidc-config ARCHIVO] [--data-dir CARPETA] [original.md propuesta.md]\n\nServidor local de lectura y comparación de Markdown. Puerto predeterminado: 3847. Modo predeterminado: local.\nEl modo hosted requiere --data-dir para el trabajo persistente y --oidc-config para autenticar. Sin OIDC, el acceso queda cerrado."
        );
        return Ok(());
    };
    let auth = match &arguments.oidc_config {
        Some(path) => Some(Arc::new(
            auth::Authenticator::new(auth::Config::load(path)?).await?,
        )),
        None => None,
    };
    let mode = match arguments.mode {
        CliMode::Local => Mode::Local(Arc::new(LocalReader::new(
            arguments.root.unwrap_or_else(|| PathBuf::from(".")),
        )?)),
        CliMode::Hosted => Mode::Hosted(Workspaces::persistent(
            arguments.data_dir.as_deref().unwrap(),
        )?),
    };
    let listener =
        tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, arguments.port)).await?;
    let port = listener.local_addr()?.port();
    let app = Arc::new(App { mode, port, auth });
    if let [base, candidate] = arguments.documents.as_slice() {
        app.local_reader()?
            .open(base.clone(), Some(candidate.clone()))
            .await
            .map_err(|error| std::io::Error::other(error.1))?;
    }
    println!("MDdiff: http://127.0.0.1:{port}");
    match &app.mode {
        Mode::Local(reader) => println!("Carpeta autorizada: {}", reader.root().display()),
        Mode::Hosted(_) => {
            println!(
                "Modo alojado: {}",
                if app.auth.is_some() {
                    "autenticación OIDC habilitada."
                } else {
                    "acceso cerrado sin configuración OIDC."
                }
            )
        }
    }
    // Open SSE streams must not block shutdown; SQLite recovers incomplete transactions.
    tokio::select! {
        result = async { axum::serve(listener, router(app)).await } => result?,
        result = tokio::signal::ctrl_c() => result?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use futures_util::StreamExt;
    use mddiff::workspace::Document;
    use tower::ServiceExt;

    fn identity(subject: &str) -> VerifiedIdentity {
        VerifiedIdentity::from_verified_claims("synthetic-issuer".into(), subject.into()).unwrap()
    }

    fn request(path: &str) -> Request<Body> {
        Request::builder()
            .uri(path)
            .header(header::HOST, "127.0.0.1:3847")
            .header("x-user-id", "bob")
            .header(header::AUTHORIZATION, "Bearer fabricated")
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn hosted_handlers_require_trusted_context_and_scope_state_and_events() {
        let app = Arc::new(App {
            port: 3847,
            mode: Mode::Hosted(Workspaces::default()),
            auth: None,
        });
        assert!(app.local_reader().is_err());
        for path in ["/api/state?owner_id=alice", "/api/events?owner_id=alice"] {
            assert_eq!(
                router(Arc::clone(&app))
                    .oneshot(request(path))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
        }
        let alice = app
            .workspace(Some(Extension(identity("alice"))))
            .await
            .unwrap();
        let alice_router = router(Arc::clone(&app)).layer(Extension(identity("alice")));
        let bob_router = router(Arc::clone(&app)).layer(Extension(identity("bob")));
        let mut alice_events = alice_router
            .clone()
            .oneshot(request("/api/events"))
            .await
            .unwrap()
            .into_body()
            .into_data_stream();
        let mut bob_events = bob_router
            .clone()
            .oneshot(request("/api/events"))
            .await
            .unwrap()
            .into_body()
            .into_data_stream();
        for stream in [&mut alice_events, &mut bob_events] {
            let first = tokio::time::timeout(Duration::from_secs(1), stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(
                std::str::from_utf8(&first)
                    .unwrap()
                    .lines()
                    .any(|l| l.strip_prefix("data:").is_some_and(|n| n.trim() == "0"))
            );
        }
        alice
            .replace_content(
                Document {
                    label: "alice.md".into(),
                    markdown: "# Alice\n".into(),
                },
                None,
            )
            .await
            .unwrap();
        let changed = tokio::time::timeout(Duration::from_secs(1), alice_events.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            std::str::from_utf8(&changed)
                .unwrap()
                .lines()
                .any(|l| l.strip_prefix("data:").is_some_and(|n| n.trim() == "1"))
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(25), bob_events.next())
                .await
                .is_err()
        );
        for (router, expected_revision, expected_label) in
            [(alice_router, 1, Some("alice.md")), (bob_router, 0, None)]
        {
            let response = router
                .oneshot(request("/api/state?owner_id=bob"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let state: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                    .unwrap();
            assert_eq!(state["revision"], expected_revision);
            assert_eq!(state["base_path"].as_str(), expected_label);
        }
    }

    #[test]
    fn rejects_cross_origin_requests_and_non_json_posts() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:3847"));
        assert!(validate_request(&Method::GET, &headers, 3847).is_ok());
        assert!(validate_request(&Method::POST, &headers, 3847).is_err());
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:3847"),
        );
        assert!(validate_request(&Method::POST, &headers, 3847).is_ok());
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        assert!(validate_request(&Method::POST, &headers, 3847).is_err());
        headers.remove(header::ORIGIN);
        headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(validate_request(&Method::GET, &headers, 3847).is_err());
        headers.remove("sec-fetch-site");
        headers.insert(header::HOST, HeaderValue::from_static("evil.example:3847"));
        assert!(validate_request(&Method::GET, &headers, 3847).is_err());
    }

    #[test]
    fn cli_accepts_a_pair_and_rejects_incomplete_arguments() {
        let options = parse_arguments(
            ["--root", "/tmp", "--port", "4000", "base.md", "new.md"].map(OsString::from),
        )
        .unwrap()
        .unwrap();
        assert_eq!(options.port, 4000);
        assert_eq!(options.documents.len(), 2);
        for arguments in [
            vec!["base.md"],
            vec!["--port"],
            vec!["--port", "invalid"],
            vec!["--root"],
            vec!["--oidc-config"],
            vec!["--data-dir"],
            vec!["--mode", "hosted"],
            vec!["--mode", "local", "--data-dir", "private-data"],
            vec!["--mode", "local", "--oidc-config", "private.json"],
            vec!["--unknown"],
        ] {
            assert!(parse_arguments(arguments.into_iter().map(OsString::from)).is_err());
        }
    }
}
