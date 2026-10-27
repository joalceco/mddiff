use std::sync::Arc;

use rmcp::{
    ServerHandler,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::{
        StreamableHttpServerConfig, StreamableHttpService,
        streamable_http_server::session::local::LocalSessionManager,
    },
};
use serde_json::json;

use crate::CompareRequest;
use mddiff::local::LocalReader;

#[derive(Clone)]
pub struct MarkdownMcp {
    reader: Arc<LocalReader>,
    port: u16,
}

#[tool_router]
impl MarkdownMcp {
    fn new(reader: Arc<LocalReader>, port: u16) -> Self {
        Self { reader, port }
    }

    #[tool(
        description = "Abre dos archivos Markdown locales en mddiff y muestra su comparación renderizada con highlights. base_path es el original y candidate_path es la propuesta. Actualiza automáticamente el visor abierto; no modifica archivos. Ambas rutas deben estar dentro de la carpeta autorizada del servidor. Devuelve viewer_url para mostrar la comparación en el navegador de Codex si el visor aún no está visible.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn open_comparison(
        &self,
        Parameters(request): Parameters<CompareRequest>,
    ) -> CallToolResult {
        let snapshot = match self
            .reader
            .open(request.base_path, Some(request.candidate_path))
            .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => return CallToolResult::error(vec![ContentBlock::text(error.1)]),
        };
        let comparison = snapshot
            .comparison
            .as_ref()
            .expect("a successful comparison has content");
        CallToolResult::structured(json!({
            "viewer_url": format!("http://127.0.0.1:{}/", self.port),
            "base_path": snapshot.base_path,
            "candidate_path": snapshot.candidate_path,
            "revision": snapshot.revision,
            "added_blocks": comparison.added,
            "removed_blocks": comparison.removed,
            "source_equal": comparison.source_equal,
            "processing_ms": snapshot.processing_ms,
        }))
    }
}

#[tool_handler]
impl ServerHandler for MarkdownMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "Usa open_comparison para comparar dos Markdown sin modificar archivos. El visor abierto se actualiza automáticamente. Tras una llamada correcta, muestra viewer_url en el navegador de Codex si aún no está visible. Las rutas se resuelven dentro de {}. Los errores conservan la comparación anterior.",
                self.reader.root().display()
            ))
    }
}

pub fn service(
    reader: Arc<LocalReader>,
    port: u16,
) -> StreamableHttpService<MarkdownMcp, LocalSessionManager> {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_max_request_body_bytes(16 * 1024);
    StreamableHttpService::new(
        move || Ok(MarkdownMcp::new(Arc::clone(&reader), port)),
        Arc::new(LocalSessionManager::default()),
        config,
    )
}
