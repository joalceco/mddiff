use std::{collections::HashMap, sync::Arc, time::Instant};

use serde::Serialize;
use tokio::sync::{Mutex, RwLock, watch};

use crate::{AppError, MAX_DOCUMENT_BYTES, diff};

/// Identity established by a trusted authentication adapter, never request fields.
/// The adapter must verify credentials before calling this constructor.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct VerifiedIdentity {
    issuer: String,
    subject: String,
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
pub struct Document {
    pub label: String,
    pub markdown: String,
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
}

impl Workspaces {
    pub async fn for_user(&self, identity: &VerifiedIdentity) -> Arc<Workspace> {
        Arc::clone(
            self.users
                .lock()
                .await
                .entry(identity.clone())
                .or_insert_with(|| Arc::new(Workspace::new())),
        )
    }
}

pub struct Workspace {
    snapshot: RwLock<Arc<Snapshot>>,
    updates: Mutex<()>,
    revisions: watch::Sender<u64>,
}

impl Workspace {
    pub(crate) fn new() -> Self {
        let (revisions, _) = watch::channel(0);
        Self {
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

    pub async fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&*self.snapshot.read().await)
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.revisions.subscribe()
    }

    pub async fn replace_content(
        &self,
        base: Document,
        candidate: Option<Document>,
    ) -> Result<Arc<Snapshot>, AppError> {
        self.load(move || Ok((base, candidate))).await
    }

    pub(crate) async fn load(
        &self,
        read: impl FnOnce() -> Result<(Document, Option<Document>), AppError> + Send + 'static,
    ) -> Result<Arc<Snapshot>, AppError> {
        // Hold the owner's lock from loading through commit; another owner never waits here.
        let _update = self.updates.lock().await;
        let started = Instant::now();
        let (base_path, candidate_path, comparison) = tokio::task::spawn_blocking(move || {
            let (base, candidate) = read()?;
            if base.markdown.len() as u64 > MAX_DOCUMENT_BYTES
                || candidate
                    .as_ref()
                    .is_some_and(|d| d.markdown.len() as u64 > MAX_DOCUMENT_BYTES)
            {
                return Err(AppError(
                    axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                    "Cada documento admite hasta 2 MiB.".into(),
                ));
            }
            let comparison = diff::compare(
                &base.markdown,
                candidate
                    .as_ref()
                    .map_or(base.markdown.as_str(), |d| d.markdown.as_str()),
            );
            Ok((base.label, candidate.map(|d| d.label), comparison))
        })
        .await
        .map_err(|_| AppError::internal("No se pudo procesar el documento."))??;
        // shortcut: current work is in memory; add durable storage in S04 before hosting.
        let snapshot = Arc::new(Snapshot {
            revision: self.snapshot.read().await.revision + 1,
            base_path: Some(base_path),
            candidate_path,
            comparison: Some(comparison),
            processing_ms: started.elapsed().as_secs_f64() * 1000.0,
        });
        *self.snapshot.write().await = Arc::clone(&snapshot);
        self.revisions.send_replace(snapshot.revision);
        Ok(snapshot)
    }
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

    #[tokio::test]
    async fn owners_have_separate_content_revisions_events_and_coordination() {
        let store = Workspaces::default();
        let alice = store.for_user(&identity("issuer-a", "alice")).await;
        let bob = store.for_user(&identity("issuer-a", "bob")).await;
        let other_issuer = store.for_user(&identity("issuer-b", "alice")).await;
        assert!(Arc::ptr_eq(
            &alice,
            &store.for_user(&identity("issuer-a", "alice")).await
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
        let workspace = store.for_user(&identity("issuer", "alice")).await;
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
