use std::{collections::HashMap, sync::Arc, time::Instant};

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, OnceCell, RwLock, watch};

use crate::{
    AppError, MAX_DOCUMENT_BYTES,
    connections::Provider,
    diff,
    storage::{Storage, conflict},
};

/// Identity established by a trusted authentication adapter, never request fields.
/// The adapter must verify credentials before calling this constructor.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct VerifiedIdentity {
    pub(crate) issuer: String,
    pub(crate) subject: String,
}

impl VerifiedIdentity {
    pub fn from_verified_claims(issuer: String, subject: String) -> Result<Self, AppError> {
        if issuer.trim().is_empty() || subject.trim().is_empty() {
            return Err(AppError::bad_request(
                "La identidad verificada está incompleta.",
            ));
        }
        Ok(Self { issuer, subject })
    }
}

/// Already-read Markdown; a label is displayed but is never opened as a path.
#[derive(Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub label: String,
    pub markdown: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceMetadata {
    pub provider: Provider,
    pub destination: String,
    pub resource: String,
    pub url: String,
    pub captured_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Default, Serialize)]
pub struct Work {
    pub revision: u64,
    pub base: Option<Document>,
    pub candidate: Option<Document>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_source: Option<SourceMetadata>,
}

#[derive(Serialize)]
pub struct Snapshot {
    pub revision: u64,
    pub base_path: Option<String>,
    pub candidate_path: Option<String>,
    pub comparison: Option<diff::Comparison>,
    pub processing_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_source: Option<SourceMetadata>,
}

#[derive(Default)]
pub struct Workspaces {
    users: Mutex<HashMap<VerifiedIdentity, Arc<Workspace>>>,
    storage: Option<Arc<Storage>>,
}

impl Workspaces {
    /// Checks the opened SQLite handle with SELECT 1; does not inspect work or test writes.
    pub async fn check_storage(&self) -> Result<(), AppError> {
        let storage = self.storage.clone().ok_or_else(|| {
            AppError(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "El almacenamiento persistente no está disponible.".into(),
            )
        })?;
        tokio::task::spawn_blocking(move || storage.check_health())
            .await
            .map_err(|_| {
                AppError(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "El almacenamiento persistente no está disponible.".into(),
                )
            })?
    }

    pub fn connection_store(
        &self,
        key_path: &std::path::Path,
    ) -> Result<crate::connections::ConnectionStore, AppError> {
        let storage = self.storage.as_ref().ok_or_else(|| {
            AppError::bad_request("Las conexiones requieren almacenamiento persistente alojado.")
        })?;
        crate::connections::ConnectionStore::new(Arc::clone(storage), key_path)
    }

    pub fn persistent(directory: &std::path::Path) -> Result<Self, AppError> {
        Ok(Self {
            users: Mutex::default(),
            storage: Some(Arc::new(Storage::open(directory)?)),
        })
    }

    pub async fn for_user(&self, identity: &VerifiedIdentity) -> Result<Arc<Workspace>, AppError> {
        let workspace = Arc::clone(
            self.users
                .lock()
                .await
                .entry(identity.clone())
                .or_insert_with(|| {
                    Arc::new(Workspace::with_storage(
                        self.storage
                            .as_ref()
                            .map(|store| (Arc::clone(store), identity.clone())),
                    ))
                }),
        );
        workspace.restore().await?;
        Ok(workspace)
    }
}

struct Current {
    snapshot: Arc<Snapshot>,
    work: Arc<Work>,
}

pub struct Workspace {
    current: RwLock<Current>,
    updates: Mutex<()>,
    revisions: watch::Sender<u64>,
    storage: Option<(Arc<Storage>, VerifiedIdentity)>,
    restored: OnceCell<()>,
}

impl Workspace {
    pub(crate) fn new() -> Self {
        Self::with_storage(None)
    }

    fn with_storage(storage: Option<(Arc<Storage>, VerifiedIdentity)>) -> Self {
        let (revisions, _) = watch::channel(0);
        Self {
            current: RwLock::new(Current {
                snapshot: Arc::new(Snapshot {
                    revision: 0,
                    base_path: None,
                    candidate_path: None,
                    comparison: None,
                    processing_ms: 0.0,
                    base_source: None,
                }),
                work: Arc::new(Work::default()),
            }),
            updates: Mutex::new(()),
            revisions,
            storage,
            restored: OnceCell::new(),
        }
    }

    async fn restore(&self) -> Result<(), AppError> {
        if let Some((store, owner)) = &self.storage {
            self.restored
                .get_or_try_init(|| async {
                    let store = Arc::clone(store);
                    let owner = owner.clone();
                    let current = tokio::task::spawn_blocking(move || {
                        let started = Instant::now();
                        prepare(store.read(&owner)?, true, started)
                    })
                    .await
                    .map_err(|_| AppError::internal("No se pudo recuperar el documento."))??;
                    let revision = current.work.revision;
                    *self.current.write().await = current;
                    self.revisions.send_replace(revision);
                    Ok(())
                })
                .await?;
        }
        Ok(())
    }

    pub async fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&self.current.read().await.snapshot)
    }

    pub async fn work(&self) -> Arc<Work> {
        Arc::clone(&self.current.read().await.work)
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.revisions.subscribe()
    }

    pub async fn replace_content(
        self: &Arc<Self>,
        base: Document,
        candidate: Option<Document>,
    ) -> Result<Arc<Snapshot>, AppError> {
        self.load(move || Ok((base, candidate))).await
    }

    pub async fn replace_if_revision(
        self: &Arc<Self>,
        expected: u64,
        base: Document,
        candidate: Option<Document>,
    ) -> Result<Arc<Snapshot>, AppError> {
        self.update(Some(expected), None, move || {
            Ok(Work {
                base: Some(base),
                candidate,
                ..Work::default()
            })
        })
        .await
    }

    pub async fn clear_if_revision(
        self: &Arc<Self>,
        expected: u64,
    ) -> Result<Arc<Snapshot>, AppError> {
        self.update(Some(expected), None, || Ok(Work::default()))
            .await
    }

    pub async fn propose_if_revision(
        self: &Arc<Self>,
        expected: u64,
        candidate: Document,
    ) -> Result<Arc<Snapshot>, AppError> {
        let work = self.work().await;
        if work.revision != expected {
            return Err(conflict());
        }
        let base = work
            .base
            .as_ref()
            .ok_or_else(|| {
                AppError(
                    axum::http::StatusCode::CONFLICT,
                    "Abre un original antes de enviar una propuesta.".into(),
                )
            })?
            .clone();
        // The update checks the same revision again under the owner's lock.
        let base_source = work.base_source.clone();
        self.update(Some(expected), None, move || {
            Ok(Work {
                base: Some(base),
                candidate: Some(candidate),
                base_source,
                ..Work::default()
            })
        })
        .await
    }

    pub async fn capture_base_if_revision(
        self: &Arc<Self>,
        expected: u64,
        base: Document,
        source: SourceMetadata,
        connection_generation: u64,
    ) -> Result<Arc<Snapshot>, AppError> {
        if self.storage.is_none() {
            return Err(AppError::bad_request(
                "La captura de fuentes requiere almacenamiento alojado.",
            ));
        }
        let work = self.work().await;
        if work.revision != expected {
            return Err(conflict());
        }
        let candidate = work.candidate.clone();
        self.update(Some(expected), Some(connection_generation), move || {
            Ok(Work {
                base: Some(base),
                candidate,
                base_source: Some(source),
                ..Work::default()
            })
        })
        .await
    }

    pub(crate) async fn load(
        self: &Arc<Self>,
        read: impl FnOnce() -> Result<(Document, Option<Document>), AppError> + Send + 'static,
    ) -> Result<Arc<Snapshot>, AppError> {
        if self.storage.is_some() {
            return Err(AppError::bad_request(
                "El trabajo alojado requiere una revisión esperada.",
            ));
        }
        self.update(None, None, move || {
            let (base, candidate) = read()?;
            Ok(Work {
                base: Some(base),
                candidate,
                ..Work::default()
            })
        })
        .await
    }

    async fn update(
        self: &Arc<Self>,
        expected: Option<u64>,
        connection_generation: Option<u64>,
        read: impl FnOnce() -> Result<Work, AppError> + Send + 'static,
    ) -> Result<Arc<Snapshot>, AppError> {
        let workspace = Arc::clone(self);
        // Finish commit and publication even if the requesting browser disconnects.
        tokio::spawn(async move {
            let _update = workspace.updates.lock().await;
            let revision = workspace.current.read().await.work.revision;
            if expected.is_some_and(|n| n != revision) {
                return Err(conflict());
            }
            let next = revision
                .checked_add(1)
                .filter(|n| *n <= i64::MAX as u64)
                .ok_or_else(|| AppError::bad_request("La revisión alcanzó el límite admitido."))?;
            let storage = workspace.storage.clone();
            let current = tokio::task::spawn_blocking(move || {
                let started = Instant::now();
                let mut work = read()?;
                work.revision = next;
                let current = prepare(work, expected.is_some(), started)?;
                if let Some((store, owner)) = storage {
                    store.replace(&owner, revision, &current.work, connection_generation)?;
                }
                Ok::<_, AppError>(current)
            })
            .await
            .map_err(|_| AppError::internal("No se pudo procesar el documento."))??;
            let snapshot = Arc::clone(&current.snapshot);
            *workspace.current.write().await = current;
            workspace.revisions.send_replace(snapshot.revision);
            Ok(snapshot)
        })
        .await
        .map_err(|_| AppError::internal("No se pudo completar el cambio de trabajo."))?
    }
}

fn prepare(work: Work, validate_labels: bool, started: Instant) -> Result<Current, AppError> {
    if work.base.is_none() && work.base_source.is_some() {
        return Err(AppError::bad_request(
            "La procedencia requiere un documento original.",
        ));
    }
    for document in work.base.iter().chain(work.candidate.iter()) {
        if document.markdown.len() as u64 > MAX_DOCUMENT_BYTES {
            return Err(AppError(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "Cada documento admite hasta 2 MiB.".into(),
            ));
        }
        if validate_labels
            && (document.label.trim().is_empty()
                || document.label.len() > 255
                || document.label.chars().any(char::is_control))
        {
            return Err(AppError::bad_request(
                "La etiqueta requiere entre 1 y 255 bytes, sin caracteres de control.",
            ));
        }
    }
    let comparison = work.base.as_ref().map(|base| {
        diff::compare(
            &base.markdown,
            work.candidate
                .as_ref()
                .map_or(&base.markdown, |d| &d.markdown),
        )
    });
    let snapshot = Arc::new(Snapshot {
        revision: work.revision,
        base_path: work.base.as_ref().map(|d| d.label.clone()),
        candidate_path: work.candidate.as_ref().map(|d| d.label.clone()),
        comparison,
        processing_ms: started.elapsed().as_secs_f64() * 1000.0,
        base_source: work.base_source.clone(),
    });
    Ok(Current {
        snapshot,
        work: Arc::new(work),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    static DATABASE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn identity(issuer: &str, subject: &str) -> VerifiedIdentity {
        VerifiedIdentity::from_verified_claims(issuer.into(), subject.into()).unwrap()
    }
    fn document(text: &str) -> Document {
        Document {
            label: "synthetic.md".into(),
            markdown: text.into(),
        }
    }

    fn source() -> SourceMetadata {
        SourceMetadata {
            provider: Provider::Memos,
            destination: "notes".into(),
            resource: "memos/synthetic-1".into(),
            url: "https://notes.example/memos/synthetic-1".into(),
            captured_at: 123456,
            updated_at: Some("2026-10-28T12:00:00Z".into()),
        }
    }

    fn connect(store: &Workspaces, owner: &VerifiedIdentity, expected: u64) -> u64 {
        store
            .storage
            .as_ref()
            .unwrap()
            .replace_connection(
                owner,
                "memos",
                "notes",
                expected,
                Some(crate::storage::EncryptedCredential {
                    nonce: [0; 12],
                    ciphertext: vec![0; 16],
                }),
            )
            .unwrap()
    }

    #[tokio::test]
    async fn health_checks_sqlite_without_requiring_writes_or_creating_work() {
        let directory = Database::new();
        let store = Workspaces::persistent(&directory.0).unwrap();
        store.storage.as_ref().unwrap().read_only_for_test(true);
        store.check_storage().await.unwrap();
        assert_eq!(
            store
                .storage
                .as_ref()
                .unwrap()
                .database_for_test()
                .query_row("SELECT count(*) FROM current_work", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            Workspaces::default().check_storage().await.unwrap_err().0,
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn captured_source_survives_restart_proposals_and_disconnect_but_explicit_replacement_clears_it()
     {
        let directory = Database::new();
        let owner = identity("issuer", "alice");
        let expected_source = serde_json::to_value(source()).unwrap();
        {
            let store = Workspaces::persistent(&directory.0).unwrap();
            let workspace = store.for_user(&owner).await.unwrap();
            assert!(
                serde_json::to_value(workspace.work().await)
                    .unwrap()
                    .get("base_source")
                    .is_none()
            );
            assert!(
                serde_json::to_value(workspace.snapshot().await)
                    .unwrap()
                    .get("base_source")
                    .is_none()
            );
            workspace
                .replace_if_revision(
                    0,
                    document("old original"),
                    Some(document("preserved proposal\r\n")),
                )
                .await
                .unwrap();
            connect(&store, &owner, 0);
            let captured = workspace
                .capture_base_if_revision(1, document("captured original\r\n"), source(), 1)
                .await
                .unwrap();
            assert_eq!(
                serde_json::to_value(&captured.base_source).unwrap(),
                expected_source
            );
            assert_eq!(
                workspace.work().await.candidate.as_ref().unwrap().markdown,
                "preserved proposal\r\n"
            );
            store
                .storage
                .as_ref()
                .unwrap()
                .replace_connection(&owner, "memos", "notes", 1, None)
                .unwrap();
            workspace
                .propose_if_revision(2, document("updated proposal"))
                .await
                .unwrap();
            assert_eq!(
                serde_json::to_value(&workspace.work().await.base_source).unwrap(),
                expected_source
            );
        }
        {
            let store = Workspaces::persistent(&directory.0).unwrap();
            let workspace = store.for_user(&owner).await.unwrap();
            let work = workspace.work().await;
            assert_eq!(work.revision, 3);
            assert_eq!(
                work.base.as_ref().unwrap().markdown,
                "captured original\r\n"
            );
            assert_eq!(
                work.candidate.as_ref().unwrap().markdown,
                "updated proposal"
            );
            assert_eq!(
                serde_json::to_value(&work.base_source).unwrap(),
                expected_source
            );
            assert_eq!(
                serde_json::to_value(&workspace.snapshot().await.base_source).unwrap(),
                expected_source
            );
            workspace
                .replace_if_revision(3, document("pasted original"), None)
                .await
                .unwrap();
            assert!(workspace.work().await.base_source.is_none());
            assert!(workspace.snapshot().await.base_source.is_none());
            connect(&store, &owner, 2);
            let mut without_update = source();
            without_update.updated_at = None;
            assert!(
                serde_json::to_value(&without_update)
                    .unwrap()
                    .get("updated_at")
                    .is_none()
            );
            workspace
                .capture_base_if_revision(4, document("captured again"), without_update, 3)
                .await
                .unwrap();
            workspace.clear_if_revision(5).await.unwrap();
            assert!(workspace.work().await.base_source.is_none());
            assert!(workspace.snapshot().await.base_source.is_none());
        }
        let connection = rusqlite::Connection::open(directory.0.join("work.sqlite3")).unwrap();
        let row: (i64, bool) = connection
            .query_row(
                "SELECT revision,base_source IS NULL FROM current_work",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, (6, true));
    }

    #[tokio::test]
    async fn stale_or_disconnected_capture_preserves_work_snapshot_and_events() {
        let directory = Database::new();
        let store = Workspaces::persistent(&directory.0).unwrap();
        let alice = identity("issuer", "alice");
        let bob = identity("issuer", "bob");
        let workspace = store.for_user(&alice).await.unwrap();
        workspace
            .replace_if_revision(0, document("current"), Some(document("proposal")))
            .await
            .unwrap();
        connect(&store, &bob, 0);
        let before = workspace.snapshot().await;
        let events = workspace.subscribe();
        for metadata in [
            source(),
            SourceMetadata {
                destination: "another".into(),
                ..source()
            },
            SourceMetadata {
                provider: Provider::Jira,
                ..source()
            },
        ] {
            assert_eq!(
                workspace
                    .capture_base_if_revision(1, document("must not save"), metadata, 1)
                    .await
                    .err()
                    .unwrap()
                    .0,
                axum::http::StatusCode::CONFLICT
            );
            assert!(Arc::ptr_eq(&before, &workspace.snapshot().await));
            assert!(!events.has_changed().unwrap());
        }
        connect(&store, &alice, 0);
        assert_eq!(
            workspace
                .capture_base_if_revision(0, document("late original"), source(), 1)
                .await
                .err()
                .unwrap()
                .0,
            axum::http::StatusCode::CONFLICT
        );
        connect(&store, &alice, 1);
        assert_eq!(
            workspace
                .capture_base_if_revision(1, document("old credential generation"), source(), 1)
                .await
                .err()
                .unwrap()
                .0,
            axum::http::StatusCode::CONFLICT
        );
        store
            .storage
            .as_ref()
            .unwrap()
            .replace_connection(&alice, "memos", "notes", 2, None)
            .unwrap();
        // Even knowing the tombstone's current generation cannot authorize a capture.
        assert_eq!(
            workspace
                .capture_base_if_revision(1, document("disconnected original"), source(), 3)
                .await
                .err()
                .unwrap()
                .0,
            axum::http::StatusCode::CONFLICT
        );
        assert!(Arc::ptr_eq(&before, &workspace.snapshot().await));
        assert!(!events.has_changed().unwrap());
        assert_eq!(
            workspace.work().await.candidate.as_ref().unwrap().markdown,
            "proposal"
        );
        assert_eq!(
            store
                .storage
                .as_ref()
                .unwrap()
                .read(&alice)
                .unwrap()
                .revision,
            1
        );
    }

    #[tokio::test]
    async fn disconnect_during_capture_preparation_is_checked_before_the_durable_commit() {
        let directory = Database::new();
        let store = Workspaces::persistent(&directory.0).unwrap();
        let owner = identity("issuer", "alice");
        let workspace = store.for_user(&owner).await.unwrap();
        workspace
            .replace_if_revision(0, document("keep me"), None)
            .await
            .unwrap();
        connect(&store, &owner, 0);
        let before = workspace.snapshot().await;
        let events = workspace.subscribe();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let worker = Arc::clone(&workspace);
        let capture = tokio::spawn(async move {
            worker
                .update(Some(1), Some(1), move || {
                    entered.send(()).unwrap();
                    wait.recv().unwrap();
                    Ok(Work {
                        base: Some(document("late remote response")),
                        base_source: Some(source()),
                        ..Work::default()
                    })
                })
                .await
        });
        ready.await.unwrap();
        store
            .storage
            .as_ref()
            .unwrap()
            .replace_connection(&owner, "memos", "notes", 1, None)
            .unwrap();
        release.send(()).unwrap();
        assert_eq!(
            capture.await.unwrap().err().unwrap().0,
            axum::http::StatusCode::CONFLICT
        );
        assert!(Arc::ptr_eq(&before, &workspace.snapshot().await));
        assert!(!events.has_changed().unwrap());
        assert_eq!(
            store
                .storage
                .as_ref()
                .unwrap()
                .read(&owner)
                .unwrap()
                .base
                .unwrap()
                .markdown,
            "keep me"
        );
    }

    #[tokio::test]
    async fn schema_two_migration_preserves_work_and_encrypted_connections() {
        let directory = Database::new();
        let owner = identity("issuer", "alice");
        {
            let store = Workspaces::persistent(&directory.0).unwrap();
            store
                .for_user(&owner)
                .await
                .unwrap()
                .replace_if_revision(
                    0,
                    document("schema two original"),
                    Some(document("schema two proposal")),
                )
                .await
                .unwrap();
            connect(&store, &owner, 0);
        }
        let database = rusqlite::Connection::open(directory.0.join("work.sqlite3")).unwrap();
        database
            .execute_batch(
                "ALTER TABLE current_work DROP COLUMN base_source; PRAGMA user_version=2;",
            )
            .unwrap();
        drop(database);
        let store = Workspaces::persistent(&directory.0).unwrap();
        let work = store.for_user(&owner).await.unwrap().work().await;
        assert_eq!(work.revision, 1);
        assert_eq!(work.base.as_ref().unwrap().markdown, "schema two original");
        assert_eq!(
            work.candidate.as_ref().unwrap().markdown,
            "schema two proposal"
        );
        assert!(work.base_source.is_none());
        let connection = store
            .storage
            .as_ref()
            .unwrap()
            .read_connection(&owner, "memos", "notes")
            .unwrap();
        assert_eq!(connection.generation, 1);
        assert_eq!(connection.encrypted.unwrap().ciphertext, vec![0; 16]);
        assert_eq!(
            store
                .storage
                .as_ref()
                .unwrap()
                .database_for_test()
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            3
        );
    }

    #[test]
    fn ordinary_document_inputs_cannot_claim_source_metadata() {
        for key in ["source", "base_source"] {
            let mut forged = serde_json::json!({"label":"pasted.md", "markdown":"# Pasted"});
            forged[key] = serde_json::to_value(source()).unwrap();
            assert!(serde_json::from_value::<Document>(forged).is_err());
        }
        let mut forged = serde_json::to_value(source()).unwrap();
        forged["owner_id"] = "someone-else".into();
        assert!(serde_json::from_value::<SourceMetadata>(forged).is_err());
    }

    #[tokio::test]
    async fn proposals_preserve_the_original_and_do_not_replace_a_different_work() {
        let workspace = Arc::new(Workspace::new());
        assert_eq!(
            workspace
                .propose_if_revision(0, document("proposal"))
                .await
                .err()
                .unwrap()
                .0,
            axum::http::StatusCode::CONFLICT
        );
        workspace
            .replace_if_revision(0, document("original\r\n"), None)
            .await
            .unwrap();
        workspace
            .propose_if_revision(1, document("proposal"))
            .await
            .unwrap();
        let work = workspace.work().await;
        assert_eq!(work.base.as_ref().unwrap().markdown, "original\r\n");
        assert_eq!(work.candidate.as_ref().unwrap().markdown, "proposal");
        workspace
            .replace_if_revision(2, document("another original"), None)
            .await
            .unwrap();
        let before = workspace.snapshot().await;
        let events = workspace.subscribe();
        assert_eq!(
            workspace
                .propose_if_revision(2, document("late"))
                .await
                .err()
                .unwrap()
                .0,
            axum::http::StatusCode::CONFLICT
        );
        assert!(Arc::ptr_eq(&before, &workspace.snapshot().await));
        assert!(!events.has_changed().unwrap());
    }

    struct Database(std::path::PathBuf);
    impl Database {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                    "mddiff-work-{}-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos(),
                    DATABASE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
                )))
        }
    }
    impl Drop for Database {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn durable_replacement_and_deletion_recover_exact_content_and_revision_per_owner() {
        let directory = Database::new();
        let alice = identity("issuer", "alice");
        let bob = identity("issuer", "bob");
        let original = "# Original\r\n\r\nAcentos y 🦀.\r\n";
        let proposal = "# Propuesta\n\nTexto sin salto final";
        {
            let store = Workspaces::persistent(&directory.0).unwrap();
            store
                .for_user(&alice)
                .await
                .unwrap()
                .replace_if_revision(0, document("old"), None)
                .await
                .unwrap();
            let workspace = store.for_user(&alice).await.unwrap();
            workspace
                .replace_if_revision(1, document(original), Some(document(proposal)))
                .await
                .unwrap();
            store
                .for_user(&bob)
                .await
                .unwrap()
                .replace_if_revision(0, document("# Bob"), None)
                .await
                .unwrap();
            assert_eq!(workspace.work().await.revision, 2);
        }
        {
            let store = Workspaces::persistent(&directory.0).unwrap();
            let workspace = store.for_user(&alice).await.unwrap();
            let work = workspace.work().await;
            assert_eq!(work.revision, 2);
            assert_eq!(work.base.as_ref().unwrap().markdown, original);
            assert_eq!(work.candidate.as_ref().unwrap().markdown, proposal);
            assert_eq!(workspace.snapshot().await.revision, 2);
            assert_eq!(
                store
                    .for_user(&identity("other-issuer", "alice"))
                    .await
                    .unwrap()
                    .work()
                    .await
                    .revision,
                0
            );
            workspace.clear_if_revision(2).await.unwrap();
            assert!(workspace.work().await.base.is_none());
            assert_eq!(
                store
                    .for_user(&bob)
                    .await
                    .unwrap()
                    .work()
                    .await
                    .base
                    .as_ref()
                    .unwrap()
                    .markdown,
                "# Bob"
            );
        }
        let store = Workspaces::persistent(&directory.0).unwrap();
        let workspace = store.for_user(&alice).await.unwrap();
        assert_eq!(workspace.work().await.revision, 3);
        assert!(workspace.work().await.base.is_none());
        assert!(workspace.work().await.candidate.is_none());
        assert!(workspace.snapshot().await.comparison.is_none());
        assert_eq!(
            workspace
                .replace_if_revision(2, document("stale"), None)
                .await
                .err()
                .unwrap()
                .0,
            axum::http::StatusCode::CONFLICT
        );
        assert_eq!(
            workspace.clear_if_revision(0).await.err().unwrap().0,
            axum::http::StatusCode::CONFLICT
        );
        workspace
            .replace_if_revision(3, document("fresh"), None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn storage_errors_and_invalid_documents_preserve_cache_revision_and_events() {
        let directory = Database::new();
        let store = Workspaces::persistent(&directory.0).unwrap();
        let workspace = store.for_user(&identity("issuer", "alice")).await.unwrap();
        let before = workspace
            .replace_if_revision(0, document("last valid"), None)
            .await
            .unwrap();
        let events = workspace.subscribe();
        let database = &workspace.storage.as_ref().unwrap().0;
        database.read_only_for_test(true);
        for result in [
            workspace
                .replace_if_revision(1, document("cannot save"), None)
                .await,
            workspace.clear_if_revision(1).await,
        ] {
            assert_eq!(
                result.err().unwrap().0,
                axum::http::StatusCode::SERVICE_UNAVAILABLE
            );
            assert!(Arc::ptr_eq(&before, &workspace.snapshot().await));
            assert!(!events.has_changed().unwrap());
        }
        database.read_only_for_test(false);
        for doc in [
            document(&"x".repeat(MAX_DOCUMENT_BYTES as usize + 1)),
            Document {
                label: "\n".into(),
                markdown: "invalid label".into(),
            },
        ] {
            assert!(workspace.replace_if_revision(1, doc, None).await.is_err());
            assert!(Arc::ptr_eq(&before, &workspace.snapshot().await));
            assert!(!events.has_changed().unwrap());
        }
        assert_eq!(
            database
                .read(&identity("issuer", "alice"))
                .unwrap()
                .base
                .unwrap()
                .markdown,
            "last valid"
        );
    }

    #[tokio::test]
    async fn simultaneous_changes_with_one_revision_have_one_winner() {
        let directory = Database::new();
        let store = Workspaces::persistent(&directory.0).unwrap();
        let workspace = store.for_user(&identity("issuer", "alice")).await.unwrap();
        let (first, second) = tokio::join!(
            workspace.replace_if_revision(0, document("one"), None),
            workspace.clear_if_revision(0)
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        assert_eq!(
            first.err().or_else(|| second.err()).unwrap().0,
            axum::http::StatusCode::CONFLICT
        );
        assert_eq!(workspace.work().await.revision, 1);
    }

    #[tokio::test]
    async fn disconnected_request_still_finishes_durable_commit_and_publication() {
        let directory = Database::new();
        let store = Workspaces::persistent(&directory.0).unwrap();
        let owner = identity("issuer", "alice");
        let workspace = store.for_user(&owner).await.unwrap();
        let mut events = workspace.subscribe();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let worker = Arc::clone(&workspace);
        let request = tokio::spawn(async move {
            worker
                .update(Some(0), None, move || {
                    entered.send(()).unwrap();
                    wait.recv().unwrap();
                    Ok(Work {
                        base: Some(document("committed despite disconnect")),
                        ..Work::default()
                    })
                })
                .await
        });
        ready.await.unwrap();
        request.abort();
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), events.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(workspace.snapshot().await.revision, 1);
        assert_eq!(
            workspace
                .storage
                .as_ref()
                .unwrap()
                .0
                .read(&owner)
                .unwrap()
                .base
                .unwrap()
                .markdown,
            "committed despite disconnect"
        );
    }

    #[test]
    fn schema_version_is_migrated_once_and_future_versions_are_not_downgraded() {
        let directory = Database::new();
        drop(Workspaces::persistent(&directory.0).unwrap());
        let connection = rusqlite::Connection::open(directory.0.join("work.sqlite3")).unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            3
        );
        let columns: i32 = connection
            .query_row(
                "SELECT count(*) FROM pragma_table_info('current_work')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(columns, 8);
        connection.pragma_update(None, "user_version", 4).unwrap();
        drop(connection);
        assert!(Workspaces::persistent(&directory.0).is_err());
        let connection = rusqlite::Connection::open(directory.0.join("work.sqlite3")).unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            4
        );
    }

    #[cfg(unix)]
    #[test]
    fn storage_is_private_and_rejects_another_instance_or_unsafe_paths() {
        use std::{
            fs,
            os::unix::fs::{PermissionsExt, symlink},
        };
        let directory = Database::new();
        let first = Workspaces::persistent(&directory.0).unwrap();
        for name in ["work.sqlite3", "work.sqlite3-wal", "work.sqlite3-shm"] {
            let path = directory.0.join(name);
            if path.exists() {
                assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
            }
        }
        assert_eq!(
            fs::metadata(&directory.0).unwrap().permissions().mode() & 0o077,
            0
        );
        assert!(Workspaces::persistent(&directory.0).is_err());
        drop(first);
        let file = directory.0.join("work.sqlite3");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Workspaces::persistent(&directory.0).is_err());
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        let link = directory.0.with_extension("link");
        symlink(&directory.0, &link).unwrap();
        assert!(Workspaces::persistent(&link).is_err());
        fs::remove_file(link).unwrap();
        fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Workspaces::persistent(&directory.0).is_err());
    }

    #[tokio::test]
    async fn owners_have_separate_content_revisions_events_and_coordination() {
        let store = Workspaces::default();
        let alice = store
            .for_user(&identity("issuer-a", "alice"))
            .await
            .unwrap();
        let bob = store.for_user(&identity("issuer-a", "bob")).await.unwrap();
        let other_issuer = store
            .for_user(&identity("issuer-b", "alice"))
            .await
            .unwrap();
        assert!(Arc::ptr_eq(
            &alice,
            &store
                .for_user(&identity("issuer-a", "alice"))
                .await
                .unwrap()
        ));
        let mut alice_events = alice.subscribe();
        let mut bob_events = bob.subscribe();
        let alice_lock = alice.updates.lock().await;
        let bob_opened = tokio::time::timeout(
            Duration::from_secs(1),
            bob.replace_content(document("# Bob\n"), None),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(bob_opened.revision, 1);
        assert_eq!(*bob_events.borrow_and_update(), 1);
        assert!(!alice_events.has_changed().unwrap());
        assert_eq!(alice.snapshot().await.revision, 0);
        drop(alice_lock);
        let alice_opened = alice
            .replace_content(document("# Alice\n"), Some(document("# Proposal\n")))
            .await
            .unwrap();
        assert_eq!(alice_opened.revision, 1);
        assert_eq!(*alice_events.borrow_and_update(), 1);
        assert!(!bob_events.has_changed().unwrap());
        assert_eq!(
            bob.snapshot().await.comparison.as_ref().unwrap().rows[0]
                .left
                .as_ref()
                .unwrap()
                .source,
            "# Bob\n"
        );
        assert_eq!(
            alice_opened.comparison.as_ref().unwrap().rows[0]
                .left
                .as_ref()
                .unwrap()
                .source,
            "# Alice\n"
        );
        assert_eq!(other_issuer.snapshot().await.revision, 0);
    }

    #[tokio::test]
    async fn content_is_never_a_path_and_invalid_content_preserves_state() {
        let store = Workspaces::default();
        let workspace = store.for_user(&identity("issuer", "alice")).await.unwrap();
        let opened = workspace
            .replace_content(
                Document {
                    label: "/missing/not-a-readable-file.md".into(),
                    markdown: "# Inline\n".into(),
                },
                None,
            )
            .await
            .unwrap();
        let events = workspace.subscribe();
        let error = workspace
            .replace_content(document(&"x".repeat(MAX_DOCUMENT_BYTES as usize + 1)), None)
            .await
            .err()
            .unwrap();
        assert_eq!(error.0, axum::http::StatusCode::PAYLOAD_TOO_LARGE);
        assert!(Arc::ptr_eq(&opened, &workspace.snapshot().await));
        assert!(!events.has_changed().unwrap());
    }

    #[test]
    fn verified_identity_requires_issuer_and_subject() {
        assert!(VerifiedIdentity::from_verified_claims("".into(), "alice".into()).is_err());
        assert!(VerifiedIdentity::from_verified_claims("issuer".into(), " ".into()).is_err());
    }
}
