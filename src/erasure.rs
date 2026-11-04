//! External Jira erasure decisions survive document checkpoint restores.

use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::http::StatusCode;
use serde::Deserialize;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use crate::{
    AppError,
    privacy::{JiraData, WorkPrivacy, valid_account_id},
};

const MAX_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const REVISION_SPAN: u64 = 1_000_000_000;
const MAX_SAFE_REVISION: u64 = (1 << 53) - 1;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Closed,
    Updated,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Decision {
    status: Status,
    received_at: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Journal {
    version: u32,
    pub generation: u64,
    pub revision_floor: u64,
    pub accounts: BTreeMap<String, Decision>,
}

#[derive(PartialEq)]
struct Stamp {
    modified: SystemTime,
    length: u64,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    uid: u32,
    #[cfg(unix)]
    gid: u32,
    #[cfg(unix)]
    mode: u32,
}

fn closed() -> AppError {
    AppError(StatusCode::SERVICE_UNAVAILABLE,
        "No se pudo verificar la barrera de privacidad. Conserva el mantenimiento y verifica el registro externo antes de reiniciar.".into())
}

fn private_parent(path: &Path) -> Result<(), AppError> {
    let parent = fs::symlink_metadata(path.parent().ok_or_else(closed)?).map_err(|_| closed())?;
    if !parent.is_dir() {
        return Err(closed());
    }
    #[cfg(unix)]
    {
        if parent.permissions().mode() & 0o077 != 0 {
            return Err(closed());
        }
    }
    Ok(())
}

fn stamp(metadata: &fs::Metadata) -> Result<Stamp, AppError> {
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err(closed());
    }
    #[cfg(unix)]
    {
        if metadata.permissions().mode() & 0o077 != 0 || metadata.nlink() != 1 {
            return Err(closed());
        }
    }
    Ok(Stamp {
        modified: metadata.modified().map_err(|_| closed())?,
        length: metadata.len(),
        #[cfg(unix)]
        dev: metadata.dev(),
        #[cfg(unix)]
        ino: metadata.ino(),
        #[cfg(unix)]
        uid: metadata.uid(),
        #[cfg(unix)]
        gid: metadata.gid(),
        #[cfg(unix)]
        mode: metadata.mode(),
    })
}

pub(crate) struct Barrier {
    pub journal: Journal,
    path: PathBuf,
    stamp: Stamp,
}

impl Barrier {
    pub fn load(path: &Path, data: &Path) -> Result<Self, AppError> {
        private_parent(path)?;
        let canonical = fs::canonicalize(path).map_err(|_| closed())?;
        if canonical.starts_with(data) || canonical.ancestors().any(|p| p.join(".git").exists()) {
            return Err(AppError::bad_request(
                "El registro de privacidad debe estar fuera del volumen de trabajo y del repositorio.",
            ));
        }
        let before = stamp(&fs::symlink_metadata(path).map_err(|_| closed())?)?;
        let file = fs::File::open(path).map_err(|_| closed())?;
        if stamp(&file.metadata().map_err(|_| closed())?)? != before {
            return Err(closed());
        }
        let mut raw = Vec::new();
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut raw)
            .map_err(|_| closed())?;
        if raw.len() as u64 > MAX_BYTES {
            return Err(closed());
        }
        let journal: Journal = serde_json::from_slice(&raw).map_err(|_| closed())?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| closed())?
            .as_secs();
        if journal.version != 1
            || journal.generation > i64::MAX as u64
            || journal.revision_floor > MAX_SAFE_REVISION - REVISION_SPAN
            || journal.revision_floor < journal.generation.saturating_mul(REVISION_SPAN)
            || (journal.generation == 0 && !journal.accounts.is_empty())
            || journal.accounts.len() > 1_000_000
            || journal.accounts.iter().any(|(id, rule)| {
                !valid_account_id(id) || rule.received_at == 0 || rule.received_at > now
            })
        {
            return Err(closed());
        }
        let barrier = Self {
            journal,
            path: path.into(),
            stamp: before,
        };
        barrier.unchanged()?;
        Ok(barrier)
    }

    pub fn unchanged(&self) -> Result<(), AppError> {
        private_parent(&self.path)?;
        if stamp(&fs::symlink_metadata(&self.path).map_err(|_| closed())?)? != self.stamp {
            return Err(closed());
        }
        Ok(())
    }

    pub fn affected(&self, inventory: &WorkPrivacy) -> Result<(bool, bool), AppError> {
        if !inventory.base.valid()
            || !inventory.candidate.valid()
            || inventory.base.unclassified
            || inventory.candidate.unclassified
        {
            return Err(AppError(StatusCode::SERVICE_UNAVAILABLE,
                "Hay documentos o identidades sin clasificar. El trabajo se conserva; completa su clasificación antes de activar la barrera.".into()));
        }
        let matches = |data: &JiraData| {
            data.accounts.iter().any(|(id, at)| {
                self.journal
                    .accounts
                    .get(id)
                    .is_some_and(|rule| match rule.status {
                        Status::Closed => true,
                        Status::Updated => *at <= rule.received_at,
                    })
            })
        };
        let base = matches(&inventory.base);
        Ok((base, base || matches(&inventory.candidate)))
    }

    pub fn accept(&self, inventory: &WorkPrivacy) -> Result<(), AppError> {
        self.unchanged()?;
        let (base, candidate) = self.affected(inventory)?;
        if base || candidate {
            return Err(AppError(StatusCode::CONFLICT,
                "El contenido corresponde a una solicitud de privacidad. Recarga el trabajo antes de continuar.".into()));
        }
        Ok(())
    }

    pub fn accept_revision(&self, revision: u64) -> Result<(), AppError> {
        // shortcut: after one billion writes in an erasure era, add operator rollover.
        if revision >= self.journal.revision_floor + REVISION_SPAN {
            return Err(AppError::bad_request(
                "La revisión alcanzó el límite del período de privacidad. Conserva el trabajo y actualiza el registro de operación.",
            ));
        }
        Ok(())
    }
}
