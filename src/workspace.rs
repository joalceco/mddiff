use std::{collections::HashMap, sync::Arc, time::Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, OnceCell, RwLock, watch};

use crate::{
    AppError, MAX_DOCUMENT_BYTES, diff,
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
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub label: String,
    pub markdown: String,
}

#[derive(Default, Serialize)]
pub struct Work {
    pub revision: u64,
    pub base: Option<Document>,
    pub candidate: Option<Document>,
}

#[derive(Serialize)]
pub struct Snapshot {
    pub revision: u64,
    pub base_path: Option<String>,
    pub candidate_path: Option<String>,
    pub comparison: Option<diff::Comparison>,
    pub processing_ms: f64,
}

#[derive(Default)]
pub struct Workspaces {
    users: Mutex<HashMap<VerifiedIdentity, Arc<Workspace>>>,
    storage: Option<Arc<Storage>>,
}

impl Workspaces {
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
        self.update(Some(expected), move || Ok((Some(base), candidate)))
            .await
    }

    pub async fn clear_if_revision(
        self: &Arc<Self>,
        expected: u64,
    ) -> Result<Arc<Snapshot>, AppError> {
        self.update(Some(expected), || Ok((None, None))).await
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
        self.update(None, move || {
            let (base, candidate) = read()?;
            Ok((Some(base), candidate))
        })
        .await
    }

    async fn update(
        self: &Arc<Self>,
        expected: Option<u64>,
        read: impl FnOnce() -> Result<(Option<Document>, Option<Document>), AppError> + Send + 'static,
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
                let (base, candidate) = read()?;
                let current = prepare(
                    Work {
                        revision: next,
                        base,
                        candidate,
                    },
                    expected.is_some(),
                    started,
                )?;
                if let Some((store, owner)) = storage {
                    store.replace(
                        &owner,
                        revision,
                        current.work.base.as_ref(),
                        current.work.candidate.as_ref(),
                    )?;
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
    });
    Ok(Current {
        snapshot,
        work: Arc::new(work),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn identity(issuer: &str, subject: &str) -> VerifiedIdentity {
        VerifiedIdentity::from_verified_claims(issuer.into(), subject.into()).unwrap()
    }
    fn document(text: &str) -> Document {
        Document {
            label: "synthetic.md".into(),
            markdown: text.into(),
        }
    }

    struct Database(std::path::PathBuf);
    impl Database {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                    "mddiff-work-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
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
                .update(Some(0), move || {
                    entered.send(()).unwrap();
                    wait.recv().unwrap();
                    Ok((Some(document("committed despite disconnect")), None))
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
            1
        );
        let columns: i32 = connection
            .query_row(
                "SELECT count(*) FROM pragma_table_info('current_work')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(columns, 7);
        connection.pragma_update(None, "user_version", 2).unwrap();
        drop(connection);
        assert!(Workspaces::persistent(&directory.0).is_err());
        let connection = rusqlite::Connection::open(directory.0.join("work.sqlite3")).unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            2
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
