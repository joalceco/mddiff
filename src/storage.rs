use std::{fs, path::Path, sync::Mutex, time::Duration};

use axum::http::StatusCode;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::{
    AppError,
    workspace::{Document, VerifiedIdentity, Work},
};

pub(crate) struct Storage(Mutex<Connection>);

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
    #[cfg(test)]
    pub(crate) fn read_only_for_test(&self, value: bool) {
        self.0
            .lock()
            .unwrap()
            .pragma_update(None, "query_only", value)
            .unwrap();
    }

    pub(crate) fn open(directory: &Path) -> Result<Self, AppError> {
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
        if version > 1 {
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
        Ok(Self(Mutex::new(connection)))
    }

    pub(crate) fn read(&self, owner: &VerifiedIdentity) -> Result<Work, AppError> {
        let connection = self.0.lock().map_err(|_| unavailable())?;
        connection
            .query_row(
                "SELECT revision, base_label, base_markdown, candidate_label, candidate_markdown
            FROM current_work WHERE issuer=?1 AND subject=?2",
                params![owner.issuer, owner.subject],
                |row| {
                    let base_label: Option<String> = row.get(1)?;
                    let candidate_label: Option<String> = row.get(3)?;
                    let revision: i64 = row.get(0)?;
                    if revision <= 0 {
                        return Err(rusqlite::Error::IntegralValueOutOfRange(0, revision));
                    }
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
        base: Option<&Document>,
        candidate: Option<&Document>,
    ) -> Result<u64, AppError> {
        let mut connection = self.0.lock().map_err(|_| unavailable())?;
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
        let next = revision
            .checked_add(1)
            .ok_or_else(|| AppError::bad_request("La revisión alcanzó el límite admitido."))?;
        transaction.execute("INSERT INTO current_work
            (issuer,subject,revision,base_label,base_markdown,candidate_label,candidate_markdown)
            VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(issuer,subject) DO UPDATE SET
            revision=excluded.revision,base_label=excluded.base_label,base_markdown=excluded.base_markdown,
            candidate_label=excluded.candidate_label,candidate_markdown=excluded.candidate_markdown",
            params![owner.issuer,owner.subject,next,base.map(|d| &d.label),base.map(|d| &d.markdown),
                candidate.map(|d| &d.label),candidate.map(|d| &d.markdown)]).map_err(|_| unavailable())?;
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
