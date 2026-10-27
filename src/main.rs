mod diff;
mod mcp;

use axum::{
    Json, Router,
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
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use std::{
    convert::Infallible,
    env,
    ffi::OsString,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, RwLock, watch};

const MAX_DOCUMENT_BYTES: u64 = 2 * 1024 * 1024;
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'none'; font-src 'self'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

#[derive(Serialize)]
struct Snapshot {
    revision: u64,
    base_path: Option<String>,
    candidate_path: Option<String>,
    comparison: Option<diff::Comparison>,
    processing_ms: f64,
}

struct App {
    root: PathBuf,
    port: u16,
    snapshot: RwLock<Arc<Snapshot>>,
    updates: Mutex<()>,
    revisions: watch::Sender<u64>,
}

impl App {
    fn new(root: PathBuf, port: u16) -> Self {
        let (revisions, _) = watch::channel(0);
        Self {
            root,
            port,
            snapshot: RwLock::new(Arc::new(Snapshot {
                revision: 0,
                base_path: None,
                candidate_path: None,
                comparison: None,
                processing_ms: 0.0,
            })),
            updates: Mutex::new(()),
            revisions,
        }
    }

    async fn open(
        &self,
        base: PathBuf,
        candidate: Option<PathBuf>,
    ) -> Result<Arc<Snapshot>, AppError> {
        // Serialize requests so an earlier, slower comparison cannot replace a later one.
        let _update = self.updates.lock().await;
        let started = Instant::now();
        let root = self.root.clone();
        let (base_path, candidate_path, comparison) = tokio::task::spawn_blocking(move || {
            let (base_path, original) = read_document(&root, &base)?;
            let (candidate_path, comparison) = if let Some(candidate) = candidate {
                let (candidate_path, proposed) = read_document(&root, &candidate)?;
                (Some(candidate_path), diff::compare(&original, &proposed))
            } else {
                (None, diff::compare(&original, &original))
            };
            Ok::<_, AppError>((base_path, candidate_path, comparison))
        })
        .await
        .map_err(|_| AppError::internal("No se pudo procesar el documento."))??;

        let revision = self.snapshot.read().await.revision + 1;
        let snapshot = Arc::new(Snapshot {
            revision,
            base_path: Some(base_path),
            candidate_path,
            comparison: Some(comparison),
            processing_ms: started.elapsed().as_secs_f64() * 1000.0,
        });
        *self.snapshot.write().await = Arc::clone(&snapshot);
        self.revisions.send_replace(revision);
        Ok(snapshot)
    }
}

#[derive(Debug)]
struct AppError(StatusCode, String);

impl AppError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, message.into())
    }

    fn internal(message: impl Into<String>) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, message.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

fn read_document(root: &Path, requested: &Path) -> Result<(String, String), AppError> {
    let path = root
        .join(requested)
        .canonicalize()
        .map_err(|error| AppError::bad_request(format!("No se pudo abrir el archivo: {error}")))?;
    if !path.starts_with(root) {
        return Err(AppError::bad_request(
            "El archivo debe estar dentro de la carpeta autorizada.",
        ));
    }
    if !path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("markdown")
        })
    {
        return Err(AppError::bad_request(
            "Selecciona un archivo .md o .markdown.",
        ));
    }
    let metadata = path
        .metadata()
        .map_err(|error| AppError::bad_request(error.to_string()))?;
    if !metadata.is_file() {
        return Err(AppError::bad_request(
            "La ruta debe señalar un archivo normal.",
        ));
    }
    if metadata.len() > MAX_DOCUMENT_BYTES {
        return Err(AppError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Cada documento admite hasta 2 MiB.".into(),
        ));
    }
    let file = File::open(&path).map_err(|error| AppError::bad_request(error.to_string()))?;
    if !file
        .metadata()
        .map_err(|error| AppError::bad_request(error.to_string()))?
        .is_file()
    {
        return Err(AppError::bad_request(
            "La ruta debe señalar un archivo normal.",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| AppError::bad_request(format!("No se pudo leer el archivo: {error}")))?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(AppError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Cada documento admite hasta 2 MiB.".into(),
        ));
    }
    let source = String::from_utf8(bytes)
        .map_err(|_| AppError::bad_request("El documento debe usar codificación UTF-8."))?;
    Ok((path.to_string_lossy().into_owned(), source))
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
    if method == Method::POST {
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
    let mut response = match validate_request(request.method(), request.headers(), app.port) {
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

async fn get_state(State(app): State<Arc<App>>) -> Json<Arc<Snapshot>> {
    Json(Arc::clone(&*app.snapshot.read().await))
}

async fn open_document(
    State(app): State<Arc<App>>,
    payload: Result<Json<OpenRequest>, JsonRejection>,
) -> Result<Json<Arc<Snapshot>>, AppError> {
    let request = json_payload(payload)?;
    app.open(request.path, None).await.map(Json)
}

async fn compare_documents(
    State(app): State<Arc<App>>,
    payload: Result<Json<CompareRequest>, JsonRejection>,
) -> Result<Json<Arc<Snapshot>>, AppError> {
    let request = json_payload(payload)?;
    app.open(request.base_path, Some(request.candidate_path))
        .await
        .map(Json)
}

async fn events(State(app): State<Arc<App>>) -> impl IntoResponse {
    let events = stream::unfold(
        (app.revisions.subscribe(), true),
        |(mut receiver, first)| async move {
            if !first && receiver.changed().await.is_err() {
                return None;
            }
            let revision = *receiver.borrow_and_update();
            let event = Event::default()
                .event("revision")
                .data(revision.to_string());
            Some((Ok::<_, Infallible>(event), (receiver, false)))
        },
    );
    Sse::new(events).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

fn router(app: Arc<App>) -> Router {
    Router::new()
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
        .route("/api/open", post(open_document))
        .route("/api/compare", post(compare_documents))
        .route("/api/events", get(events))
        .route_service("/mcp", mcp::service(Arc::clone(&app)))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn_with_state(Arc::clone(&app), security))
        .with_state(app)
}

struct Arguments {
    root: PathBuf,
    port: u16,
    documents: Vec<PathBuf>,
}

fn parse_arguments(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<Option<Arguments>, String> {
    let mut arguments = arguments.into_iter();
    let mut options = Arguments {
        root: PathBuf::from("."),
        port: 3847,
        documents: Vec::new(),
    };
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--help" | "-h") => return Ok(None),
            Some("--root") => {
                options.root = arguments
                    .next()
                    .ok_or("Falta la ruta después de --root.")?
                    .into()
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
            "Uso: mddiff [--root CARPETA] [--port PUERTO] [original.md propuesta.md]\n\nServidor local de lectura y comparación de Markdown. Puerto predeterminado: 3847."
        );
        return Ok(());
    };
    let root = arguments.root.canonicalize()?;
    if !root.is_dir() {
        return Err(std::io::Error::other("--root debe señalar una carpeta.").into());
    }
    let listener =
        tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, arguments.port)).await?;
    let port = listener.local_addr()?.port();
    let app = Arc::new(App::new(root, port));
    if let [base, candidate] = arguments.documents.as_slice() {
        app.open(base.clone(), Some(candidate.clone()))
            .await
            .map_err(|error| std::io::Error::other(error.1))?;
    }
    println!(
        "MDdiff: http://127.0.0.1:{port}\nCarpeta autorizada: {}",
        app.root.display()
    );
    // The read-only server can stop immediately instead of waiting for open SSE streams.
    tokio::select! {
        result = async { axum::serve(listener, router(app)).await } => result?,
        result = tokio::signal::ctrl_c() => result?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = env::temp_dir().join(format!(
                "mddiff-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn rejects_unsafe_files_and_accepts_markdown() {
        let directory = TestDirectory::new();
        let root = directory.0.join("authorized");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("valid.MD"), "# Hola\n").unwrap();
        fs::write(directory.0.join("outside.md"), "secret").unwrap();
        fs::write(root.join("script.html"), "<script>").unwrap();
        fs::write(root.join("invalid.md"), [0xff]).unwrap();
        fs::create_dir(root.join("directory.md")).unwrap();
        let large = File::create(root.join("large.md")).unwrap();
        large.set_len(MAX_DOCUMENT_BYTES + 1).unwrap();

        assert_eq!(
            read_document(&root, Path::new("valid.MD")).unwrap().1,
            "# Hola\n"
        );
        for path in [
            "../outside.md",
            "script.html",
            "invalid.md",
            "directory.md",
            "large.md",
        ] {
            assert!(
                read_document(&root, Path::new(path)).is_err(),
                "accepted {path}"
            );
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(directory.0.join("outside.md"), root.join("escape.md"))
                .unwrap();
            assert!(read_document(&root, Path::new("escape.md")).is_err());
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

    #[tokio::test]
    async fn failed_open_preserves_document_and_revision() {
        let directory = TestDirectory::new();
        fs::write(directory.0.join("base.md"), "# Original\n").unwrap();
        fs::write(directory.0.join("candidate.md"), "# Propuesta\n").unwrap();
        let app = App::new(directory.0.clone(), 3847);
        let mut revisions = app.revisions.subscribe();
        let opened = app.open("base.md".into(), None).await.unwrap();
        assert_eq!(opened.revision, 1);
        assert!(opened.candidate_path.is_none());
        assert!(revisions.has_changed().unwrap());
        assert_eq!(*revisions.borrow_and_update(), 1);
        assert!(app.open("missing.md".into(), None).await.is_err());
        assert!(Arc::ptr_eq(&opened, &*app.snapshot.read().await));
        assert!(!revisions.has_changed().unwrap());
        let compared = app
            .open("base.md".into(), Some("candidate.md".into()))
            .await
            .unwrap();
        assert_eq!(compared.revision, 2);
        assert!(compared.candidate_path.is_some());
        assert_eq!(*revisions.borrow_and_update(), 2);
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
            vec!["--unknown"],
        ] {
            assert!(parse_arguments(arguments.into_iter().map(OsString::from)).is_err());
        }
    }
}
