//! Owner-scoped provider credentials. Only connection metadata may cross API boundaries.

use std::{fs, io::Read, path::Path, sync::Arc};

use axum::http::StatusCode;
use ring::{
    aead::{self, Aad, LessSafeKey, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    AppError,
    storage::{ConnectionRow, EncryptedCredential, Storage},
    workspace::VerifiedIdentity,
};

pub const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Jira,
    Memos,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jira => "jira",
            Self::Memos => "memos",
        }
    }
}

/// Safe to return to the owning user; this type never includes credentials.
#[derive(Serialize)]
pub struct ConnectionMetadata {
    pub provider: Provider,
    pub destination: String,
    pub generation: u64,
    pub connected: bool,
}

/// Backend-only JSON. Deliberately has no Debug, Display, or Serialize implementation.
pub struct CredentialRecord(Value);

impl CredentialRecord {
    pub fn new(value: Value) -> Result<Self, AppError> {
        if value.as_object().is_none_or(|object| object.is_empty()) {
            return Err(AppError::bad_request(
                "La credencial requiere un objeto JSON no vacío.",
            ));
        }
        let mut encoded = serde_json::to_vec(&value).map_err(|_| cipher_error())?;
        let valid = encoded.len() <= MAX_CREDENTIAL_BYTES;
        encoded.fill(0);
        if !valid {
            return Err(AppError(
                StatusCode::PAYLOAD_TOO_LARGE,
                "La credencial supera el límite admitido.".into(),
            ));
        }
        Ok(Self(value))
    }

    /// Explicit backend access for provider requests; never return this value to clients.
    pub fn value(&self) -> &Value {
        &self.0
    }
}

/// Backend-only result, including the generation required for a conditional refresh.
pub struct StoredCredential {
    pub metadata: ConnectionMetadata,
    pub credential: CredentialRecord,
}

#[derive(Clone)]
pub struct ConnectionStore {
    storage: Arc<Storage>,
    key: Arc<LessSafeKey>,
}

fn cipher_error() -> AppError {
    AppError::internal(
        "No se pudo proteger o recuperar la credencial. Verifica la clave y el almacenamiento.",
    )
}

fn key_error() -> AppError {
    AppError::bad_request(
        "La clave de conexiones requiere un archivo regular privado de 32 bytes, con permisos 0600, fuera de la carpeta de datos y de repositorios Git.",
    )
}

pub(crate) fn connection_conflict() -> AppError {
    AppError(
        StatusCode::CONFLICT,
        "La conexión cambió. Recarga su estado antes de volver a intentarlo.".into(),
    )
}

fn destination_id(value: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    {
        return Err(AppError::bad_request(
            "El destino requiere un identificador administrativo válido.",
        ));
    }
    Ok(())
}

fn load_key(path: &Path, directory: &Path) -> Result<LessSafeKey, AppError> {
    let before = fs::symlink_metadata(path).map_err(|_| key_error())?;
    if !before.is_file() || before.len() != 32 {
        return Err(key_error());
    }
    let canonical = fs::canonicalize(path).map_err(|_| key_error())?;
    if canonical.starts_with(directory)
        || canonical
            .ancestors()
            .any(|parent| parent.join(".git").exists())
    {
        return Err(key_error());
    }
    let mut file = fs::File::open(path).map_err(|_| key_error())?;
    let opened = file.metadata().map_err(|_| key_error())?;
    if !opened.is_file() || opened.len() != 32 {
        return Err(key_error());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if before.permissions().mode() & 0o777 != 0o600
            || opened.permissions().mode() & 0o777 != 0o600
            || (before.dev(), before.ino()) != (opened.dev(), opened.ino())
        {
            return Err(key_error());
        }
    }
    let mut material = [0u8; 32];
    if file.read_exact(&mut material).is_err() {
        material.fill(0);
        return Err(key_error());
    }
    let mut extra = [0u8; 1];
    if !matches!(file.read(&mut extra), Ok(0)) {
        material.fill(0);
        return Err(key_error());
    }
    let key = UnboundKey::new(&aead::AES_256_GCM, &material);
    material.fill(0);
    Ok(LessSafeKey::new(key.map_err(|_| key_error())?))
}

fn associated_data(
    owner: &VerifiedIdentity,
    provider: Provider,
    destination: &str,
    generation: u64,
) -> Result<Vec<u8>, AppError> {
    serde_json::to_vec(&(
        "mddiff.connections/v1",
        &owner.issuer,
        &owner.subject,
        provider.as_str(),
        destination,
        generation,
    ))
    .map_err(|_| cipher_error())
}

fn metadata(provider: Provider, destination: String, row: &ConnectionRow) -> ConnectionMetadata {
    ConnectionMetadata {
        provider,
        destination,
        generation: row.generation,
        connected: row.encrypted.is_some(),
    }
}

impl ConnectionStore {
    pub(crate) fn new(storage: Arc<Storage>, key_path: &Path) -> Result<Self, AppError> {
        let key = Arc::new(load_key(key_path, storage.directory())?);
        Ok(Self { storage, key })
    }

    async fn blocking<T: Send + 'static>(
        &self,
        operation: impl FnOnce(Self) -> Result<T, AppError> + Send + 'static,
    ) -> Result<T, AppError> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || operation(store))
            .await
            .map_err(|_| cipher_error())?
    }

    pub async fn metadata(
        &self,
        owner: &VerifiedIdentity,
        provider: Provider,
        destination: &str,
    ) -> Result<ConnectionMetadata, AppError> {
        destination_id(destination)?;
        let owner = owner.clone();
        let destination = destination.to_owned();
        self.blocking(move |store| {
            let row = store
                .storage
                .read_connection(&owner, provider.as_str(), &destination)?;
            Ok(metadata(provider, destination, &row))
        })
        .await
    }

    pub async fn get(
        &self,
        owner: &VerifiedIdentity,
        provider: Provider,
        destination: &str,
    ) -> Result<Option<StoredCredential>, AppError> {
        destination_id(destination)?;
        let owner = owner.clone();
        let destination = destination.to_owned();
        self.blocking(move |store| {
            let row = store
                .storage
                .read_connection(&owner, provider.as_str(), &destination)?;
            let metadata = metadata(provider, destination.clone(), &row);
            let Some(encrypted) = row.encrypted else {
                return Ok(None);
            };
            let aad = associated_data(&owner, provider, &destination, row.generation)?;
            let mut buffer = encrypted.ciphertext;
            let decoded = store.key.open_in_place(
                Nonce::assume_unique_for_key(encrypted.nonce),
                Aad::from(&aad),
                &mut buffer,
            );
            let value = match decoded {
                Ok(plaintext) => serde_json::from_slice(plaintext).map_err(|_| cipher_error()),
                Err(_) => Err(cipher_error()),
            };
            buffer.fill(0);
            Ok(Some(StoredCredential {
                metadata,
                credential: CredentialRecord::new(value?)?,
            }))
        })
        .await
    }

    /// Save only if the generation still matches the one captured before provider I/O.
    pub async fn save(
        &self,
        owner: &VerifiedIdentity,
        provider: Provider,
        destination: &str,
        expected_generation: u64,
        credential: CredentialRecord,
    ) -> Result<ConnectionMetadata, AppError> {
        destination_id(destination)?;
        let generation = expected_generation
            .checked_add(1)
            .filter(|value| *value <= i64::MAX as u64)
            .ok_or_else(|| {
                AppError::bad_request("La generación de la conexión alcanzó el límite admitido.")
            })?;
        let owner = owner.clone();
        let destination = destination.to_owned();
        self.blocking(move |store| {
            let mut nonce = [0u8; 12];
            SystemRandom::new()
                .fill(&mut nonce)
                .map_err(|_| cipher_error())?;
            let aad = associated_data(&owner, provider, &destination, generation)?;
            let mut ciphertext =
                serde_json::to_vec(credential.value()).map_err(|_| cipher_error())?;
            if store
                .key
                .seal_in_place_append_tag(
                    Nonce::assume_unique_for_key(nonce),
                    Aad::from(&aad),
                    &mut ciphertext,
                )
                .is_err()
            {
                ciphertext.fill(0);
                return Err(cipher_error());
            }
            store.storage.replace_connection(
                &owner,
                provider.as_str(),
                &destination,
                expected_generation,
                Some(EncryptedCredential { nonce, ciphertext }),
            )?;
            Ok(ConnectionMetadata {
                provider,
                destination,
                generation,
                connected: true,
            })
        })
        .await
    }

    /// A tombstone remains even when there was no credential, invalidating pending callbacks.
    pub async fn delete(
        &self,
        owner: &VerifiedIdentity,
        provider: Provider,
        destination: &str,
        expected_generation: u64,
    ) -> Result<ConnectionMetadata, AppError> {
        destination_id(destination)?;
        let owner = owner.clone();
        let destination = destination.to_owned();
        self.blocking(move |store| {
            let generation = store.storage.replace_connection(
                &owner,
                provider.as_str(),
                &destination,
                expected_generation,
                None,
            )?;
            Ok(ConnectionMetadata {
                provider,
                destination,
                generation,
                connected: false,
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{Document, Workspaces};
    use rusqlite::params;
    use std::{
        fs::OpenOptions,
        io::Write,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        data: PathBuf,
        key: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "mddiff-connections-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&root).unwrap();
            let key = root.join("connection.key");
            Self::write_key(&key, &[7; 32]);
            Self {
                data: root.join("data"),
                key,
                root,
            }
        }

        fn write_key(path: &Path, material: &[u8]) {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(path).unwrap().write_all(material).unwrap();
        }

        fn store(&self) -> (Workspaces, ConnectionStore) {
            let workspaces = Workspaces::persistent(&self.data).unwrap();
            let store = workspaces.connection_store(&self.key).unwrap();
            (workspaces, store)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn owner(subject: &str) -> VerifiedIdentity {
        VerifiedIdentity::from_verified_claims(
            "https://issuer.invalid/realm".into(),
            subject.into(),
        )
        .unwrap()
    }

    fn credential(value: &str) -> CredentialRecord {
        CredentialRecord::new(serde_json::json!({"token":value,"expires_at":123456})).unwrap()
    }

    #[tokio::test]
    async fn credentials_are_private_durable_and_deleted_with_nonreusable_generations() {
        let fixture = Fixture::new();
        let alice = owner("alice");
        let bob = owner("bob");
        {
            let (workspaces, store) = fixture.store();
            workspaces
                .for_user(&alice)
                .await
                .unwrap()
                .replace_if_revision(
                    0,
                    Document {
                        label: "original.md".into(),
                        markdown: "# Preserve this work".into(),
                    },
                    None,
                )
                .await
                .unwrap();
            let absent = store
                .metadata(&alice, Provider::Memos, "notes")
                .await
                .unwrap();
            assert_eq!(absent.generation, 0);
            assert!(!absent.connected);
            let first = store
                .save(
                    &alice,
                    Provider::Memos,
                    "notes",
                    0,
                    credential("synthetic-alice-secret"),
                )
                .await
                .unwrap();
            assert_eq!(first.generation, 1);
            assert!(first.connected);
            for (other, provider, destination) in [
                (&bob, Provider::Memos, "notes"),
                (&alice, Provider::Jira, "notes"),
                (&alice, Provider::Memos, "another"),
                (
                    &VerifiedIdentity::from_verified_claims(
                        "https://another.invalid".into(),
                        "alice".into(),
                    )
                    .unwrap(),
                    Provider::Memos,
                    "notes",
                ),
            ] {
                assert!(
                    store
                        .get(other, provider, destination)
                        .await
                        .unwrap()
                        .is_none()
                );
            }
            store
                .save(
                    &bob,
                    Provider::Memos,
                    "notes",
                    0,
                    credential("synthetic-bob-secret"),
                )
                .await
                .unwrap();
            let metadata = serde_json::to_value(&first).unwrap();
            assert_eq!(metadata.as_object().unwrap().len(), 4);
            assert!(metadata.get("token").is_none());
        }
        {
            let (workspaces, store) = fixture.store();
            let recovered = store
                .get(&alice, Provider::Memos, "notes")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(recovered.metadata.generation, 1);
            assert_eq!(
                recovered.credential.value()["token"],
                "synthetic-alice-secret"
            );
            let deleted = store
                .delete(&alice, Provider::Memos, "notes", 1)
                .await
                .unwrap();
            assert_eq!(deleted.generation, 2);
            assert!(!deleted.connected);
            assert!(
                store
                    .get(&alice, Provider::Memos, "notes")
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                store
                    .save(
                        &alice,
                        Provider::Memos,
                        "notes",
                        1,
                        credential("stale-callback")
                    )
                    .await
                    .err()
                    .unwrap()
                    .0,
                StatusCode::CONFLICT
            );
            assert!(
                store
                    .get(&bob, Provider::Memos, "notes")
                    .await
                    .unwrap()
                    .is_some()
            );
            assert_eq!(
                workspaces
                    .for_user(&alice)
                    .await
                    .unwrap()
                    .work()
                    .await
                    .base
                    .as_ref()
                    .unwrap()
                    .markdown,
                "# Preserve this work"
            );
        }
        let connection = rusqlite::Connection::open(fixture.data.join("work.sqlite3")).unwrap();
        let tombstone: (i64, bool, bool) = connection.query_row(
            "SELECT generation,nonce IS NULL,ciphertext IS NULL FROM encrypted_connections WHERE subject='alice'",
            [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).unwrap();
        assert_eq!(tombstone, (2, true, true));
        drop(connection);
        for entry in fs::read_dir(&fixture.data).unwrap() {
            let bytes = fs::read(entry.unwrap().path()).unwrap();
            for token in [
                b"synthetic-alice-secret".as_slice(),
                b"synthetic-bob-secret".as_slice(),
            ] {
                assert!(!bytes.windows(token.len()).any(|window| window == token));
            }
        }
        let (_workspaces, store) = fixture.store();
        assert_eq!(
            store
                .metadata(&alice, Provider::Memos, "notes")
                .await
                .unwrap()
                .generation,
            2
        );
        assert_eq!(
            store
                .save(
                    &alice,
                    Provider::Memos,
                    "notes",
                    0,
                    credential("stale-zero")
                )
                .await
                .err()
                .unwrap()
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            store
                .save(
                    &alice,
                    Provider::Memos,
                    "notes",
                    2,
                    credential("reconnected")
                )
                .await
                .unwrap()
                .generation,
            3
        );
    }

    #[tokio::test]
    async fn disconnecting_an_absent_connection_invalidates_pending_oauth_and_racing_saves() {
        let fixture = Fixture::new();
        let (_workspaces, store) = fixture.store();
        let alice = owner("alice");
        assert_eq!(
            store
                .delete(&alice, Provider::Jira, "cloud", 0)
                .await
                .unwrap()
                .generation,
            1
        );
        assert_eq!(
            store
                .save(&alice, Provider::Jira, "cloud", 0, credential("late-oauth"))
                .await
                .err()
                .unwrap()
                .0,
            StatusCode::CONFLICT
        );
        let (a, b) = tokio::join!(
            store.save(&alice, Provider::Jira, "cloud", 1, credential("first")),
            store.save(&alice, Provider::Jira, "cloud", 1, credential("second")),
        );
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        assert_eq!(a.err().or_else(|| b.err()).unwrap().0, StatusCode::CONFLICT);
        assert_eq!(
            store
                .metadata(&alice, Provider::Jira, "cloud")
                .await
                .unwrap()
                .generation,
            2
        );
    }

    #[tokio::test]
    async fn wrong_key_and_tampered_binding_or_ciphertext_fail_closed() {
        for modification in [
            "subject='bob'",
            "issuer='https://another.invalid'",
            "provider='jira'",
            "destination='another'",
            "generation=2",
            "ciphertext",
        ] {
            let fixture = Fixture::new();
            let (_workspaces, store) = fixture.store();
            let alice = owner("alice");
            store
                .save(
                    &alice,
                    Provider::Memos,
                    "notes",
                    0,
                    credential("synthetic-secret"),
                )
                .await
                .unwrap();
            let mut expected_owner = alice.clone();
            let mut expected_provider = Provider::Memos;
            let mut destination = "notes";
            {
                let database = store.storage.database_for_test();
                if modification == "ciphertext" {
                    let mut ciphertext: Vec<u8> = database
                        .query_row("SELECT ciphertext FROM encrypted_connections", [], |row| {
                            row.get(0)
                        })
                        .unwrap();
                    ciphertext[0] ^= 1;
                    database
                        .execute(
                            "UPDATE encrypted_connections SET ciphertext=?1",
                            params![ciphertext],
                        )
                        .unwrap();
                } else {
                    database
                        .execute(
                            &format!("UPDATE encrypted_connections SET {modification}"),
                            [],
                        )
                        .unwrap();
                    match modification {
                        "subject='bob'" => expected_owner = owner("bob"),
                        "issuer='https://another.invalid'" => {
                            expected_owner = VerifiedIdentity::from_verified_claims(
                                "https://another.invalid".into(),
                                "alice".into(),
                            )
                            .unwrap()
                        }
                        "provider='jira'" => expected_provider = Provider::Jira,
                        "destination='another'" => destination = "another",
                        _ => (),
                    }
                }
            }
            assert_eq!(
                store
                    .get(&expected_owner, expected_provider, destination)
                    .await
                    .err()
                    .unwrap()
                    .0,
                StatusCode::INTERNAL_SERVER_ERROR
            );
        }
        let fixture = Fixture::new();
        let (workspaces, correct) = fixture.store();
        correct
            .save(
                &owner("alice"),
                Provider::Memos,
                "notes",
                0,
                credential("synthetic-secret"),
            )
            .await
            .unwrap();
        let another_key = fixture.root.join("another.key");
        Fixture::write_key(&another_key, &[8; 32]);
        let wrong = workspaces.connection_store(&another_key).unwrap();
        assert_eq!(
            wrong
                .get(&owner("alice"), Provider::Memos, "notes")
                .await
                .err()
                .unwrap()
                .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert!(
            correct
                .get(&owner("alice"), Provider::Memos, "notes")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn writes_use_new_nonces_and_storage_errors_preserve_the_previous_credential() {
        let fixture = Fixture::new();
        let (_workspaces, store) = fixture.store();
        let alice = owner("alice");
        store
            .save(&alice, Provider::Memos, "notes", 0, credential("first"))
            .await
            .unwrap();
        let before = store
            .storage
            .read_connection(&alice, "memos", "notes")
            .unwrap()
            .encrypted
            .unwrap();
        store
            .save(&alice, Provider::Memos, "notes", 1, credential("second"))
            .await
            .unwrap();
        let after = store
            .storage
            .read_connection(&alice, "memos", "notes")
            .unwrap()
            .encrypted
            .unwrap();
        assert_ne!(before.nonce, after.nonce);
        assert_ne!(before.ciphertext, after.ciphertext);
        store.storage.read_only_for_test(true);
        assert_eq!(
            store
                .delete(&alice, Provider::Memos, "notes", 2)
                .await
                .err()
                .unwrap()
                .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            store
                .save(
                    &alice,
                    Provider::Memos,
                    "notes",
                    2,
                    credential("cannot-save")
                )
                .await
                .err()
                .unwrap()
                .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        store.storage.read_only_for_test(false);
        let unchanged = store
            .get(&alice, Provider::Memos, "notes")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(unchanged.metadata.generation, 2);
        assert_eq!(unchanged.credential.value()["token"], "second");
    }

    #[test]
    fn migration_from_schema_one_preserves_current_work() {
        let fixture = Fixture::new();
        fs::create_dir(&fixture.data).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fixture.data, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = fixture.data.join("work.sqlite3");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch(
            "CREATE TABLE current_work (issuer TEXT NOT NULL,subject TEXT NOT NULL,revision INTEGER NOT NULL,
             base_label TEXT,base_markdown TEXT,candidate_label TEXT,candidate_markdown TEXT,
             PRIMARY KEY(issuer,subject)) WITHOUT ROWID;
             INSERT INTO current_work VALUES ('https://issuer.invalid/realm','alice',5,'original.md','# Existing original',NULL,NULL);
             PRAGMA user_version=1;",
        ).unwrap();
        drop(connection);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let (_workspaces, store) = fixture.store();
        let work = store.storage.read(&owner("alice")).unwrap();
        assert_eq!(work.revision, 5);
        assert_eq!(work.base.unwrap().markdown, "# Existing original");
        assert_eq!(
            store
                .storage
                .database_for_test()
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            3
        );
    }

    #[cfg(unix)]
    #[test]
    fn key_files_are_private_regular_exact_length_and_outside_the_data_directory() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let fixture = Fixture::new();
        let (workspaces, _store) = fixture.store();
        let inside = fixture.data.join("forbidden.key");
        Fixture::write_key(&inside, &[7; 32]);
        assert!(workspaces.connection_store(&inside).is_err());
        let checkout = fixture.root.join("checkout");
        fs::create_dir(&checkout).unwrap();
        fs::create_dir(checkout.join(".git")).unwrap();
        let tracked_key = checkout.join("private.key");
        Fixture::write_key(&tracked_key, &[7; 32]);
        assert!(workspaces.connection_store(&tracked_key).is_err());
        let symlink_path = fixture.root.join("symlink.key");
        symlink(&fixture.key, &symlink_path).unwrap();
        assert!(workspaces.connection_store(&symlink_path).is_err());
        assert!(workspaces.connection_store(&fixture.root).is_err());
        fs::set_permissions(&fixture.key, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(workspaces.connection_store(&fixture.key).is_err());
        fs::set_permissions(&fixture.key, fs::Permissions::from_mode(0o600)).unwrap();
        for size in [0, 31, 33] {
            let key = fixture.root.join(format!("invalid-{size}.key"));
            Fixture::write_key(&key, &vec![7; size]);
            assert!(workspaces.connection_store(&key).is_err());
        }
        assert!(
            workspaces
                .connection_store(&fixture.root.join("missing.key"))
                .is_err()
        );
    }

    #[tokio::test]
    async fn invalid_payloads_and_destinations_never_create_a_connection() {
        let fixture = Fixture::new();
        let (_workspaces, store) = fixture.store();
        for invalid in [
            Value::Null,
            serde_json::json!({}),
            serde_json::json!("token"),
        ] {
            assert_eq!(
                CredentialRecord::new(invalid).err().unwrap().0,
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            CredentialRecord::new(serde_json::json!({"token":"x".repeat(MAX_CREDENTIAL_BYTES)}))
                .err()
                .unwrap()
                .0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        for invalid in ["", "https://arbitrary.invalid", "../notes", "notes\n"] {
            // Dot-only path segments cannot select storage paths; IDs are never opened.
            if invalid == "../notes" {
                assert!(
                    store
                        .metadata(&owner("alice"), Provider::Memos, invalid)
                        .await
                        .is_err()
                );
            } else {
                assert!(
                    store
                        .save(
                            &owner("alice"),
                            Provider::Memos,
                            invalid,
                            0,
                            credential("unused")
                        )
                        .await
                        .is_err()
                );
            }
        }
        assert!(
            store
                .get(&owner("alice"), Provider::Memos, "notes")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            Workspaces::default()
                .connection_store(&fixture.key)
                .is_err()
        );
    }
}
