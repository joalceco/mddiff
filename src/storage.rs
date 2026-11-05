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
    workspace::{Document, InputMetadata, VerifiedIdentity, Work},
};

pub(crate) struct Storage {
    connection: Mutex<Connection>,
    directory: PathBuf,
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
    pub(crate) fn checkpoint_directory(&self, directory: &Path) -> Result<PathBuf, AppError> {
        if !cfg!(unix) {
            return Err(AppError::bad_request(
                "El checkpoint del operador requiere Unix.",
            ));
        }
        private_path(directory, true)?;
        let directory = directory.canonicalize().map_err(|_| unavailable())?;
        if directory.starts_with(&self.directory)
            || directory.to_str().is_none()
            || directory
                .ancestors()
                .any(|parent| parent.join(".git").exists())
        {
            return Err(AppError::bad_request(
                "La carpeta de checkpoints debe quedar fuera de los datos y de repositorios Git.",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let output = fs::metadata(&directory).map_err(|_| unavailable())?;
            if output.mode() & 0o777 != 0o700
                || output.uid()
                    != fs::metadata(&self.directory)
                        .map_err(|_| unavailable())?
                        .uid()
            {
                return Err(AppError::bad_request(
                    "El checkpoint requiere una carpeta 0700 del propietario de los datos.",
                ));
            }
        }
        Ok(directory)
    }

    pub(crate) fn checkpoint(&self, directory: &Path) -> Result<(), AppError> {
        let directory = self.checkpoint_directory(directory)?;
        let destination = directory.join("work.sqlite3");
        if fs::symlink_metadata(&destination).is_ok() {
            return Err(AppError::bad_request(
                "Retira el checkpoint anterior antes de solicitar otro.",
            ));
        }
        let temporary = directory.join(".work.sqlite3.pending");
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let output = options.open(&temporary).map_err(|_| unavailable())?;
        let result = (|| {
            let connection = self.connection.lock().map_err(|_| unavailable())?;
            connection
                .execute("VACUUM INTO ?1", [temporary.to_str().unwrap()])
                .map_err(|_| unavailable())?;
            output.sync_all().map_err(|_| unavailable())?;
            // A hard link publishes complete bytes without replacing an operator's file.
            fs::hard_link(&temporary, &destination).map_err(|_| unavailable())?;
            fs::remove_file(&temporary).map_err(|_| unavailable())?;
            fs::File::open(&directory)
                .and_then(|parent| parent.sync_all())
                .map_err(|_| unavailable())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub(crate) fn check_health(&self) -> Result<(), AppError> {
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
        if version > 7 {
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
        if version < 7 {
            let transaction = connection.transaction().map_err(|_| unavailable())?;
            transaction
                .execute_batch(
                    "CREATE TABLE current_work_v7 (
                issuer TEXT NOT NULL,
                subject TEXT NOT NULL,
                revision INTEGER NOT NULL CHECK (revision > 0),
                base_label TEXT,
                base_markdown TEXT,
                candidate_label TEXT,
                candidate_markdown TEXT,
                base_input TEXT CHECK (base_label IS NOT NULL OR base_input IS NULL),
                candidate_input TEXT CHECK (candidate_label IS NOT NULL OR candidate_input IS NULL),
                PRIMARY KEY (issuer, subject),
                CHECK ((base_label IS NULL) = (base_markdown IS NULL)),
                CHECK ((candidate_label IS NULL) = (candidate_markdown IS NULL)),
                CHECK (base_label IS NOT NULL OR candidate_label IS NULL)
            ) WITHOUT ROWID;",
                )
                .map_err(|_| unavailable())?;
            if version != 0 {
                let columns = {
                    let mut query = transaction
                        .prepare("PRAGMA table_info(current_work)")
                        .map_err(|_| unavailable())?;
                    query
                        .query_map([], |row| row.get::<_, String>(1))
                        .map_err(|_| unavailable())?
                        .collect::<Result<std::collections::HashSet<_>, _>>()
                        .map_err(|_| unavailable())?
                };
                let base_input = if columns.contains("base_input") {
                    "base_input"
                } else {
                    "NULL"
                };
                let candidate_input = if columns.contains("candidate_input") {
                    "candidate_input"
                } else {
                    "NULL"
                };
                transaction.execute_batch(&format!("INSERT INTO current_work_v7
                    SELECT issuer,subject,revision,base_label,base_markdown,candidate_label,candidate_markdown,{base_input},{candidate_input} FROM current_work;
                    DROP TABLE current_work;")).map_err(|_| unavailable())?;
            }
            transaction
                .execute_batch(
                    "ALTER TABLE current_work_v7 RENAME TO current_work;
                DROP TABLE IF EXISTS encrypted_connections;
                DROP TABLE IF EXISTS privacy_guard;
                PRAGMA user_version=7;",
                )
                .map_err(|_| unavailable())?;
            transaction.commit().map_err(|_| unavailable())?;
            if version != 0 {
                connection
                    .execute_batch("VACUUM;")
                    .map_err(|_| unavailable())?;
                let busy: i64 = connection
                    .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
                    .map_err(|_| unavailable())?;
                if busy != 0 {
                    return Err(unavailable());
                }
            }
        }
        Ok(Self {
            connection: Mutex::new(connection),
            directory: fs::canonicalize(directory).map_err(|_| unavailable())?,
        })
    }

    #[cfg(test)]
    pub(crate) fn database_for_test(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection.lock().unwrap()
    }

    pub(crate) fn read(&self, owner: &VerifiedIdentity) -> Result<Work, AppError> {
        let connection = self.connection.lock().map_err(|_| unavailable())?;
        connection
            .query_row(
                "SELECT revision, base_label, base_markdown, candidate_label, candidate_markdown, base_input, candidate_input
            FROM current_work WHERE issuer=?1 AND subject=?2",
                params![owner.issuer, owner.subject],
                |row| {
                    let base_label: Option<String> = row.get(1)?;
                    let candidate_label: Option<String> = row.get(3)?;
                    let revision: i64 = row.get(0)?;
                    if revision <= 0 {
                        return Err(rusqlite::Error::IntegralValueOutOfRange(0, revision));
                    }
                    let input = |index| -> rusqlite::Result<Option<InputMetadata>> {
                        let raw: Option<String> = row.get(index)?;
                        raw.map(|s| serde_json::from_str(&s).map_err(|_| rusqlite::Error::InvalidQuery)).transpose()
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
                        base_input: input(5)?,
                        candidate_input: input(6)?,
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
    ) -> Result<u64, AppError> {
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
        let next = revision
            .checked_add(1)
            .ok_or_else(|| AppError::bad_request("La revisión alcanzó el límite admitido."))?;
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
        transaction.execute("INSERT INTO current_work
            (issuer,subject,revision,base_label,base_markdown,candidate_label,candidate_markdown,base_input,candidate_input)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(issuer,subject) DO UPDATE SET
            revision=excluded.revision,base_label=excluded.base_label,base_markdown=excluded.base_markdown,
            candidate_label=excluded.candidate_label,candidate_markdown=excluded.candidate_markdown,
            base_input=excluded.base_input,candidate_input=excluded.candidate_input",
            params![owner.issuer,owner.subject,next,base.map(|d| &d.label),base.map(|d| &d.markdown),
                candidate.map(|d| &d.label),candidate.map(|d| &d.markdown),base_input,candidate_input]).map_err(|_| unavailable())?;
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
