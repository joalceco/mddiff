use std::{
    collections::HashMap,
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, OnceCell, OwnedSemaphorePermit, RwLock, Semaphore, watch};

use crate::{
    AppError, MAX_DOCUMENT_BYTES,
    connections::Provider,
    diff,
    privacy::{JiraData, WorkPrivacy},
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
#[derive(Clone, Deserialize, Serialize, PartialEq, schemars::JsonSchema)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub converter: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum InputKind {
    Text,
    File,
    Api,
    Agent,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManualInput {
    Text,
    File,
}

impl From<ManualInput> for InputKind {
    fn from(value: ManualInput) -> Self {
        match value {
            ManualInput::Text => Self::Text,
            ManualInput::File => Self::File,
        }
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Debug)]
#[serde(deny_unknown_fields)]
pub struct InputMetadata {
    pub kind: InputKind,
    pub captured_at: u64,
}

impl InputMetadata {
    fn capture(kind: InputKind) -> Self {
        Self {
            kind,
            captured_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        }
    }
}

#[derive(Default, Serialize)]
pub struct Work {
    pub revision: u64,
    pub base: Option<Document>,
    pub candidate: Option<Document>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_source: Option<SourceMetadata>,
    // Rendering metadata stays out of the raw API/MCP document contract.
    #[serde(skip_serializing)]
    pub base_input: Option<InputMetadata>,
    #[serde(skip_serializing)]
    pub candidate_input: Option<InputMetadata>,
    #[serde(skip)]
    pub jira_privacy: WorkPrivacy,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_input: Option<InputMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_input: Option<InputMetadata>,
}

#[derive(Default)]
pub struct Workspaces {
    users: Mutex<HashMap<VerifiedIdentity, Arc<Workspace>>>,
    storage: Option<Arc<Storage>>,
    budget: Arc<Budget>,
}

struct Budget {
    requests: Arc<Semaphore>,
    bodies: Arc<Semaphore>,
    sources: Arc<Semaphore>,
    compute: Arc<Semaphore>,
    jobs: Arc<Semaphore>,
    events: Arc<Semaphore>,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            requests: Arc::new(Semaphore::new(16)),
            bodies: Arc::new(Semaphore::new(2)),
            sources: Arc::new(Semaphore::new(2)),
            compute: Arc::new(Semaphore::new(2)),
            jobs: Arc::new(Semaphore::new(10)),
            events: Arc::new(Semaphore::new(40)),
        }
    }
}

fn capacity() -> AppError {
    AppError(
        axum::http::StatusCode::TOO_MANY_REQUESTS,
        "El servicio alcanzó su cuota de operaciones. Vuelve a intentarlo en un segundo.".into(),
    )
}

fn permit(semaphore: &Arc<Semaphore>) -> Result<OwnedSemaphorePermit, AppError> {
    Arc::clone(semaphore)
        .try_acquire_owned()
        .map_err(|_| capacity())
}

pub enum RequestKind {
    Content,
    Source,
    Events,
}

impl Workspaces {
    pub async fn request_permits(
        &self,
        identity: &VerifiedIdentity,
        kind: RequestKind,
    ) -> Result<Vec<OwnedSemaphorePermit>, AppError> {
        let workspace = self.entry(identity).await?;
        if matches!(kind, RequestKind::Events) {
            return Ok(vec![
                permit(&self.budget.events)?,
                permit(&workspace.events)?,
            ]);
        }
        let mut permits = vec![permit(&self.budget.requests)?, permit(&workspace.requests)?];
        permits.push(
            Arc::clone(if matches!(kind, RequestKind::Source) {
                &self.budget.sources
            } else {
                &self.budget.bodies
            })
            .acquire_owned()
            .await
            .map_err(|_| capacity())?,
        );
        Ok(permits)
    }

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

    pub fn checkpoint_directory(
        &self,
        directory: &std::path::Path,
    ) -> Result<std::path::PathBuf, AppError> {
        self.storage
            .as_ref()
            .ok_or_else(|| {
                AppError::bad_request("El checkpoint requiere almacenamiento persistente.")
            })?
            .checkpoint_directory(directory)
    }

    pub async fn checkpoint(&self, directory: std::path::PathBuf) -> Result<(), AppError> {
        let storage = self.storage.clone().ok_or_else(|| {
            AppError::bad_request("El checkpoint requiere almacenamiento persistente.")
        })?;
        tokio::task::spawn_blocking(move || storage.checkpoint(&directory))
            .await
            .map_err(|_| AppError::internal("No se pudo crear el checkpoint del operador."))?
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
            budget: Arc::default(),
        })
    }

    pub fn persistent_with_barrier(
        directory: &std::path::Path,
        journal: &std::path::Path,
    ) -> Result<Self, AppError> {
        Ok(Self {
            users: Mutex::default(),
            storage: Some(Arc::new(Storage::open_with_barrier(
                directory,
                Some(journal),
            )?)),
            budget: Arc::default(),
        })
    }

    async fn entry(&self, identity: &VerifiedIdentity) -> Result<Arc<Workspace>, AppError> {
        if let Some(storage) = &self.storage {
            storage.check_barrier()?;
        }
        let mut users = self.users.lock().await;
        if self.storage.is_some() && !users.contains_key(identity) && users.len() >= 10 {
            let idle = users
                .iter()
                .find(|(_, workspace)| {
                    Arc::strong_count(workspace) == 1
                        && workspace.requests.available_permits() == 4
                        && workspace.events.available_permits() == 3
                })
                .map(|(owner, _)| owner.clone());
            if let Some(idle) = idle {
                users.remove(&idle);
            } else {
                return Err(capacity());
            }
        }
        Ok(Arc::clone(users.entry(identity.clone()).or_insert_with(
            || {
                Arc::new(Workspace::with_storage(
                    self.storage
                        .as_ref()
                        .map(|store| (Arc::clone(store), identity.clone())),
                    Some(Arc::clone(&self.budget)),
                ))
            },
        )))
    }

    pub async fn for_user(&self, identity: &VerifiedIdentity) -> Result<Arc<Workspace>, AppError> {
        let workspace = self.entry(identity).await?;
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
    budget: Option<Arc<Budget>>,
    jobs: Arc<Semaphore>,
    requests: Arc<Semaphore>,
    events: Arc<Semaphore>,
}

impl Workspace {
    pub(crate) fn new() -> Self {
        Self::with_storage(None, None)
    }

    fn with_storage(
        storage: Option<(Arc<Storage>, VerifiedIdentity)>,
        budget: Option<Arc<Budget>>,
    ) -> Self {
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
                    base_input: None,
                    candidate_input: None,
                }),
                work: Arc::new(Work::default()),
            }),
            updates: Mutex::new(()),
            revisions,
            storage,
            restored: OnceCell::new(),
            budget,
            jobs: Arc::new(Semaphore::new(2)),
            requests: Arc::new(Semaphore::new(4)),
            events: Arc::new(Semaphore::new(3)),
        }
    }

    async fn restore(&self) -> Result<(), AppError> {
        if let Some((store, owner)) = &self.storage {
            self.restored
                .get_or_try_init(|| async {
                    let compute = match &self.budget {
                        Some(budget) => Some(
                            Arc::clone(&budget.compute)
                                .acquire_owned()
                                .await
                                .map_err(|_| capacity())?,
                        ),
                        None => None,
                    };
                    let store = Arc::clone(store);
                    let owner = owner.clone();
                    let current = tokio::task::spawn_blocking(move || {
                        let _compute = compute;
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

    pub async fn compute_permit(&self) -> Result<Option<OwnedSemaphorePermit>, AppError> {
        match &self.budget {
            Some(budget) => Arc::clone(&budget.compute)
                .acquire_owned()
                .await
                .map(Some)
                .map_err(|_| capacity()),
            None => Ok(None),
        }
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
        self.replace_with_inputs_if_revision(expected, base, candidate, None, None)
            .await
    }

    pub async fn replace_with_inputs_if_revision(
        self: &Arc<Self>,
        expected: u64,
        base: Document,
        candidate: Option<Document>,
        base_kind: Option<InputKind>,
        candidate_kind: Option<InputKind>,
    ) -> Result<Arc<Snapshot>, AppError> {
        let previous = self.work().await;
        if previous.revision != expected {
            return Err(conflict());
        }
        let same_base = previous.base.as_ref() == Some(&base) && base_kind.is_none();
        let same_candidate = previous.candidate == candidate && candidate_kind.is_none();
        let base_source = if same_base {
            previous.base_source.clone()
        } else {
            None
        };
        let base_input = if same_base {
            previous.base_input.clone()
        } else {
            Some(InputMetadata::capture(base_kind.unwrap_or(InputKind::Api)))
        };
        let candidate_input = if candidate.is_none() {
            None
        } else if same_candidate {
            previous.candidate_input.clone()
        } else {
            Some(InputMetadata::capture(
                candidate_kind.unwrap_or(InputKind::Api),
            ))
        };
        let jira_privacy = WorkPrivacy {
            base: if same_base {
                previous.jira_privacy.base.clone()
            } else {
                JiraData::default()
            },
            candidate: if candidate.is_none() {
                JiraData::default()
            } else if same_candidate {
                previous.jira_privacy.candidate.clone()
            } else if candidate_kind.is_none() {
                previous.jira_privacy.candidate_copy()
            } else {
                JiraData::default()
            },
        };
        self.update(Some(expected), None, move || {
            Ok(Work {
                base: Some(base),
                candidate,
                base_source,
                base_input,
                candidate_input,
                jira_privacy,
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
        self.propose_with_input_if_revision(expected, candidate, InputKind::Api)
            .await
    }

    pub async fn propose_with_input_if_revision(
        self: &Arc<Self>,
        expected: u64,
        candidate: Document,
        kind: InputKind,
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
        let base_input = work.base_input.clone();
        let jira_privacy = WorkPrivacy {
            base: work.jira_privacy.base.clone(),
            candidate: if matches!(kind, InputKind::Agent | InputKind::Api) {
                work.jira_privacy.candidate_copy()
            } else {
                work.jira_privacy.base.clone()
            },
        };
        self.update(Some(expected), None, move || {
            Ok(Work {
                base: Some(base),
                candidate: Some(candidate),
                base_source,
                base_input,
                candidate_input: Some(InputMetadata::capture(kind)),
                jira_privacy,
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
        let privacy = JiraData {
            unclassified: source.provider == Provider::Jira,
            ..JiraData::default()
        };
        self.capture_base_with_privacy_if_revision(
            expected,
            base,
            source,
            connection_generation,
            privacy,
        )
        .await
    }

    pub async fn capture_base_with_privacy_if_revision(
        self: &Arc<Self>,
        expected: u64,
        base: Document,
        source: SourceMetadata,
        connection_generation: u64,
        privacy: JiraData,
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
        let candidate_input = work.candidate_input.clone();
        let jira_privacy = WorkPrivacy {
            base: privacy,
            candidate: work.jira_privacy.candidate.clone(),
        };
        self.update(Some(expected), Some(connection_generation), move || {
            Ok(Work {
                base: Some(base),
                candidate,
                base_source: Some(source),
                candidate_input,
                jira_privacy,
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
        let quota = match &self.budget {
            Some(budget) => Some((permit(&budget.jobs)?, permit(&self.jobs)?)),
            None => None,
        };
        let workspace = Arc::clone(self);
        // Finish commit and publication even if the requesting browser disconnects.
        tokio::spawn(async move {
            let _quota = quota;
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
            // Detached commits retain compute capacity after an HTTP client disconnects.
            let compute = match &workspace.budget {
                Some(budget) => Some(
                    Arc::clone(&budget.compute)
                        .acquire_owned()
                        .await
                        .map_err(|_| capacity())?,
                ),
                None => None,
            };
            let current = tokio::task::spawn_blocking(move || {
                let _compute = compute;
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
    if !work.jira_privacy.base.valid()
        || !work.jira_privacy.candidate.valid()
        || (work.base.is_none() && work.jira_privacy.base != JiraData::default())
        || (work.candidate.is_none() && work.jira_privacy.candidate != JiraData::default())
    {
        return Err(AppError::bad_request(
            "El inventario privado de Jira no es válido.",
        ));
    }
    if work.base.is_none() && work.base_source.is_some() {
        return Err(AppError::bad_request(
            "La procedencia requiere un documento original.",
        ));
    }
    if (work.base.is_none() && work.base_input.is_some())
        || (work.candidate.is_none() && work.candidate_input.is_some())
        || (work.base_source.is_some() && work.base_input.is_some())
    {
        return Err(AppError::bad_request(
            "La procedencia requiere el documento correspondiente.",
        ));
    }
    if let Some(source) = &work.base_source
        && (source
            .title
            .as_ref()
            .is_some_and(|s| s.len() > 1024 || s.chars().any(char::is_control))
            || source.converter.as_ref().is_some_and(|s| {
                s.is_empty()
                    || s.len() > 64
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            })
            || source.warnings.len() > 64
            || source
                .warnings
                .iter()
                .any(|s| s.is_empty() || s.len() > 1024 || s.chars().any(char::is_control)))
    {
        return Err(AppError::bad_request(
            "Los metadatos de conversión no son válidos.",
        ));
    }
    for document in work.base.iter().chain(work.candidate.iter()) {
        if document.markdown.len() as u64 > MAX_DOCUMENT_BYTES {
            return Err(AppError(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "Cada documento admite hasta 2 MiB.".into(),
            ));
        }
        if validate_labels && !diff::within_hosted_complexity(&document.markdown) {
            return Err(AppError(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "Este Markdown es demasiado complejo para la vista alojada. Reduce las líneas o simplifica su formato."
                    .into(),
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
        base_input: work.base_input.clone(),
        candidate_input: work.candidate_input.clone(),
    });
    if validate_labels {
        struct Size(usize);
        impl std::io::Write for Size {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 += bytes.len();
                if self.0 > 24 * 1024 * 1024 {
                    return Err(std::io::Error::other("render budget"));
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        serde_json::to_writer(&mut Size(0), &snapshot).map_err(|_| {
            AppError(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "La vista calculada supera 24 MiB. Reduce el tamaño o la complejidad del Markdown."
                    .into(),
            )
        })?;
    }
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
            title: None,
            converter: None,
            warnings: vec![],
        }
    }

    fn privacy_fixture() -> (Database, std::path::PathBuf, std::path::PathBuf) {
        let root = Database::new();
        std::fs::create_dir(&root.0).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let data = root.0.join("data");
        let journal = root.0.join("journal.json");
        write_privacy_journal(&journal, 0, serde_json::json!({}));
        (root, data, journal)
    }

    fn write_privacy_journal(path: &std::path::Path, generation: u64, accounts: serde_json::Value) {
        let next = path.with_extension("next");
        std::fs::write(
            &next,
            serde_json::to_vec(
                &serde_json::json!({"version":1,"generation":generation,"revision_floor":generation * crate::erasure::REVISION_SPAN,"accounts":accounts}),
            )
            .unwrap(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&next, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::fs::rename(next, path).unwrap();
    }

    fn personal_data(id: &str, at: u64) -> JiraData {
        JiraData {
            accounts: std::collections::BTreeMap::from([(id.into(), at)]),
            unclassified: false,
        }
    }

    #[tokio::test]
    async fn external_erasure_filters_restored_copies_for_all_owners_before_serving() {
        let (root, data, journal) = privacy_fixture();
        {
            let store = Workspaces::persistent_with_barrier(&data, &journal).unwrap();
            for (subject, base, candidate, base_data, candidate_data) in [
                (
                    "alice",
                    "Original-A-to-remove",
                    Some("Proposal-A-to-remove"),
                    personal_data("synthetic:closed", 100),
                    JiraData::default(),
                ),
                (
                    "bob",
                    "Original-B-to-keep\r\n",
                    Some("Proposal-B-to-remove"),
                    personal_data("synthetic:other", 100),
                    personal_data("synthetic:closed", 100),
                ),
                (
                    "charlie",
                    "Fresh-C-to-keep",
                    None,
                    personal_data("synthetic:updated", 300),
                    JiraData::default(),
                ),
                (
                    "delta",
                    "Stale-D-to-remove",
                    None,
                    personal_data("synthetic:updated", 100),
                    JiraData::default(),
                ),
            ] {
                let workspace = store.for_user(&identity("issuer", subject)).await.unwrap();
                workspace
                    .update(Some(0), None, move || {
                        Ok(Work {
                            base: Some(document(base)),
                            candidate: candidate.map(document),
                            jira_privacy: WorkPrivacy {
                                base: base_data,
                                candidate: candidate_data,
                            },
                            ..Work::default()
                        })
                    })
                    .await
                    .unwrap();
            }
            connect(&store, &identity("issuer", "alice"), 0);
        }
        let restored = root.0.join("restored");
        std::fs::create_dir(&restored).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&restored, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::copy(data.join("work.sqlite3"), restored.join("work.sqlite3")).unwrap();
        {
            let store = Workspaces::persistent_with_barrier(&data, &journal).unwrap();
            store
                .for_user(&identity("issuer", "bob"))
                .await
                .unwrap()
                .propose_if_revision(1, document("Proposal-B-to-remove"))
                .await
                .unwrap();
        }
        write_privacy_journal(
            &journal,
            1,
            serde_json::json!({
                "synthetic:closed":{"status":"closed","received_at":200},
                "synthetic:updated":{"status":"updated","received_at":200}
            }),
        );
        for path in [&data, &restored] {
            {
                let store = Workspaces::persistent_with_barrier(path, &journal).unwrap();
                for subject in ["alice", "delta"] {
                    let work = store
                        .for_user(&identity("issuer", subject))
                        .await
                        .unwrap()
                        .work()
                        .await;
                    assert!(work.base.is_none() && work.candidate.is_none());
                    assert_eq!(work.revision, crate::erasure::REVISION_SPAN);
                    assert_eq!(work.jira_privacy, WorkPrivacy::default());
                }
                let bob = store
                    .for_user(&identity("issuer", "bob"))
                    .await
                    .unwrap()
                    .work()
                    .await;
                assert_eq!(
                    bob.base.as_ref().unwrap().markdown,
                    "Original-B-to-keep\r\n"
                );
                assert!(bob.candidate.is_none());
                let stale = store
                    .for_user(&identity("issuer", "bob"))
                    .await
                    .unwrap()
                    .propose_if_revision(2, document("stale erased name"))
                    .await;
                assert_eq!(stale.err().unwrap().0, axum::http::StatusCode::CONFLICT);
                assert_eq!(bob.revision, crate::erasure::REVISION_SPAN);
                let charlie = store
                    .for_user(&identity("issuer", "charlie"))
                    .await
                    .unwrap();
                assert_eq!(charlie.work().await.revision, 1);
                assert_eq!(
                    charlie.work().await.base.as_ref().unwrap().markdown,
                    "Fresh-C-to-keep"
                );
                assert_eq!(
                    store
                        .storage
                        .as_ref()
                        .unwrap()
                        .read_connection(&identity("issuer", "alice"), "memos", "notes")
                        .unwrap()
                        .generation,
                    1
                );
                assert!(
                    store
                        .for_user(&identity("issuer", "alice"))
                        .await
                        .unwrap()
                        .propose_if_revision(1, document("late proposal"))
                        .await
                        .is_err()
                );
                assert!(
                    charlie
                        .update(Some(1), None, || Ok(Work {
                            base: Some(document("closed reinsertion")),
                            jira_privacy: WorkPrivacy {
                                base: personal_data("synthetic:closed", 400),
                                ..WorkPrivacy::default()
                            },
                            ..Work::default()
                        }))
                        .await
                        .is_err()
                );
            }
            assert!(Workspaces::persistent(path).is_err());
            for entry in std::fs::read_dir(path).unwrap() {
                let raw = std::fs::read(entry.unwrap().path()).unwrap();
                for erased in [
                    "Original-A-to-remove",
                    "Proposal-A-to-remove",
                    "Proposal-B-to-remove",
                    "Stale-D-to-remove",
                ] {
                    assert!(
                        !raw.windows(erased.len())
                            .any(|bytes| bytes == erased.as_bytes())
                    );
                }
            }
            let again = Workspaces::persistent_with_barrier(path, &journal).unwrap();
            assert_eq!(
                again
                    .for_user(&identity("issuer", "alice"))
                    .await
                    .unwrap()
                    .work()
                    .await
                    .revision,
                crate::erasure::REVISION_SPAN
            );
        }
    }

    #[tokio::test]
    async fn erasure_rolls_back_all_work_if_any_document_cannot_be_classified() {
        let (_root, data, journal) = privacy_fixture();
        {
            let store = Workspaces::persistent(&data).unwrap();
            for (subject, privacy) in [
                ("a", personal_data("synthetic:closed", 100)),
                (
                    "z",
                    JiraData {
                        unclassified: true,
                        ..JiraData::default()
                    },
                ),
            ] {
                store
                    .for_user(&identity("issuer", subject))
                    .await
                    .unwrap()
                    .update(Some(0), None, move || {
                        Ok(Work {
                            base: Some(document("preserve original")),
                            jira_privacy: WorkPrivacy {
                                base: privacy,
                                ..WorkPrivacy::default()
                            },
                            ..Work::default()
                        })
                    })
                    .await
                    .unwrap();
            }
        }
        write_privacy_journal(
            &journal,
            1,
            serde_json::json!({"synthetic:closed":{"status":"closed","received_at":200}}),
        );
        assert!(Workspaces::persistent_with_barrier(&data, &journal).is_err());
        assert!(Workspaces::persistent(&data).is_err());
        let db = rusqlite::Connection::open(data.join("work.sqlite3")).unwrap();
        assert_eq!(db.query_row("SELECT count(*) FROM current_work WHERE revision=1 AND base_markdown='preserve original'",[],|r|r.get::<_,u32>(0)).unwrap(),2);
        assert_eq!(
            db.query_row("SELECT generation FROM privacy_guard", [], |r| r
                .get::<_, u32>(0))
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn privacy_revision_ranges_never_overlap_or_wrap() {
        let (_root, data, journal) = privacy_fixture();
        let owner = identity("issuer", "alice");
        let last = crate::erasure::REVISION_SPAN - 1;
        {
            let store = Workspaces::persistent_with_barrier(&data, &journal).unwrap();
            store
                .for_user(&owner)
                .await
                .unwrap()
                .replace_if_revision(0, document("keep original"), None)
                .await
                .unwrap();
            store
                .storage
                .as_ref()
                .unwrap()
                .database_for_test()
                .execute("UPDATE current_work SET revision=?1", [last as i64])
                .unwrap();
        }
        {
            let store = Workspaces::persistent_with_barrier(&data, &journal).unwrap();
            let workspace = store.for_user(&owner).await.unwrap();
            assert_eq!(
                workspace
                    .propose_if_revision(last, document("overlap"))
                    .await
                    .err()
                    .unwrap()
                    .0,
                axum::http::StatusCode::BAD_REQUEST
            );
            assert_eq!(workspace.work().await.revision, last);
        }
        write_privacy_journal(&journal, 1, serde_json::json!({}));
        let store = Workspaces::persistent_with_barrier(&data, &journal).unwrap();
        let workspace = store.for_user(&owner).await.unwrap();
        workspace
            .propose_if_revision(last, document("next era"))
            .await
            .unwrap();
        assert_eq!(
            workspace.work().await.revision,
            crate::erasure::REVISION_SPAN
        );
    }

    #[tokio::test]
    async fn changed_missing_or_rolled_back_erasure_journal_closes_access() {
        let (_root, data, journal) = privacy_fixture();
        {
            let store = Workspaces::persistent_with_barrier(&data, &journal).unwrap();
            let workspace = store.for_user(&identity("issuer", "alice")).await.unwrap();
            workspace
                .replace_if_revision(0, document("safe current"), None)
                .await
                .unwrap();
            write_privacy_journal(&journal, 1, serde_json::json!({}));
            assert!(store.for_user(&identity("issuer", "alice")).await.is_err());
            assert!(store.check_storage().await.is_err());
            assert!(
                workspace
                    .propose_if_revision(1, document("late write"))
                    .await
                    .is_err()
            );
            assert_eq!(workspace.work().await.revision, 1);
        }
        drop(Workspaces::persistent_with_barrier(&data, &journal).unwrap());
        write_privacy_journal(&journal, 0, serde_json::json!({}));
        assert!(Workspaces::persistent_with_barrier(&data, &journal).is_err());
        std::fs::remove_file(&journal).unwrap();
        assert!(Workspaces::persistent_with_barrier(&data, &journal).is_err());
        assert!(Workspaces::persistent(&data).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn erasure_journal_requires_private_external_regular_valid_input() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (root, data, journal) = privacy_fixture();
        drop(Workspaces::persistent_with_barrier(&data, &journal).unwrap());
        std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Workspaces::persistent_with_barrier(&data, &journal).is_err());
        std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = root.0.join("linked.json");
        symlink(&journal, &link).unwrap();
        assert!(Workspaces::persistent_with_barrier(&data, &link).is_err());
        let inside = data.join("journal.json");
        std::fs::copy(&journal, &inside).unwrap();
        assert!(Workspaces::persistent_with_barrier(&data, &inside).is_err());
        let hard = root.0.join("hard.json");
        std::fs::hard_link(&journal, &hard).unwrap();
        assert!(Workspaces::persistent_with_barrier(&data, &journal).is_err());
        std::fs::remove_file(hard).unwrap();
        {
            let store = Workspaces::persistent_with_barrier(&data, &journal).unwrap();
            std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o400)).unwrap();
            assert!(store.storage.as_ref().unwrap().check_health().is_err());
            std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::set_permissions(&root.0, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(store.storage.as_ref().unwrap().check_health().is_err());
            std::fs::set_permissions(&root.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        for accounts in [
            serde_json::json!({"unknown":{"status":"closed","received_at":100}}),
            serde_json::json!({"synthetic:alice":{"status":"unexpected","received_at":100}}),
            serde_json::json!({"synthetic:alice":{"status":"closed","received_at":0}}),
        ] {
            write_privacy_journal(&journal, 1, accounts);
            assert!(Workspaces::persistent_with_barrier(&data, &journal).is_err());
        }
        std::fs::write(&journal, "sensitive-diagnostic-must-not-appear").unwrap();
        let error = Workspaces::persistent_with_barrier(&data, &journal)
            .err()
            .unwrap();
        assert!(!error.to_string().contains("sensitive-diagnostic"));
    }

    #[tokio::test]
    async fn jira_inventory_survives_derived_proposals_source_changes_restart_and_owner_isolation()
    {
        let directory = Database::new();
        let owner = identity("issuer", "alice");
        let inventory = |id: &str, at| JiraData {
            accounts: std::collections::BTreeMap::from([(id.into(), at)]),
            unclassified: false,
        };
        let mut metadata = source();
        metadata.provider = Provider::Jira;
        {
            let store = Workspaces::persistent(&directory.0).unwrap();
            store
                .storage
                .as_ref()
                .unwrap()
                .replace_connection(
                    &owner,
                    "jira",
                    "notes",
                    0,
                    Some(crate::storage::EncryptedCredential {
                        nonce: [0; 12],
                        ciphertext: vec![0; 16],
                    }),
                )
                .unwrap();
            let workspace = store.for_user(&owner).await.unwrap();
            workspace
                .capture_base_with_privacy_if_revision(
                    0,
                    document("first source"),
                    metadata.clone(),
                    1,
                    inventory("synthetic:first", 100),
                )
                .await
                .unwrap();
            workspace
                .propose_if_revision(1, document("derived proposal"))
                .await
                .unwrap();
            workspace
                .capture_base_with_privacy_if_revision(
                    2,
                    document("second source"),
                    metadata.clone(),
                    1,
                    inventory("synthetic:second", 200),
                )
                .await
                .unwrap();
            assert_eq!(
                workspace.work().await.jira_privacy.candidate,
                inventory("synthetic:first", 100)
            );
            workspace
                .propose_if_revision(3, document("revised derived proposal"))
                .await
                .unwrap();
            let before = workspace.work().await;
            assert_eq!(before.jira_privacy.candidate.accounts.len(), 2);
            assert!(
                workspace
                    .capture_base_with_privacy_if_revision(
                        3,
                        document("late source"),
                        metadata.clone(),
                        1,
                        inventory("synthetic:late", 300)
                    )
                    .await
                    .is_err()
            );
            assert_eq!(workspace.work().await.jira_privacy, before.jira_privacy);
            assert!(
                !serde_json::to_string(before.as_ref())
                    .unwrap()
                    .contains("jira_privacy")
            );
            assert_eq!(
                store
                    .for_user(&identity("issuer", "bob"))
                    .await
                    .unwrap()
                    .work()
                    .await
                    .jira_privacy,
                WorkPrivacy::default()
            );
        }
        {
            let store = Workspaces::persistent(&directory.0).unwrap();
            let workspace = store.for_user(&owner).await.unwrap();
            assert_eq!(workspace.work().await.revision, 4);
            assert_eq!(
                workspace.work().await.jira_privacy.candidate.accounts["synthetic:first"],
                100
            );
            workspace
                .replace_with_inputs_if_revision(
                    4,
                    document("manual base"),
                    Some(document("revised derived proposal")),
                    Some(InputKind::File),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(
                workspace.work().await.jira_privacy.base,
                JiraData::default()
            );
            assert_eq!(
                workspace.work().await.jira_privacy.candidate.accounts.len(),
                2
            );
            workspace
                .propose_with_input_if_revision(5, document("new manual file"), InputKind::File)
                .await
                .unwrap();
            assert_eq!(workspace.work().await.jira_privacy, WorkPrivacy::default());
            workspace
                .capture_base_with_privacy_if_revision(
                    6,
                    document("latest source"),
                    metadata,
                    1,
                    inventory("synthetic:first", 400),
                )
                .await
                .unwrap();
            workspace.clear_if_revision(7).await.unwrap();
        }
        let store = Workspaces::persistent(&directory.0).unwrap();
        assert_eq!(
            store
                .for_user(&owner)
                .await
                .unwrap()
                .work()
                .await
                .jira_privacy,
            WorkPrivacy::default()
        );
    }

    #[tokio::test]
    async fn schema_four_jira_is_preserved_and_flagged_for_reclassification() {
        let directory = Database::new();
        let owner = identity("issuer", "alice");
        {
            let store = Workspaces::persistent(&directory.0).unwrap();
            let workspace = store.for_user(&owner).await.unwrap();
            workspace
                .replace_if_revision(
                    0,
                    document("legacy original"),
                    Some(document("legacy proposal")),
                )
                .await
                .unwrap();
        }
        let database = rusqlite::Connection::open(directory.0.join("work.sqlite3")).unwrap();
        let mut metadata = source();
        metadata.provider = Provider::Jira;
        database
            .execute(
                "UPDATE current_work SET base_source=?1, base_input=NULL",
                [serde_json::to_string(&metadata).unwrap()],
            )
            .unwrap();
        database
            .execute_batch(
                "DROP TABLE privacy_guard; ALTER TABLE current_work DROP COLUMN jira_privacy; PRAGMA user_version=4;",
            )
            .unwrap();
        drop(database);
        let store = Workspaces::persistent(&directory.0).unwrap();
        let workspace = store.for_user(&owner).await.unwrap();
        let work = workspace.work().await;
        assert_eq!(work.base.as_ref().unwrap().markdown, "legacy original");
        assert_eq!(work.candidate.as_ref().unwrap().markdown, "legacy proposal");
        assert!(work.jira_privacy.base.unclassified && work.jira_privacy.candidate.unclassified);
        assert!(work.jira_privacy.base.accounts.is_empty());
        workspace
            .propose_if_revision(1, document("new derived proposal"))
            .await
            .unwrap();
        assert!(workspace.work().await.jira_privacy.candidate.unclassified);
    }

    #[tokio::test]
    async fn input_capture_is_durable_preserves_unchanged_sides_and_clears_on_delete() {
        let directory = Database::new();
        let owner = identity("issuer", "alice");
        let saved;
        {
            let store = Workspaces::persistent(&directory.0).unwrap();
            let workspace = store.for_user(&owner).await.unwrap();
            let first = workspace
                .replace_with_inputs_if_revision(
                    0,
                    document("original"),
                    Some(document("proposal")),
                    Some(InputKind::File),
                    Some(InputKind::Text),
                )
                .await
                .unwrap();
            assert_eq!(first.base_input.as_ref().unwrap().kind, InputKind::File);
            assert_eq!(
                first.candidate_input.as_ref().unwrap().kind,
                InputKind::Text
            );
            assert!(first.base_input.as_ref().unwrap().captured_at > 0);
            let same = workspace
                .replace_with_inputs_if_revision(
                    1,
                    document("original"),
                    Some(document("proposal")),
                    None,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(same.base_input, first.base_input);
            assert_eq!(same.candidate_input, first.candidate_input);
            let proposed = workspace
                .propose_with_input_if_revision(2, document("agent proposal"), InputKind::Agent)
                .await
                .unwrap();
            assert_eq!(proposed.base_input, first.base_input);
            assert_eq!(
                proposed.candidate_input.as_ref().unwrap().kind,
                InputKind::Agent
            );
            connect(&store, &owner, 0);
            let captured = workspace
                .capture_base_if_revision(3, document("remote original"), source(), 1)
                .await
                .unwrap();
            assert!(captured.base_input.is_none());
            assert_eq!(captured.candidate_input, proposed.candidate_input);
            let unchanged = workspace
                .replace_if_revision(
                    4,
                    document("remote original"),
                    Some(document("agent proposal")),
                )
                .await
                .unwrap();
            assert!(unchanged.base_source.is_some());
            assert_eq!(unchanged.candidate_input, captured.candidate_input);
            saved = unchanged.candidate_input.clone();
            assert!(
                serde_json::to_value(workspace.work().await)
                    .unwrap()
                    .get("candidate_input")
                    .is_none()
            );
        }
        let store = Workspaces::persistent(&directory.0).unwrap();
        let workspace = store.for_user(&owner).await.unwrap();
        assert_eq!(workspace.snapshot().await.candidate_input, saved);
        let cleared = workspace.clear_if_revision(5).await.unwrap();
        assert!(
            cleared.base_input.is_none()
                && cleared.candidate_input.is_none()
                && cleared.base_source.is_none()
        );
    }

    #[tokio::test]
    async fn schema_three_work_migrates_without_inventing_capture_times_or_changing_content() {
        let directory = Database::new();
        let owner = identity("issuer", "alice");
        {
            let store = Workspaces::persistent(&directory.0).unwrap();
            store
                .for_user(&owner)
                .await
                .unwrap()
                .replace_if_revision(0, document("legacy"), Some(document("proposal")))
                .await
                .unwrap();
        }
        let connection = rusqlite::Connection::open(directory.0.join("work.sqlite3")).unwrap();
        connection.execute_batch("DROP TABLE privacy_guard; ALTER TABLE current_work DROP COLUMN jira_privacy; ALTER TABLE current_work DROP COLUMN base_input; ALTER TABLE current_work DROP COLUMN candidate_input; PRAGMA user_version=3;").unwrap();
        drop(connection);
        let store = Workspaces::persistent(&directory.0).unwrap();
        let workspace = store.for_user(&owner).await.unwrap();
        let work = workspace.work().await;
        assert_eq!(work.revision, 1);
        assert_eq!(work.base.as_ref().unwrap().markdown, "legacy");
        assert_eq!(work.candidate.as_ref().unwrap().markdown, "proposal");
        assert!(
            workspace.snapshot().await.base_input.is_none()
                && workspace.snapshot().await.candidate_input.is_none()
        );
    }

    #[tokio::test]
    async fn hosted_quotas_isolate_owners_and_release_closed_streams() {
        let store = Workspaces::default();
        let alice = identity("https://identity.example", "alice");
        let bob = identity("https://identity.example", "bob");
        let first = store
            .request_permits(&alice, RequestKind::Content)
            .await
            .unwrap();
        let second = store
            .request_permits(&alice, RequestKind::Content)
            .await
            .unwrap();
        let workspace = store.entry(&alice).await.unwrap();
        let waiting = Arc::clone(&workspace.requests)
            .acquire_many_owned(2)
            .await
            .unwrap();
        assert_eq!(
            store
                .request_permits(&alice, RequestKind::Content)
                .await
                .unwrap_err()
                .0,
            axum::http::StatusCode::TOO_MANY_REQUESTS
        );
        drop(first);
        let other = store
            .request_permits(&bob, RequestKind::Content)
            .await
            .unwrap();
        drop((second, other, waiting));
        let mut streams = Vec::new();
        for _ in 0..3 {
            streams.push(
                store
                    .request_permits(&alice, RequestKind::Events)
                    .await
                    .unwrap(),
            );
        }
        assert_eq!(
            store
                .request_permits(&alice, RequestKind::Events)
                .await
                .unwrap_err()
                .0,
            axum::http::StatusCode::TOO_MANY_REQUESTS
        );
        let other_stream = store
            .request_permits(&bob, RequestKind::Events)
            .await
            .unwrap();
        streams.pop();
        let reopened = store
            .request_permits(&alice, RequestKind::Events)
            .await
            .unwrap();
        drop((streams, reopened, other_stream));
        assert_eq!(store.budget.events.available_permits(), 40);
        let all = Arc::clone(&store.budget.requests)
            .acquire_many_owned(16)
            .await
            .unwrap();
        assert_eq!(
            store
                .request_permits(&bob, RequestKind::Content)
                .await
                .unwrap_err()
                .0,
            axum::http::StatusCode::TOO_MANY_REQUESTS
        );
        drop(all);
        assert!(
            store
                .request_permits(&bob, RequestKind::Content)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn resident_quota_evicts_only_idle_workspaces_and_restores_their_work() {
        let directory = Database::new();
        let store = Workspaces::persistent(&directory.0).unwrap();
        let owners: Vec<_> = (0..11)
            .map(|n| identity("https://identity.example", &format!("owner-{n}")))
            .collect();
        let mut streams = Vec::new();
        for owner in &owners[..10] {
            let workspace = store.for_user(owner).await.unwrap();
            workspace
                .replace_if_revision(0, document(&owner.subject), None)
                .await
                .unwrap();
            streams.push(
                store
                    .request_permits(owner, RequestKind::Events)
                    .await
                    .unwrap(),
            );
        }
        assert_eq!(
            store.for_user(&owners[10]).await.err().unwrap().0,
            axum::http::StatusCode::TOO_MANY_REQUESTS
        );
        streams.remove(0);
        let eleventh = store
            .request_permits(&owners[10], RequestKind::Content)
            .await
            .unwrap();
        assert_eq!(store.users.lock().await.len(), 10);
        assert!(!store.users.lock().await.contains_key(&owners[0]));
        drop(eleventh);
        let restored = store.for_user(&owners[0]).await.unwrap();
        assert_eq!(restored.work().await.revision, 1);
        assert_eq!(
            restored.work().await.base.as_ref().unwrap().markdown,
            owners[0].subject
        );
        assert_eq!(store.users.lock().await.len(), 10);
    }

    #[tokio::test]
    async fn disconnected_updates_retain_compute_and_queued_job_quotas() {
        let store = Workspaces::default();
        let owner = identity("https://identity.example", "alice");
        let workspace = store.for_user(&owner).await.unwrap();
        let occupied = Arc::clone(&store.budget.compute)
            .acquire_many_owned(2)
            .await
            .unwrap();
        let caller = Arc::clone(&workspace);
        let request = tokio::spawn(async move {
            caller
                .replace_if_revision(0, document("# retained"), None)
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while workspace.jobs.available_permits() == 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        request.abort();
        assert_eq!(workspace.jobs.available_permits(), 1);
        assert_eq!(store.budget.jobs.available_permits(), 9);
        drop(occupied);
        tokio::time::timeout(Duration::from_secs(2), async {
            while workspace.snapshot().await.revision == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            workspace.work().await.base.as_ref().unwrap().markdown,
            "# retained"
        );
        assert_eq!(store.budget.compute.available_permits(), 2);
    }

    #[tokio::test]
    async fn hosted_complexity_rejection_keeps_the_last_revision() {
        let store = Workspaces::default();
        let workspace = store
            .for_user(&identity("https://identity.example", "alice"))
            .await
            .unwrap();
        workspace
            .replace_if_revision(0, document("# retained"), None)
            .await
            .unwrap();
        for markdown in [
            "x\n".repeat(10_001),
            "x\r".repeat(10_001),
            "*a* ".repeat(12_000),
            format!("{}nested", "> ".repeat(129)),
            "<div>".repeat(257),
            "\x01".repeat(2 * 1024 * 1024),
        ] {
            let error = workspace
                .replace_if_revision(1, document(&markdown), None)
                .await
                .err()
                .unwrap();
            assert_eq!(error.0, axum::http::StatusCode::PAYLOAD_TOO_LARGE);
            assert_eq!(workspace.work().await.revision, 1);
            assert_eq!(
                workspace.work().await.base.as_ref().unwrap().markdown,
                "# retained"
            );
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

    #[cfg(unix)]
    #[tokio::test]
    async fn checkpoints_copy_private_state_without_stopping_or_changing_work() {
        let (root, data, journal) = privacy_fixture();
        let output = root.0.join("checkpoints");
        std::fs::create_dir(&output).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let store = Workspaces::persistent_with_barrier(&data, &journal).unwrap();
        for name in ["alice", "bob"] {
            store
                .for_user(&identity("issuer", name))
                .await
                .unwrap()
                .update(Some(0), None, move || {
                    Ok(Work {
                        base: Some(document(name)),
                        candidate: Some(document("proposal\r\n🦀")),
                        jira_privacy: WorkPrivacy {
                            base: personal_data("synthetic:private", 100),
                            candidate: JiraData::default(),
                        },
                        ..Work::default()
                    })
                })
                .await
                .unwrap();
        }
        let alice = identity("issuer", "alice");
        connect(&store, &alice, 0);
        store.checkpoint(output.clone()).await.unwrap();
        let checkpoint = output.join("work.sqlite3");
        assert!(!output.join(".work.sqlite3.pending").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let info = std::fs::metadata(&checkpoint).unwrap();
            assert_eq!(info.permissions().mode() & 0o777, 0o600);
            assert_eq!(info.nlink(), 1);
        }
        let bytes = std::fs::read(&checkpoint).unwrap();
        assert!(store.checkpoint(output.clone()).await.is_err());
        assert_eq!(std::fs::read(&checkpoint).unwrap(), bytes);
        let live = store.for_user(&alice).await.unwrap();
        live.propose_if_revision(1, document("later"))
            .await
            .unwrap();
        store.check_storage().await.unwrap();
        let recovered = Workspaces::persistent_with_barrier(&output, &journal).unwrap();
        for name in ["alice", "bob"] {
            let work = recovered
                .for_user(&identity("issuer", name))
                .await
                .unwrap()
                .work()
                .await;
            assert_eq!(work.revision, 1);
            assert!(work.base == Some(document(name)));
            assert!(work.candidate == Some(document("proposal\r\n🦀")));
            assert_eq!(
                work.jira_privacy.base,
                personal_data("synthetic:private", 100)
            );
            assert!(
                !serde_json::to_string(&work)
                    .unwrap()
                    .contains("synthetic:private")
            );
        }
        let connection = recovered
            .storage
            .as_ref()
            .unwrap()
            .read_connection(&alice, "memos", "notes")
            .unwrap();
        assert_eq!(connection.generation, 1);
        assert_eq!(connection.encrypted.unwrap().ciphertext, vec![0; 16]);
        assert!(live.work().await.candidate == Some(document("later")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn checkpoints_reject_unsafe_destinations_and_changed_authority() {
        let (root, data, journal) = privacy_fixture();
        let store = Workspaces::persistent_with_barrier(&data, &journal).unwrap();
        assert!(store.checkpoint_directory(&data).is_err());
        assert!(store.checkpoint_directory(&data.join("nested")).is_err());
        assert!(store.checkpoint_directory(&root.0.join("missing")).is_err());
        assert!(
            Workspaces::default()
                .checkpoint(root.0.clone())
                .await
                .is_err()
        );
        let output = root.0.join("operator");
        std::fs::create_dir(&output).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(store.checkpoint_directory(&output).is_err());
            std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o700)).unwrap();
            let link = root.0.join("operator-link");
            symlink(&output, &link).unwrap();
            assert!(store.checkpoint_directory(&link).is_err());
            symlink(root.0.join("absent"), output.join("work.sqlite3")).unwrap();
            assert!(store.checkpoint(output.clone()).await.is_err());
            assert!(output.join("work.sqlite3").is_symlink());
            std::fs::remove_file(output.join("work.sqlite3")).unwrap();
        }
        std::fs::create_dir(root.0.join(".git")).unwrap();
        assert!(store.checkpoint_directory(&output).is_err());
        std::fs::remove_dir(root.0.join(".git")).unwrap();
        let pending = output.join(".work.sqlite3.pending");
        std::fs::write(&pending, b"operator-owned").unwrap();
        assert!(store.checkpoint(output.clone()).await.is_err());
        assert_eq!(std::fs::read(&pending).unwrap(), b"operator-owned");
        std::fs::remove_file(&pending).unwrap();
        write_privacy_journal(&journal, 1, serde_json::json!({}));
        assert!(store.checkpoint(output.clone()).await.is_err());
        assert!(!pending.exists() && !output.join("work.sqlite3").exists());
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
                "DROP TABLE privacy_guard; ALTER TABLE current_work DROP COLUMN jira_privacy; ALTER TABLE current_work DROP COLUMN base_source; ALTER TABLE current_work DROP COLUMN base_input; ALTER TABLE current_work DROP COLUMN candidate_input; PRAGMA user_version=2;",
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
            6
        );
    }

    #[test]
    fn ordinary_document_inputs_cannot_claim_source_metadata() {
        for key in ["source", "base_source", "jira_privacy"] {
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
            6
        );
        let columns: i32 = connection
            .query_row(
                "SELECT count(*) FROM pragma_table_info('current_work')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(columns, 11);
        connection.pragma_update(None, "user_version", 7).unwrap();
        drop(connection);
        assert!(Workspaces::persistent(&directory.0).is_err());
        let connection = rusqlite::Connection::open(directory.0.join("work.sqlite3")).unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            7
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
