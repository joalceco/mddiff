use std::{
    collections::HashMap,
    fs,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Extension, Json, Router,
    extract::{Path as RoutePath, Query, State, rejection::JsonRejection},
    http::StatusCode,
    response::Redirect,
    routing::{get, post},
};
use mddiff::{
    AppError,
    connections::{ConnectionStore, CredentialRecord, Provider},
    workspace::{Document, SourceMetadata, VerifiedIdentity},
};
use openidconnect::{CsrfToken, url::Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{App, auth::SessionHandle};

const SCOPES: &str = "read:jira-work offline_access";
const CALLBACK: &str = "/connections/jira/callback";
const RESPONSE_LIMIT: usize = 256 * 1024;
const MEMO_RESPONSE_LIMIT: usize = 6 * mddiff::MAX_DOCUMENT_BYTES as usize + 64 * 1024;

fn invalid() -> AppError {
    AppError::bad_request(
        "La configuración de conexiones no es válida. Revisa destinos, permisos y clave externa.",
    )
}
fn failed() -> AppError {
    AppError(StatusCode::BAD_GATEWAY, "No se pudo comprobar la conexión. Revisa el permiso, la versión y la disponibilidad del proveedor; vuelve a conectar si caducó o fue revocada.".into())
}
fn stale() -> AppError {
    AppError(
        StatusCode::CONFLICT,
        "La conexión cambió. Recarga antes de volver a intentarlo.".into(),
    )
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub key_file: PathBuf,
    #[serde(default)]
    memos: Vec<Memos>,
    jira: Option<Jira>,
    #[serde(default)]
    allow_insecure_loopback: bool,
    jira_test_origin: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Memos {
    id: String,
    label: String,
    url: String,
    #[serde(default)]
    addresses: Vec<IpAddr>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Jira {
    client_id: String,
    client_secret: String,
    sites: Vec<Site>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Site {
    id: String,
    label: String,
    url: String,
    cloud_id: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, AppError> {
        let metadata = fs::symlink_metadata(path).map_err(|_| invalid())?;
        if !metadata.is_file() || metadata.len() > 64 * 1024 {
            return Err(invalid());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(invalid());
            }
        }
        serde_json::from_slice(&fs::read(path).map_err(|_| invalid())?).map_err(|_| invalid())
    }
}

struct Destination {
    id: String,
    label: String,
    url: Url,
    provider: Provider,
    pins: Vec<IpAddr>,
    cloud_id: Option<String>,
}
impl Destination {
    fn binding(&self, jira: &Option<Jira>) -> String {
        json!([
            self.url.as_str(),
            self.cloud_id,
            jira.as_ref()
                .filter(|_| self.provider == Provider::Jira)
                .map(|j| &j.client_id)
        ])
        .to_string()
    }
}

struct Pending {
    owner: VerifiedIdentity,
    session: SessionHandle,
    destination: String,
    revision: u64,
    created: Instant,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credential {
    binding: String,
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<u64>,
}

type RefreshLocks = HashMap<(VerifiedIdentity, String), Arc<Mutex<()>>>;

pub struct Providers {
    store: ConnectionStore,
    destinations: Vec<Destination>,
    jira: Option<Jira>,
    jira_origin: Option<Url>,
    loopback: bool,
    origin: String,
    pending: Mutex<HashMap<String, Pending>>,
    refreshes: Mutex<RefreshLocks>,
}

fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn memo_resource(reference: &str, destination: &Url) -> Result<String, AppError> {
    let reference = reference.trim();
    let bad = || {
        AppError::bad_request(
            "Usa el ID de una nota o su URL /memos/ID en la instancia seleccionada, sin parámetros ni fragmentos.",
        )
    };
    if reference.is_empty()
        || reference.len() > 2048
        || reference.contains(['%', '\\'])
        || reference.chars().any(char::is_control)
    {
        return Err(bad());
    }
    let id = if let Ok(url) = Url::parse(reference) {
        let raw_path = reference
            .split_once("://")
            .and_then(|(_, rest)| rest.find('/').map(|start| &rest[start..]))
            .ok_or_else(bad)?;
        if url.origin() != destination.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || raw_path != url.path()
        {
            return Err(bad());
        }
        url.path()
            .strip_prefix("/memos/")
            .ok_or_else(bad)?
            .to_owned()
    } else {
        reference
            .strip_prefix("memos/")
            .unwrap_or(reference)
            .to_owned()
    };
    if id == "-"
        || id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(bad());
    }
    Ok(format!("memos/{id}"))
}
fn issue_key(reference: &str, destination: &Url) -> Result<String, AppError> {
    let reference = reference.trim();
    let bad = || {
        AppError::bad_request(
            "Usa una clave de issue Jira o su URL /browse/CLAVE del sitio seleccionado, sin parámetros ni fragmentos.",
        )
    };
    if reference.is_empty()
        || reference.len() > 2048
        || reference.contains(['%', '\\'])
        || reference.chars().any(char::is_control)
    {
        return Err(bad());
    }
    let key = if let Ok(url) = Url::parse(reference) {
        let raw_path = reference
            .split_once("://")
            .and_then(|(_, rest)| rest.find('/').map(|i| &rest[i..]))
            .ok_or_else(bad)?;
        if url.origin() != destination.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || raw_path != url.path()
        {
            return Err(bad());
        }
        url.path()
            .strip_prefix("/browse/")
            .ok_or_else(bad)?
            .to_owned()
    } else {
        reference.to_owned()
    };
    let key = key.to_ascii_uppercase();
    let (project, number) = key.split_once('-').ok_or_else(bad)?;
    if key.len() > 128
        || !project
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_uppercase)
        || !project
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        || number.is_empty()
        || number.starts_with('0')
        || !number.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(bad());
    }
    Ok(key)
}

fn loopback_url(url: &Url) -> bool {
    url.host_str() == Some("localhost")
        || url
            .host_str()
            .and_then(|h| h.trim_matches(['[', ']']).parse::<IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback())
}
fn origin_url(text: &str, allow_loopback: bool) -> Result<Url, AppError> {
    let url = Url::parse(text).map_err(|_| invalid())?;
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || !(url.scheme() == "https"
            || (allow_loopback && url.scheme() == "http" && loopback_url(&url)))
    {
        return Err(invalid());
    }
    Ok(url)
}

fn allowed_ip(ip: IpAddr, pins: &[IpAddr], loopback: bool) -> bool {
    if !pins.is_empty() && !pins.contains(&ip) {
        return false;
    }
    if let IpAddr::V6(ip) = ip
        && let Some(v4) = ip.to_ipv4_mapped()
    {
        return allowed_ip(v4.into(), pins, loopback);
    }
    if ip.is_loopback() {
        return loopback;
    }
    match ip {
        IpAddr::V4(v) => {
            if v.is_private() {
                return pins.contains(&ip);
            }
            let [a, b, _, _] = v.octets();
            !v.is_unspecified()
                && !v.is_link_local()
                && !v.is_multicast()
                && !v.is_broadcast()
                && !v.is_documentation()
                && a != 0
                && a < 224
                && !(a == 100 && (64..=127).contains(&b))
                && !(a == 192 && b == 0)
                && !(a == 198 && (18..=19).contains(&b))
        }
        IpAddr::V6(v) => {
            if v.is_unique_local() {
                pins.contains(&ip)
            } else {
                (v.segments()[0] & 0xe000) == 0x2000
                    && v.segments()[0] != 0x2002
                    && !(v.segments()[0] == 0x2001 && v.segments()[1] < 0x0200)
                    && !(v.segments()[0] == 0x2001 && v.segments()[1] == 0x0db8)
            }
        }
    }
}

impl Providers {
    pub fn new(config: Config, store: ConnectionStore, origin: &str) -> Result<Self, AppError> {
        let public = Url::parse(origin).map_err(|_| invalid())?;
        if config.allow_insecure_loopback && !loopback_url(&public) {
            return Err(invalid());
        }
        let test = config
            .jira_test_origin
            .as_deref()
            .map(|s| origin_url(s, config.allow_insecure_loopback))
            .transpose()?;
        if test
            .as_ref()
            .is_some_and(|u| !config.allow_insecure_loopback || !loopback_url(u))
        {
            return Err(invalid());
        }
        let mut destinations = Vec::new();
        for m in config.memos {
            let url = origin_url(&m.url, config.allow_insecure_loopback)?;
            if m.addresses
                .iter()
                .any(|ip| !allowed_ip(*ip, &m.addresses, config.allow_insecure_loopback))
            {
                return Err(invalid());
            }
            destinations.push(Destination {
                id: m.id,
                label: m.label,
                url,
                provider: Provider::Memos,
                pins: m.addresses,
                cloud_id: None,
            });
        }
        if let Some(jira) = &config.jira {
            if jira.client_id.is_empty()
                || jira.client_id.len() > 1024
                || jira.client_secret.is_empty()
                || jira.client_secret.len() > 4096
            {
                return Err(invalid());
            }
            for site in &jira.sites {
                let url = origin_url(&site.url, false)?;
                if !url
                    .host_str()
                    .is_some_and(|h| h.ends_with(".atlassian.net") && h != ".atlassian.net")
                    || url.port().is_some()
                    || !identifier(&site.cloud_id)
                {
                    return Err(invalid());
                }
                destinations.push(Destination {
                    id: site.id.clone(),
                    label: site.label.clone(),
                    url,
                    provider: Provider::Jira,
                    pins: vec![],
                    cloud_id: Some(site.cloud_id.clone()),
                });
            }
        }
        if destinations.len() > 32 {
            return Err(invalid());
        }
        for (i, d) in destinations.iter().enumerate() {
            if !identifier(&d.id)
                || d.label.trim().is_empty()
                || d.label.len() > 120
                || d.label.chars().any(char::is_control)
                || destinations[..i].iter().any(|other| other.id == d.id)
            {
                return Err(invalid());
            }
        }
        Ok(Self {
            store,
            destinations,
            jira: config.jira,
            jira_origin: test,
            loopback: config.allow_insecure_loopback,
            origin: origin.into(),
            pending: Mutex::default(),
            refreshes: Mutex::default(),
        })
    }

    fn destination(&self, id: &str) -> Result<&Destination, AppError> {
        self.destinations
            .iter()
            .find(|d| d.id == id)
            .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "Destino no disponible.".into()))
    }

    fn jira_endpoint(&self, path: &str, auth: bool) -> Url {
        self.jira_origin
            .clone()
            .unwrap_or_else(|| {
                Url::parse(if auth {
                    "https://auth.atlassian.com/"
                } else {
                    "https://api.atlassian.com/"
                })
                .unwrap()
            })
            .join(path)
            .unwrap()
    }

    async fn request(
        &self,
        url: Url,
        pins: &[IpAddr],
        token: Option<&str>,
        body: Option<Value>,
        limit: usize,
    ) -> Result<Value, AppError> {
        self.request_json(url, pins, token, body, limit, false)
            .await
    }

    async fn request_json(
        &self,
        url: Url,
        pins: &[IpAddr],
        token: Option<&str>,
        body: Option<Value>,
        limit: usize,
        issue_read: bool,
    ) -> Result<Value, AppError> {
        let host = url.host_str().ok_or_else(failed)?;
        let port = url.port_or_known_default().ok_or_else(failed)?;
        let addresses: Vec<SocketAddr> = tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::lookup_host((host.trim_matches(['[', ']']), port)),
        )
        .await
        .map_err(|_| failed())?
        .map_err(|_| failed())?
        .collect();
        if addresses.is_empty()
            || addresses
                .iter()
                .any(|a| !allowed_ip(a.ip(), pins, self.loopback))
        {
            return Err(failed());
        }
        // Pin this request's checked DNS results; redirects and environment proxies stay disabled.
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(8))
            .connect_timeout(Duration::from_secs(3))
            .resolve_to_addrs(host, &addresses)
            .build()
            .map_err(|_| failed())?;
        let mut request = if let Some(body) = body {
            client.post(url).json(&body)
        } else {
            client.get(url)
        };
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let mut response = request
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|_| failed())?;
        if !response.status().is_success() {
            return Err(if issue_read {
                match response.status() {
                    StatusCode::FORBIDDEN => AppError(StatusCode::BAD_GATEWAY, "Tu conexión personal no tiene permiso para leer este issue de Jira.".into()),
                    StatusCode::NOT_FOUND => AppError(StatusCode::NOT_FOUND, "Jira no encuentra el issue o no permite verlo con esta conexión.".into()),
                    StatusCode::TOO_MANY_REQUESTS => AppError(StatusCode::TOO_MANY_REQUESTS, "Jira alcanzó su límite de solicitudes. Espera antes de intentar abrir el origen de nuevo.".into()),
                    _ => failed(),
                }
            } else {
                failed()
            });
        }
        let too_large = || {
            if issue_read {
                AppError(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "La respuesta de Jira supera el límite de importación.".into(),
                )
            } else {
                failed()
            }
        };
        if response.content_length().is_some_and(|n| n > limit as u64) {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| failed())? {
            if bytes.len() + chunk.len() > limit {
                return Err(too_large());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| failed())
    }

    async fn list(&self, owner: &VerifiedIdentity) -> Result<Value, AppError> {
        let mut rows = Vec::new();
        for d in &self.destinations {
            let meta = self.store.metadata(owner, d.provider, &d.id).await?;
            rows.push(json!({"id":d.id,"provider":d.provider,"label":d.label,"url":d.url.as_str(),"connected":meta.connected,"revision":meta.generation}));
        }
        Ok(json!({"enabled":true,"destinations":rows}))
    }

    async fn save(
        &self,
        owner: &VerifiedIdentity,
        d: &Destination,
        revision: u64,
        credential: Credential,
    ) -> Result<Value, AppError> {
        let record =
            CredentialRecord::new(serde_json::to_value(credential).map_err(|_| failed())?)?;
        let meta = self
            .store
            .save(owner, d.provider, &d.id, revision, record)
            .await?;
        Ok(json!({"revision":meta.generation,"connected":meta.connected}))
    }

    async fn memos_identity(&self, d: &Destination, token: &str) -> Result<(), AppError> {
        if token.is_empty() || token.len() > 8192 || token.chars().any(char::is_whitespace) {
            return Err(AppError::bad_request("Token de Memos inválido."));
        }
        let result = self
            .request(
                d.url.join("/api/v1/auth/me").unwrap(),
                &d.pins,
                Some(token),
                None,
                RESPONSE_LIMIT,
            )
            .await?;
        if !result["user"]["name"]
            .as_str()
            .is_some_and(|name| name.starts_with("users/") && name.len() > 6)
        {
            return Err(failed());
        }
        Ok(())
    }

    async fn read_memo(
        &self,
        owner: &VerifiedIdentity,
        destination: &str,
        reference: &str,
    ) -> Result<(Document, SourceMetadata, u64), AppError> {
        let d = self.destination(destination)?;
        if d.provider != Provider::Memos {
            return Err(AppError::bad_request("Elige una conexión de Memos."));
        }
        let resource = memo_resource(reference, &d.url)?;
        let stored = self
            .store
            .get(owner, d.provider, &d.id)
            .await?
            .ok_or_else(|| {
                AppError(
                    StatusCode::CONFLICT,
                    "Conecta Memos antes de abrir una nota.".into(),
                )
            })?;
        let credential: Credential =
            serde_json::from_value(stored.credential.value().clone()).map_err(|_| failed())?;
        if credential.binding != d.binding(&self.jira) {
            return Err(failed());
        }
        // Public notes must not turn an invalid personal credential into anonymous access.
        self.memos_identity(d, &credential.access_token).await?;
        let memo = self
            .request(
                d.url.join(&format!("/api/v1/{resource}")).unwrap(),
                &d.pins,
                Some(&credential.access_token),
                None,
                MEMO_RESPONSE_LIMIT,
            )
            .await?;
        if memo["name"].as_str() != Some(&resource) {
            return Err(failed());
        }
        let markdown = memo["content"].as_str().ok_or_else(failed)?;
        if markdown.len() as u64 > mddiff::MAX_DOCUMENT_BYTES {
            return Err(AppError(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Cada documento admite hasta 2 MiB.".into(),
            ));
        }
        let updated_at = match memo.get("updateTime") {
            None | Some(Value::Null) => None,
            Some(Value::String(value))
                if value.len() <= 64 && !value.chars().any(char::is_control) =>
            {
                Some(value.clone())
            }
            _ => return Err(failed()),
        };
        let id = resource.strip_prefix("memos/").unwrap();
        Ok((
            Document {
                label: format!("Memos · {id}.md"),
                markdown: markdown.into(),
            },
            SourceMetadata {
                provider: Provider::Memos,
                destination: d.id.clone(),
                url: d.url.join(&resource).unwrap().into(),
                resource,
                captured_at: now(),
                updated_at,
                title: None,
                converter: None,
                warnings: vec![],
            },
            stored.metadata.generation,
        ))
    }

    async fn jira_credential(
        self: &Arc<Self>,
        owner: &VerifiedIdentity,
        destination: &str,
        expected: Option<u64>,
        capacity: crate::RequestCapacity,
    ) -> Result<(Credential, u64), AppError> {
        let d = self.destination(destination)?;
        if d.provider != Provider::Jira {
            return Err(AppError::bad_request("Elige una conexión de Jira Cloud."));
        }
        let lock = Arc::clone(
            self.refreshes
                .lock()
                .await
                .entry((owner.clone(), destination.into()))
                .or_default(),
        );
        let guard = lock.lock_owned().await;
        let stored = self
            .store
            .get(owner, d.provider, destination)
            .await?
            .ok_or_else(stale)?;
        if expected.is_some_and(|n| n != stored.metadata.generation) {
            return Err(stale());
        }
        let credential: Credential =
            serde_json::from_value(stored.credential.value().clone()).map_err(|_| failed())?;
        if credential.binding != d.binding(&self.jira) {
            return Err(failed());
        }
        let generation = stored.metadata.generation;
        if credential.expires_at.is_some_and(|n| n > now() + 30) {
            self.jira_resources(d, &credential.access_token).await?;
            if self
                .store
                .metadata(owner, d.provider, destination)
                .await?
                .generation
                != generation
            {
                return Err(stale());
            }
            return Ok((credential, generation));
        }
        let refresh = credential.refresh_token.ok_or_else(failed)?;
        let this = Arc::clone(self);
        let owner = owner.clone();
        let destination = destination.to_owned();
        // A rotating refresh token must be persisted even if its HTTP caller disconnects.
        tokio::spawn(async move {
            let _capacity = capacity;
            let _refresh = guard;
            let d = this.destination(&destination)?;
            let next = this
                .exchange(
                    d,
                    json!({"grant_type":"refresh_token","refresh_token":refresh}),
                )
                .await?;
            let revision = this.save(&owner, d, generation, next).await?["revision"]
                .as_u64()
                .ok_or_else(failed)?;
            let stored = this
                .store
                .get(&owner, d.provider, &destination)
                .await?
                .ok_or_else(stale)?;
            if stored.metadata.generation != revision {
                return Err(stale());
            }
            let next: Credential =
                serde_json::from_value(stored.credential.value().clone()).map_err(|_| failed())?;
            this.jira_resources(d, &next.access_token).await?;
            if this
                .store
                .metadata(&owner, d.provider, &destination)
                .await?
                .generation
                != revision
            {
                return Err(stale());
            }
            Ok((next, revision))
        })
        .await
        .map_err(|_| failed())?
    }

    async fn read_issue(
        self: &Arc<Self>,
        owner: &VerifiedIdentity,
        destination: &str,
        reference: &str,
        capacity: crate::RequestCapacity,
        workspace: &mddiff::workspace::Workspace,
    ) -> Result<(Document, SourceMetadata, u64), AppError> {
        let d = self.destination(destination)?;
        if d.provider != Provider::Jira {
            return Err(AppError::bad_request("Elige una conexión de Jira Cloud."));
        }
        let requested = issue_key(reference, &d.url)?;
        let (credential, generation) = self
            .jira_credential(owner, destination, None, Arc::clone(&capacity))
            .await?;
        let mut url = self.jira_endpoint(
            &format!(
                "/ex/jira/{}/rest/api/3/issue/{requested}",
                d.cloud_id.as_deref().ok_or_else(failed)?
            ),
            false,
        );
        url.query_pairs_mut()
            .append_pair("fields", "summary,description,updated");
        let mut issue = self
            .request_json(
                url,
                &[],
                Some(&credential.access_token),
                None,
                MEMO_RESPONSE_LIMIT,
                true,
            )
            .await?;
        let canonical = issue["key"].as_str().ok_or_else(failed)?.to_owned();
        if issue_key(&canonical, &d.url).ok().as_deref() != Some(canonical.as_str()) {
            return Err(failed());
        }
        let title = issue["fields"]["summary"]
            .as_str()
            .filter(|s| !s.trim().is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control))
            .ok_or_else(failed)?
            .to_owned();
        let updated_at = match issue["fields"].get("updated") {
            None | Some(Value::Null) => None,
            Some(Value::String(value))
                if value.len() <= 64 && !value.chars().any(char::is_control) =>
            {
                Some(value.clone())
            }
            _ => return Err(failed()),
        };
        let description = issue["fields"]
            .get_mut("description")
            .filter(|v| !v.is_null())
            .map(Value::take)
            .ok_or_else(|| {
                AppError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "El issue de Jira no tiene descripción. Se conserva el trabajo actual.".into(),
                )
            })?;
        let compute = workspace.compute_permit().await?;
        // Conversion shares the diff budget and retains capacity after caller cancellation.
        let mut converted = tokio::task::spawn_blocking(move || {
            let _capacity = capacity;
            let _compute = compute;
            mddiff::adf::convert(&description)
        })
        .await
        .map_err(|_| failed())?
        .map_err(|e| {
            if e.0 == StatusCode::BAD_REQUEST {
                AppError(StatusCode::BAD_GATEWAY, e.1)
            } else {
                e
            }
        })?;
        if canonical != requested {
            converted.warnings.push(format!(
                "Jira resolvió la referencia solicitada a su clave actual {canonical}."
            ));
        }
        Ok((
            Document {
                label: format!("Jira · {canonical}.md"),
                markdown: converted.markdown,
            },
            SourceMetadata {
                provider: Provider::Jira,
                destination: d.id.clone(),
                resource: canonical.clone(),
                url: d
                    .url
                    .join(&format!("/browse/{canonical}"))
                    .map_err(|_| failed())?
                    .into(),
                captured_at: now(),
                updated_at,
                title: Some(title),
                converter: Some(mddiff::adf::VERSION.into()),
                warnings: converted.warnings,
            },
            generation,
        ))
    }

    async fn jira_resources(&self, d: &Destination, token: &str) -> Result<(), AppError> {
        let result = self
            .request(
                self.jira_endpoint("/oauth/token/accessible-resources", false),
                &[],
                Some(token),
                None,
                RESPONSE_LIMIT,
            )
            .await?;
        let allowed = result.as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["id"].as_str() == d.cloud_id.as_deref()
                    && item["url"]
                        .as_str()
                        .and_then(|s| origin_url(s, false).ok())
                        .as_ref()
                        == Some(&d.url)
                    && item["scopes"].as_array().is_some_and(|scopes| {
                        scopes.iter().any(|s| s == "read:jira-work")
                            && scopes.iter().all(|s| {
                                s.as_str().is_some_and(|s| {
                                    matches!(s, "read:jira-work" | "offline_access")
                                })
                            })
                    })
            })
        });
        if !allowed {
            return Err(failed());
        }
        Ok(())
    }

    async fn exchange(&self, d: &Destination, grant: Value) -> Result<Credential, AppError> {
        let jira = self.jira.as_ref().ok_or_else(invalid)?;
        let mut grant = grant;
        grant["client_id"] = jira.client_id.clone().into();
        grant["client_secret"] = jira.client_secret.clone().into();
        let result = self
            .request(
                self.jira_endpoint("/oauth/token", true),
                &[],
                None,
                Some(grant),
                RESPONSE_LIMIT,
            )
            .await?;
        let access = result["access_token"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 8192 && !s.chars().any(char::is_whitespace))
            .ok_or_else(failed)?;
        let refresh = result["refresh_token"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 8192 && !s.chars().any(char::is_whitespace))
            .ok_or_else(failed)?;
        let lifetime = result["expires_in"]
            .as_u64()
            .filter(|n| *n > 0 && *n <= 7 * 86400)
            .ok_or_else(failed)?;
        if !result["token_type"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("Bearer"))
        {
            return Err(failed());
        }
        if result["scope"].as_str().is_some_and(|s| {
            !s.split_ascii_whitespace()
                .all(|s| matches!(s, "read:jira-work" | "offline_access"))
        }) {
            return Err(failed());
        }
        Ok(Credential {
            binding: d.binding(&self.jira),
            access_token: access.into(),
            refresh_token: Some(refresh.into()),
            expires_at: Some(now() + lifetime),
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revision {
    expected_revision: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemosInput {
    expected_revision: u64,
    token: String,
    acknowledge_full_access: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoCapture {
    destination: String,
    reference: String,
    expected_revision: u64,
}
#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

fn providers(app: &App) -> Result<&Arc<Providers>, AppError> {
    app.connections.as_ref().ok_or_else(|| {
        AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Las conexiones no están habilitadas por administración.".into(),
        )
    })
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/sources/memos", post(capture_memo))
        .route("/api/sources/jira", post(capture_issue))
        .route("/api/connections", get(list))
        .route("/api/connections/{id}/memos", post(memos))
        .route("/api/connections/{id}/authorize", post(authorize))
        .route("/api/connections/{id}/check", post(check))
        .route("/api/connections/{id}", axum::routing::delete(disconnect))
        .route(CALLBACK, get(callback))
}

async fn capture_memo(
    State(app): State<Arc<App>>,
    Extension(owner): Extension<VerifiedIdentity>,
    Extension(session): Extension<SessionHandle>,
    payload: Result<Json<MemoCapture>, JsonRejection>,
) -> Result<Json<Arc<mddiff::workspace::Snapshot>>, AppError> {
    let input = crate::json_payload(payload)?;
    let workspace = app.workspace(Some(Extension(owner.clone()))).await?;
    if workspace.work().await.revision != input.expected_revision {
        return Err(AppError(
            StatusCode::CONFLICT,
            "El trabajo cambió. Recarga antes de abrir otra nota.".into(),
        ));
    }
    let p = providers(&app)?;
    let (base, source, generation) = p
        .read_memo(&owner, &input.destination, &input.reference)
        .await?;
    if app
        .auth
        .as_ref()
        .ok_or_else(failed)?
        .revalidate(&session)
        .await?
        != owner
    {
        return Err(failed());
    }
    workspace
        .capture_base_if_revision(input.expected_revision, base, source, generation)
        .await
        .map(Json)
}

async fn capture_issue(
    State(app): State<Arc<App>>,
    Extension(owner): Extension<VerifiedIdentity>,
    Extension(session): Extension<SessionHandle>,
    Extension(capacity): Extension<crate::RequestCapacity>,
    payload: Result<Json<MemoCapture>, JsonRejection>,
) -> Result<Json<Arc<mddiff::workspace::Snapshot>>, AppError> {
    let input = crate::json_payload(payload)?;
    let workspace = app.workspace(Some(Extension(owner.clone()))).await?;
    if workspace.work().await.revision != input.expected_revision {
        return Err(AppError(
            StatusCode::CONFLICT,
            "El trabajo cambió. Recarga antes de abrir otro issue.".into(),
        ));
    }
    let p = providers(&app)?;
    let (base, source, generation) = p
        .read_issue(
            &owner,
            &input.destination,
            &input.reference,
            capacity,
            workspace.as_ref(),
        )
        .await?;
    if app
        .auth
        .as_ref()
        .ok_or_else(failed)?
        .revalidate(&session)
        .await?
        != owner
    {
        return Err(failed());
    }
    workspace
        .capture_base_if_revision(input.expected_revision, base, source, generation)
        .await
        .map(Json)
}

async fn list(
    State(app): State<Arc<App>>,
    Extension(owner): Extension<VerifiedIdentity>,
    Extension(_session): Extension<SessionHandle>,
) -> Result<Json<Value>, AppError> {
    match &app.connections {
        Some(p) => p.list(&owner).await.map(Json),
        None => Ok(Json(json!({"enabled":false,"destinations":[]}))),
    }
}

async fn memos(
    State(app): State<Arc<App>>,
    Extension(owner): Extension<VerifiedIdentity>,
    Extension(_session): Extension<SessionHandle>,
    RoutePath(id): RoutePath<String>,
    payload: Result<Json<MemosInput>, JsonRejection>,
) -> Result<Json<Value>, AppError> {
    let input = crate::json_payload(payload)?;
    let p = providers(&app)?;
    let d = p.destination(&id)?;
    if d.provider != Provider::Memos || !input.acknowledge_full_access {
        return Err(AppError::bad_request(
            "Confirma el alcance del token personal de Memos.",
        ));
    }
    if p.store.metadata(&owner, d.provider, &id).await?.generation != input.expected_revision {
        return Err(stale());
    }
    p.memos_identity(d, &input.token).await?;
    p.save(
        &owner,
        d,
        input.expected_revision,
        Credential {
            binding: d.binding(&p.jira),
            access_token: input.token,
            refresh_token: None,
            expires_at: None,
        },
    )
    .await
    .map(Json)
}

async fn authorize(
    State(app): State<Arc<App>>,
    Extension(owner): Extension<VerifiedIdentity>,
    Extension(session): Extension<SessionHandle>,
    RoutePath(id): RoutePath<String>,
    payload: Result<Json<Revision>, JsonRejection>,
) -> Result<Json<Value>, AppError> {
    let input = crate::json_payload(payload)?;
    let p = providers(&app)?;
    let d = p.destination(&id)?;
    if d.provider != Provider::Jira {
        return Err(invalid());
    }
    if p.store.metadata(&owner, d.provider, &id).await?.generation != input.expected_revision {
        return Err(stale());
    }
    let state = CsrfToken::new_random().secret().clone();
    let mut pending = p.pending.lock().await;
    pending.retain(|_, v| {
        v.created.elapsed() < Duration::from_secs(300) && !(v.owner == owner && v.destination == id)
    });
    if pending.len() >= 256 {
        return Err(AppError(
            StatusCode::TOO_MANY_REQUESTS,
            "Demasiadas autorizaciones pendientes.".into(),
        ));
    }
    pending.insert(
        state.clone(),
        Pending {
            owner,
            session,
            destination: id,
            revision: input.expected_revision,
            created: Instant::now(),
        },
    );
    let mut url = p.jira_endpoint("/authorize", true);
    url.query_pairs_mut().extend_pairs([
        ("audience", "api.atlassian.com"),
        ("client_id", p.jira.as_ref().unwrap().client_id.as_str()),
        ("scope", SCOPES),
        ("redirect_uri", &format!("{}{CALLBACK}", p.origin)),
        ("state", &state),
        ("response_type", "code"),
        ("prompt", "consent"),
    ]);
    Ok(Json(json!({"authorization_url":url.as_str()})))
}

async fn callback(
    State(app): State<Arc<App>>,
    Extension(owner): Extension<VerifiedIdentity>,
    Extension(session): Extension<SessionHandle>,
    Query(query): Query<Callback>,
) -> Redirect {
    let result = async {
        let p = providers(&app)?;
        let state = query.state.as_deref().ok_or_else(failed)?;
        let pending = {
            let mut map = p.pending.lock().await;
            let value = map.get(state).ok_or_else(failed)?;
            if value.owner != owner || value.session != session || value.created.elapsed() >= Duration::from_secs(300) { return Err(failed()); }
            map.remove(state).unwrap()
        };
        let d = p.destination(&pending.destination)?;
        if query.error.is_some() || p.store.metadata(&owner,d.provider,&d.id).await?.generation != pending.revision { return Err(stale()); }
        let code = query.code.filter(|s| !s.is_empty() && s.len() <= 4096).ok_or_else(failed)?;
        let credential = p.exchange(d,json!({"grant_type":"authorization_code","code":code,"redirect_uri":format!("{}{CALLBACK}",p.origin)})).await?;
        p.jira_resources(d,&credential.access_token).await?;
        p.save(&owner,d,pending.revision,credential).await?;
        Ok::<_,AppError>(())
    }.await;
    Redirect::to(if result.is_ok() {
        "/?connection=connected"
    } else {
        "/?connection=error"
    })
}

async fn check(
    State(app): State<Arc<App>>,
    Extension(owner): Extension<VerifiedIdentity>,
    Extension(_session): Extension<SessionHandle>,
    Extension(capacity): Extension<crate::RequestCapacity>,
    RoutePath(id): RoutePath<String>,
    payload: Result<Json<Revision>, JsonRejection>,
) -> Result<Json<Value>, AppError> {
    let input = crate::json_payload(payload)?;
    let p = providers(&app)?;
    let d = p.destination(&id)?;
    if d.provider == Provider::Jira {
        let (_, revision) = p
            .jira_credential(&owner, &id, Some(input.expected_revision), capacity)
            .await?;
        return Ok(Json(json!({"revision":revision,"connected":true})));
    }
    let stored = p
        .store
        .get(&owner, d.provider, &id)
        .await?
        .ok_or_else(stale)?;
    if stored.metadata.generation != input.expected_revision {
        return Err(stale());
    }
    let credential: Credential =
        serde_json::from_value(stored.credential.value().clone()).map_err(|_| failed())?;
    if credential.binding != d.binding(&p.jira) {
        return Err(failed());
    }
    p.memos_identity(d, &credential.access_token).await?;
    if p.store.metadata(&owner, d.provider, &id).await?.generation != input.expected_revision {
        return Err(stale());
    }
    Ok(Json(
        json!({"revision":input.expected_revision,"connected":true}),
    ))
}

async fn disconnect(
    State(app): State<Arc<App>>,
    Extension(owner): Extension<VerifiedIdentity>,
    Extension(_session): Extension<SessionHandle>,
    RoutePath(id): RoutePath<String>,
    payload: Result<Json<Revision>, JsonRejection>,
) -> Result<Json<Value>, AppError> {
    let input = crate::json_payload(payload)?;
    let p = providers(&app)?;
    let d = p.destination(&id)?;
    let meta = p
        .store
        .delete(&owner, d.provider, &id, input.expected_revision)
        .await?;
    p.pending
        .lock()
        .await
        .retain(|_, v| !(v.owner == owner && v.destination == id));
    Ok(Json(json!({"revision":meta.generation,"connected":false})))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn memo_references_are_local_resources_not_urls_to_fetch() {
        let origin = Url::parse("https://notes.example/").unwrap();
        for reference in [
            "note-1",
            "memos/note-1",
            "https://notes.example/memos/note-1",
        ] {
            assert_eq!(memo_resource(reference, &origin).unwrap(), "memos/note-1");
        }
        for bad in [
            "",
            "-",
            "../note-1",
            "memos/../note-1",
            "https://other.example/memos/note-1",
            "https://notes.example.evil/memos/note-1",
            "https://user@notes.example/memos/note-1",
            "https://notes.example/memos/note-1?token=private",
            "https://notes.example/memos/note-1#part",
            "https://notes.example/memos/a/../note-1",
            "https://notes.example/memos/%6eote-1",
            "https://notes.example\\memos\\note-1",
            "https://notes.example/api/v1/memos/note-1",
            "https://notes.example/memos/shares/secret",
        ] {
            assert!(memo_resource(bad, &origin).is_err(), "{bad}");
        }
    }
    #[test]
    fn jira_references_are_keys_for_one_approved_site() {
        let site = Url::parse("https://team-alpha.atlassian.net/").unwrap();
        for reference in [
            "PROJ-12",
            "proj-12",
            "https://team-alpha.atlassian.net/browse/PROJ-12",
        ] {
            assert_eq!(issue_key(reference, &site).unwrap(), "PROJ-12");
        }
        for reference in [
            "",
            "12",
            "-12",
            "PROJ-0",
            "PROJ-01",
            "PROJ--1",
            "PROJ-1/other",
            "../PROJ-1",
            "https://evil.example/browse/PROJ-1",
            "https://team-alpha.atlassian.net.evil/browse/PROJ-1",
            "https://user@team-alpha.atlassian.net/browse/PROJ-1",
            "https://team-alpha.atlassian.net/browse/PROJ-1?token=private",
            "https://team-alpha.atlassian.net/browse/PROJ-1#part",
            "https://team-alpha.atlassian.net/browse/a/../PROJ-1",
            "https://team-alpha.atlassian.net/browse/%50ROJ-1",
            "https://team-alpha.atlassian.net/rest/api/3/issue/PROJ-1",
        ] {
            assert!(issue_key(reference, &site).is_err(), "{reference}");
        }
    }

    #[test]
    fn destinations_and_network_are_closed_to_arbitrary_hosts_and_metadata() {
        for bad in [
            "http://public.example",
            "https://user:secret@example.com",
            "https://example.com/path",
            "https://example.com/?q=1",
            "https://example.com/#x",
        ] {
            assert!(origin_url(bad, false).is_err());
        }
        assert!(origin_url("https://memos.example", false).is_ok());
        for denied in [
            "127.0.0.1",
            "169.254.169.254",
            "10.1.2.3",
            "192.168.1.1",
            "::1",
            "::ffff:127.0.0.1",
            "fe80::1",
            "fc00::1",
            "100.64.0.1",
            "2002:0a01:0203::1",
            "2001::1",
        ] {
            assert!(!allowed_ip(denied.parse().unwrap(), &[], false), "{denied}");
        }
        let approved: IpAddr = "10.1.2.3".parse().unwrap();
        assert!(allowed_ip(approved, &[approved], false));
        assert!(!allowed_ip("10.1.2.4".parse().unwrap(), &[approved], false));
        assert!(!allowed_ip(
            "169.254.169.254".parse().unwrap(),
            &["169.254.169.254".parse().unwrap()],
            false
        ));
        assert!(allowed_ip("8.8.8.8".parse().unwrap(), &[], false));
    }
}
