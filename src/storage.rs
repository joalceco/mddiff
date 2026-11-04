use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use axum::http::StatusCode;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::{
    AppError,
    erasure::{Barrier, REVISION_SPAN},
    privacy::{JiraData, WorkPrivacy},
    workspace::{Document, InputMetadata, SourceMetadata, VerifiedIdentity, Work},
};

pub(crate) struct Storage {
    connection: Mutex<Connection>,
    directory: PathBuf,
    barrier: Option<Barrier>,
}

pub(crate) struct EncryptedCredential {
    pub nonce: [u8; 12],
    pub ciphertext: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct ConnectionRow {
    pub generation: u64,
    pub encrypted: Option<EncryptedCredential>,
}

fn unavailable() -> AppError {
    AppError(StatusCode::SERVICE_UNAVAILABLE,
        "No se pudo guardar o recuperar el trabajo. Verifica permisos, espacio disponible y que solo una instancia use la base; vuelve a intentarlo.".into())
}

pub(crate) fn conflict() -> AppError {
    AppError(
        StatusCode::CONFLICT,
        "El trabajo cambió. Recarga su estado antes de reemplazarlo o borrarlo.".into(),
    )
}

impl Storage {
    pub(crate) fn check_health(&self) -> Result<(), AppError> {
        self.check_barrier()?;
        self.connection
            .lock()
            .map_err(|_| unavailable())?
            .query_row("SELECT 1", [], |_| Ok(()))
            .map_err(|_| unavailable())
    }

    #[cfg(test)]
    pub(crate) fn read_only_for_test(&self, value: bool) {
        self.connection
            .lock()
            .unwrap()
            .pragma_update(None, "query_only", value)
            .unwrap();
    }

    pub(crate) fn open(directory: &Path) -> Result<Self, AppError> {
        Self::open_with_barrier(directory, None)
    }

    pub(crate) fn open_with_barrier(
        directory: &Path,
        barrier: Option<&Path>,
    ) -> Result<Self, AppError> {
        if !directory.exists() {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(directory).map_err(|_| unavailable())?;
        }
        private_path(directory, true)?;
        let path = directory.join("work.sqlite3");
        if !path.exists() {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(&path).map_err(|_| unavailable())?;
        }
        private_path(&path, false)?;
        for suffix in [
            "work.sqlite3-wal",
            "work.sqlite3-shm",
            "work.sqlite3-journal",
        ] {
            let sidecar = directory.join(suffix);
            if fs::symlink_metadata(&sidecar).is_ok() {
                private_path(&sidecar, false)?;
            }
        }
        let mut connection = Connection::open(&path).map_err(|_| unavailable())?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(|_| unavailable())?;
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(|_| unavailable())?;
        if version > 6 {
            return Err(AppError::bad_request(
                "La base usa un esquema más reciente; utiliza una versión compatible de mddiff.",
            ));
        }
        connection
            .execute_batch(
                "PRAGMA locking_mode=EXCLUSIVE; PRAGMA journal_mode=WAL;
            PRAGMA synchronous=FULL; PRAGMA secure_delete=ON;
            BEGIN EXCLUSIVE; COMMIT;",
            )
            .map_err(|_| unavailable())?;
        if version == 0 {
            let transaction = connection.transaction().map_err(|_| unavailable())?;
            transaction
                .execute_batch(
                    "CREATE TABLE current_work (
                issuer TEXT NOT NULL,
                subject TEXT NOT NULL,
                revision INTEGER NOT NULL CHECK (revision > 0),
                base_label TEXT,
                base_markdown TEXT,
                candidate_label TEXT,
                candidate_markdown TEXT,
                PRIMARY KEY (issuer, subject),
                CHECK ((base_label IS NULL) = (base_markdown IS NULL)),
                CHECK ((candidate_label IS NULL) = (candidate_markdown IS NULL)),
                CHECK (base_label IS NOT NULL OR candidate_label IS NULL)
            ) WITHOUT ROWID;
            PRAGMA user_version=1;",
                )
                .map_err(|_| unavailable())?;
            transaction.commit().map_err(|_| unavailable())?;
        }
        if version < 2 {
            let transaction = connection.transaction().map_err(|_| unavailable())?;
            transaction
                .execute_batch(
                    "CREATE TABLE encrypted_connections (
                    issuer TEXT NOT NULL,
                    subject TEXT NOT NULL,
                    provider TEXT NOT NULL CHECK (provider IN ('jira', 'memos')),
                    destination TEXT NOT NULL,
                    generation INTEGER NOT NULL CHECK (generation > 0),
                    nonce BLOB,
                    ciphertext BLOB,
                    PRIMARY KEY (issuer, subject, provider, destination),
                    CHECK ((nonce IS NULL) = (ciphertext IS NULL)),
                    CHECK (nonce IS NULL OR length(nonce) = 12),
                    CHECK (ciphertext IS NULL OR length(ciphertext) BETWEEN 16 AND 65552)
                ) WITHOUT ROWID;
                PRAGMA user_version=2;",
                )
                .map_err(|_| unavailable())?;
            transaction.commit().map_err(|_| unavailable())?;
        }
        if version < 3 {
            let transaction = connection.transaction().map_err(|_| unavailable())?;
            transaction
                .execute_batch(
                    "ALTER TABLE current_work ADD COLUMN base_source TEXT
                    CHECK (base_label IS NOT NULL OR base_source IS NULL);
                 PRAGMA user_version=3;",
                )
                .map_err(|_| unavailable())?;
            transaction.commit().map_err(|_| unavailable())?;
        }
        if version < 4 {
            let transaction = connection.transaction().map_err(|_| unavailable())?;
            transaction
                .execute_batch(
                    "ALTER TABLE current_work ADD COLUMN base_input TEXT
                    CHECK (base_label IS NOT NULL OR base_input IS NULL);
                 ALTER TABLE current_work ADD COLUMN candidate_input TEXT
                    CHECK (candidate_label IS NOT NULL OR candidate_input IS NULL);
                 PRAGMA user_version=4;",
                )
                .map_err(|_| unavailable())?;
            transaction.commit().map_err(|_| unavailable())?;
        }
        if version < 5 {
            let transaction = connection.transaction().map_err(|_| unavailable())?;
            transaction
                .execute_batch(
                    "ALTER TABLE current_work ADD COLUMN jira_privacy TEXT;
                 PRAGMA user_version=5;",
                )
                .map_err(|_| unavailable())?;
            transaction.commit().map_err(|_| unavailable())?;
        }
        if version < 6 {
            let transaction = connection.transaction().map_err(|_| unavailable())?;
            transaction
                .execute_batch(
                    "CREATE TABLE privacy_guard (
                singleton INTEGER PRIMARY KEY CHECK (singleton=1),
                generation INTEGER NOT NULL CHECK (generation>=0),
                revision_floor INTEGER NOT NULL CHECK (revision_floor>=0)
            ); PRAGMA user_version=6;",
                )
                .map_err(|_| unavailable())?;
            transaction.commit().map_err(|_| unavailable())?;
        }
        let directory = fs::canonicalize(directory).map_err(|_| unavailable())?;
        let barrier = barrier
            .map(|path| Barrier::load(path, &directory))
            .transpose()?;
        let store = Self {
            connection: Mutex::new(connection),
            directory,
            barrier,
        };
        store.apply_erasures()?;
        Ok(store)
    }

    pub(crate) fn check_barrier(&self) -> Result<(), AppError> {
        if let Some(barrier) = &self.barrier {
            barrier.unchanged()?;
        }
        Ok(())
    }

    fn apply_erasures(&self) -> Result<(), AppError> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let previous: Option<(i64, i64)> = connection
            .query_row(
                "SELECT generation,revision_floor FROM privacy_guard WHERE singleton=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|_| unavailable())?;
        let Some(barrier) = &self.barrier else {
            return if previous.is_some() {
                Err(AppError::bad_request(
                    "Esta base requiere el registro externo de privacidad; no se permite abrirla sin su barrera.",
                ))
            } else {
                Ok(())
            };
        };
        if previous.is_some_and(|(generation, floor)| {
            generation < 0
                || floor < 0
                || generation > barrier.journal.generation as i64
                || floor > barrier.journal.revision_floor as i64
                || (barrier.journal.generation - generation as u64)
                    .checked_mul(REVISION_SPAN)
                    .and_then(|span| (floor as u64).checked_add(span))
                    .is_none_or(|minimum| barrier.journal.revision_floor < minimum)
        }) {
            return Err(AppError::bad_request(
                "El registro de privacidad es anterior al ya aplicado; conserva el mantenimiento y recupera el registro vigente.",
            ));
        }
        // Once requested, the external barrier remains mandatory even if classification fails.
        connection
            .execute(
                "INSERT OR IGNORE INTO privacy_guard(singleton,generation,revision_floor) VALUES(1,0,0)",
                [],
            )
            .map_err(|_| unavailable())?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| unavailable())?;
        let rows: Vec<(String, String, i64, bool, bool, Option<String>)> = {
            let mut statement = transaction.prepare("SELECT issuer,subject,revision,base_label IS NOT NULL,candidate_label IS NOT NULL,jira_privacy FROM current_work").map_err(|_| unavailable())?;
            statement
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })
                .map_err(|_| unavailable())?
                .collect::<Result<_, _>>()
                .map_err(|_| unavailable())?
        };
        for (issuer, subject, revision, base, candidate, raw) in rows {
            barrier.accept_revision(u64::try_from(revision).map_err(|_| unavailable())?)?;
            let mut inventory: WorkPrivacy = match raw {
                Some(raw) => serde_json::from_str(&raw).map_err(|_| unavailable())?,
                None => WorkPrivacy {
                    base: JiraData {
                        unclassified: base,
                        ..JiraData::default()
                    },
                    candidate: JiraData {
                        unclassified: candidate,
                        ..JiraData::default()
                    },
                },
            };
            let (erase_base, erase_candidate) = barrier.affected(&inventory)?;
            if !(erase_base || erase_candidate) {
                continue;
            }
            let next = revision
                .checked_add(1)
                .filter(|n| *n > 0)
                .ok_or_else(unavailable)?
                .max(barrier.journal.revision_floor as i64);
            barrier.accept_revision(next as u64)?;
            if erase_base {
                inventory.base = JiraData::default();
            }
            if erase_candidate {
                inventory.candidate = JiraData::default();
            }
            transaction
                .execute(
                    "UPDATE current_work SET revision=?3,
                base_label=CASE WHEN ?4 THEN NULL ELSE base_label END,
                base_markdown=CASE WHEN ?4 THEN NULL ELSE base_markdown END,
                base_source=CASE WHEN ?4 THEN NULL ELSE base_source END,
                base_input=CASE WHEN ?4 THEN NULL ELSE base_input END,
                candidate_label=CASE WHEN ?5 THEN NULL ELSE candidate_label END,
                candidate_markdown=CASE WHEN ?5 THEN NULL ELSE candidate_markdown END,
                candidate_input=CASE WHEN ?5 THEN NULL ELSE candidate_input END,
                jira_privacy=?6 WHERE issuer=?1 AND subject=?2",
                    params![
                        issuer,
                        subject,
                        next,
                        erase_base,
                        erase_candidate,
                        serde_json::to_string(&inventory).map_err(|_| unavailable())?
                    ],
                )
                .map_err(|_| unavailable())?;
        }
        transaction
            .execute(
                "INSERT INTO privacy_guard(singleton,generation,revision_floor) VALUES(1,?1,?2)
            ON CONFLICT(singleton) DO UPDATE SET generation=excluded.generation,revision_floor=excluded.revision_floor",
                [barrier.journal.generation as i64,barrier.journal.revision_floor as i64],
            )
            .map_err(|_| unavailable())?;
        barrier.unchanged()?;
        transaction.commit().map_err(|_| unavailable())?;
        if !barrier.journal.accounts.is_empty() {
            let busy: i64 = connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))
                .map_err(|_| unavailable())?;
            if busy != 0 {
                return Err(unavailable());
            }
            connection
                .execute_batch("VACUUM;")
                .map_err(|_| unavailable())?;
            let busy: i64 = connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))
                .map_err(|_| unavailable())?;
            if busy != 0 {
                return Err(unavailable());
            }
        }
        Ok(())
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    #[cfg(test)]
    pub(crate) fn database_for_test(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection.lock().unwrap()
    }

    pub(crate) fn read_connection(
        &self,
        owner: &VerifiedIdentity,
        provider: &str,
        destination: &str,
    ) -> Result<ConnectionRow, AppError> {
        let connection = self.connection.lock().map_err(|_| unavailable())?;
        connection
            .query_row(
                "SELECT generation, nonce, ciphertext FROM encrypted_connections
             WHERE issuer=?1 AND subject=?2 AND provider=?3 AND destination=?4",
                params![owner.issuer, owner.subject, provider, destination],
                |row| {
                    let generation: i64 = row.get(0)?;
                    if generation <= 0 {
                        return Err(rusqlite::Error::IntegralValueOutOfRange(0, generation));
                    }
                    let nonce: Option<Vec<u8>> = row.get(1)?;
                    let ciphertext: Option<Vec<u8>> = row.get(2)?;
                    let encrypted = match (nonce, ciphertext) {
                        (Some(nonce), Some(ciphertext))
                            if (16..=65552).contains(&ciphertext.len()) =>
                        {
                            Some(EncryptedCredential {
                                nonce: nonce
                                    .try_into()
                                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                                ciphertext,
                            })
                        }
                        (None, None) => None,
                        _ => return Err(rusqlite::Error::InvalidQuery),
                    };
                    Ok(ConnectionRow {
                        generation: generation as u64,
                        encrypted,
                    })
                },
            )
            .optional()
            .map(|row| row.unwrap_or_default())
            .map_err(|_| unavailable())
    }

    pub(crate) fn replace_connection(
        &self,
        owner: &VerifiedIdentity,
        provider: &str,
        destination: &str,
        expected: u64,
        encrypted: Option<EncryptedCredential>,
    ) -> Result<u64, AppError> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| unavailable())?;
        let generation: i64 = transaction
            .query_row(
                "SELECT generation FROM encrypted_connections
             WHERE issuer=?1 AND subject=?2 AND provider=?3 AND destination=?4",
                params![owner.issuer, owner.subject, provider, destination],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| unavailable())?
            .unwrap_or(0);
        if u64::try_from(generation).ok() != Some(expected) {
            return Err(crate::connections::connection_conflict());
        }
        let next = generation.checked_add(1).ok_or_else(|| {
            AppError::bad_request("La generación de la conexión alcanzó el límite admitido.")
        })?;
        transaction
            .execute(
                "INSERT INTO encrypted_connections
             (issuer,subject,provider,destination,generation,nonce,ciphertext)
             VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(issuer,subject,provider,destination) DO UPDATE SET
             generation=excluded.generation,nonce=excluded.nonce,ciphertext=excluded.ciphertext",
                params![
                    owner.issuer,
                    owner.subject,
                    provider,
                    destination,
                    next,
                    encrypted.as_ref().map(|value| value.nonce.as_slice()),
                    encrypted.as_ref().map(|value| &value.ciphertext)
                ],
            )
            .map_err(|_| unavailable())?;
        transaction.commit().map_err(|_| unavailable())?;
        Ok(next as u64)
    }

    pub(crate) fn read(&self, owner: &VerifiedIdentity) -> Result<Work, AppError> {
        let connection = self.connection.lock().map_err(|_| unavailable())?;
        connection
            .query_row(
                "SELECT revision, base_label, base_markdown, candidate_label, candidate_markdown, base_source, base_input, candidate_input, jira_privacy
            FROM current_work WHERE issuer=?1 AND subject=?2",
                params![owner.issuer, owner.subject],
                |row| {
                    let base_label: Option<String> = row.get(1)?;
                    let candidate_label: Option<String> = row.get(3)?;
                    let revision: i64 = row.get(0)?;
                    if revision <= 0 {
                        return Err(rusqlite::Error::IntegralValueOutOfRange(0, revision));
                    }
                    let base_source: Option<String> = row.get(5)?;
                    let base_source = base_source.map(|source| serde_json::from_str::<SourceMetadata>(&source)
                        .map_err(|_| rusqlite::Error::InvalidQuery)).transpose()?;
                    let input = |index| -> rusqlite::Result<Option<InputMetadata>> {
                        let raw: Option<String> = row.get(index)?;
                        raw.map(|s| serde_json::from_str(&s).map_err(|_| rusqlite::Error::InvalidQuery)).transpose()
                    };
                    let privacy: Option<String> = row.get(8)?;
                    let jira_privacy = match privacy {
                        Some(raw) => serde_json::from_str::<WorkPrivacy>(&raw)
                            .map_err(|_| rusqlite::Error::InvalidQuery)?,
                        None => {
                            let mut legacy = WorkPrivacy::default();
                            // Earlier proposals may have outlived their Jira source metadata.
                            legacy.base.unclassified = base_label.is_some();
                            legacy.candidate.unclassified = candidate_label.is_some();
                            legacy
                        }
                    };
                    Ok(Work {
                        revision: revision as u64,
                        base: match base_label {
                            Some(label) => Some(Document {
                                label,
                                markdown: row.get(2)?,
                            }),
                            None => None,
                        },
                        candidate: match candidate_label {
                            Some(label) => Some(Document {
                                label,
                                markdown: row.get(4)?,
                            }),
                            None => None,
                        },
                        base_source,
                        base_input: input(6)?,
                        candidate_input: input(7)?,
                        jira_privacy,
                    })
                },
            )
            .optional()
            .map(|work| work.unwrap_or_default())
            .map_err(|_| unavailable())
    }

    pub(crate) fn replace(
        &self,
        owner: &VerifiedIdentity,
        expected: u64,
        work: &Work,
        connection_generation: Option<u64>,
    ) -> Result<u64, AppError> {
        if let Some(barrier) = &self.barrier {
            barrier.accept(&work.jira_privacy)?;
        }
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| unavailable())?;
        let revision: i64 = transaction
            .query_row(
                "SELECT revision FROM current_work WHERE issuer=?1 AND subject=?2",
                params![owner.issuer, owner.subject],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| unavailable())?
            .unwrap_or(0);
        if u64::try_from(revision).ok() != Some(expected) {
            return Err(conflict());
        }
        if let Some(expected_generation) = connection_generation {
            let source = work.base_source.as_ref().ok_or_else(conflict)?;
            let connection: Option<(i64, bool)> = transaction
                .query_row(
                    "SELECT generation,ciphertext IS NOT NULL FROM encrypted_connections
                 WHERE issuer=?1 AND subject=?2 AND provider=?3 AND destination=?4",
                    params![
                        owner.issuer,
                        owner.subject,
                        source.provider.as_str(),
                        source.destination
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|_| unavailable())?;
            if !connection.is_some_and(|(generation, connected)| {
                connected && u64::try_from(generation).ok() == Some(expected_generation)
            }) {
                return Err(crate::connections::connection_conflict());
            }
        }
        let next = revision
            .checked_add(1)
            .ok_or_else(|| AppError::bad_request("La revisión alcanzó el límite admitido."))?;
        if let Some(barrier) = &self.barrier {
            barrier.accept_revision(next as u64)?;
        }
        let source = work
            .base_source
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| unavailable())?;
        let base = work.base.as_ref();
        let candidate = work.candidate.as_ref();
        let base_input = work
            .base_input
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| unavailable())?;
        let candidate_input = work
            .candidate_input
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| unavailable())?;
        let jira_privacy = serde_json::to_string(&work.jira_privacy).map_err(|_| unavailable())?;
        transaction.execute("INSERT INTO current_work
            (issuer,subject,revision,base_label,base_markdown,candidate_label,candidate_markdown,base_source,base_input,candidate_input,jira_privacy)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) ON CONFLICT(issuer,subject) DO UPDATE SET
            revision=excluded.revision,base_label=excluded.base_label,base_markdown=excluded.base_markdown,
            candidate_label=excluded.candidate_label,candidate_markdown=excluded.candidate_markdown,base_source=excluded.base_source,
            base_input=excluded.base_input,candidate_input=excluded.candidate_input,jira_privacy=excluded.jira_privacy",
            params![owner.issuer,owner.subject,next,base.map(|d| &d.label),base.map(|d| &d.markdown),
                candidate.map(|d| &d.label),candidate.map(|d| &d.markdown),source,base_input,candidate_input,jira_privacy]).map_err(|_| unavailable())?;
        self.check_barrier()?;
        transaction.commit().map_err(|_| unavailable())?;
        Ok(next as u64)
    }
}

fn private_path(path: &Path, directory: bool) -> Result<(), AppError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| unavailable())?;
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err(unavailable());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(AppError::bad_request(
                "La carpeta de datos requiere permisos 0700 y sus archivos 0600.",
            ));
        }
    }
    Ok(())
}
