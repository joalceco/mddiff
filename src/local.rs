use crate::{
    AppError, MAX_DOCUMENT_BYTES,
    workspace::{Document, Snapshot, Workspace},
};
use axum::http::StatusCode;
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

pub struct LocalReader {
    root: PathBuf,
    workspace: Arc<Workspace>,
}

impl LocalReader {
    pub fn new(root: PathBuf) -> Result<Self, AppError> {
        let root = root
            .canonicalize()
            .map_err(|error| AppError::bad_request(error.to_string()))?;
        if !root.is_dir() {
            return Err(AppError::bad_request("--root debe señalar una carpeta."));
        }
        Ok(Self {
            root,
            workspace: Arc::new(Workspace::new()),
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn workspace(&self) -> Arc<Workspace> {
        Arc::clone(&self.workspace)
    }
    pub async fn open(
        &self,
        base: PathBuf,
        candidate: Option<PathBuf>,
    ) -> Result<Arc<Snapshot>, AppError> {
        let root = self.root.clone();
        self.workspace
            .load(move || {
                let (label, markdown) = read_document(&root, &base)?;
                let base = Document { label, markdown };
                let candidate = candidate
                    .map(|path| {
                        read_document(&root, &path)
                            .map(|(label, markdown)| Document { label, markdown })
                    })
                    .transpose()?;
                Ok((base, candidate))
            })
            .await
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
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

    #[tokio::test]
    async fn failed_open_preserves_document_and_revision() {
        let directory = TestDirectory::new();
        fs::write(directory.0.join("base.md"), "# Original\n").unwrap();
        fs::write(directory.0.join("candidate.md"), "# Propuesta\n").unwrap();
        let app = LocalReader::new(directory.0.clone()).unwrap();
        let mut revisions = app.workspace().subscribe();
        let opened = app.open("base.md".into(), None).await.unwrap();
        assert_eq!(opened.revision, 1);
        assert!(opened.candidate_path.is_none());
        assert!(revisions.has_changed().unwrap());
        assert_eq!(*revisions.borrow_and_update(), 1);
        assert!(app.open("missing.md".into(), None).await.is_err());
        assert!(Arc::ptr_eq(&opened, &app.workspace().snapshot().await));
        assert!(!revisions.has_changed().unwrap());
        let compared = app
            .open("base.md".into(), Some("candidate.md".into()))
            .await
            .unwrap();
        assert_eq!(compared.revision, 2);
        assert!(compared.candidate_path.is_some());
        assert_eq!(*revisions.borrow_and_update(), 2);
    }
}
