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
    workspace::VerifiedIdentity,
};
use openidconnect::{CsrfToken, url::Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{App, auth::SessionHandle};

const SCOPES: &str = "read:jira-work offline_access";
const CALLBACK: &str = "/connections/jira/callback";
const RESPONSE_LIMIT: usize = 256 * 1024;

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
        let mut response = request.send().await.map_err(|_| failed())?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|n| n > RESPONSE_LIMIT as u64)
        {
            return Err(failed());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| failed())? {
            if bytes.len() + chunk.len() > RESPONSE_LIMIT {
                return Err(failed());
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

    async fn jira_resources(&self, d: &Destination, token: &str) -> Result<(), AppError> {
        let result = self
            .request(
                self.jira_endpoint("/oauth/token/accessible-resources", false),
                &[],
                Some(token),
                None,
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
        .route("/api/connections", get(list))
        .route("/api/connections/{id}/memos", post(memos))
        .route("/api/connections/{id}/authorize", post(authorize))
        .route("/api/connections/{id}/check", post(check))
        .route("/api/connections/{id}", axum::routing::delete(disconnect))
        .route(CALLBACK, get(callback))
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
    RoutePath(id): RoutePath<String>,
    payload: Result<Json<Revision>, JsonRejection>,
) -> Result<Json<Value>, AppError> {
    let input = crate::json_payload(payload)?;
    // Finish refresh rotation and its durable commit even if the caller disconnects.
    tokio::spawn(async move {
        let p = providers(&app)?;
        let d = p.destination(&id)?;
        // Serialize rotating refresh-token use; delete remains free to invalidate an in-flight check.
        let lock = Arc::clone(
            p.refreshes
                .lock()
                .await
                .entry((owner.clone(), id.clone()))
                .or_default(),
        );
        let _refresh = lock.lock().await;
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
        if d.provider == Provider::Memos {
            p.memos_identity(d, &credential.access_token).await?;
        } else {
            let credential = if credential.expires_at.is_none_or(|n| n <= now() + 30) {
                let refresh = credential.refresh_token.as_ref().ok_or_else(failed)?;
                let next = p
                    .exchange(
                        d,
                        json!({"grant_type":"refresh_token","refresh_token":refresh}),
                    )
                    .await?;
                // Save each rotated refresh token before any further network request.
                let revision = p.save(&owner, d, input.expected_revision, next).await?["revision"]
                    .as_u64()
                    .unwrap();
                let stored = p
                    .store
                    .get(&owner, d.provider, &id)
                    .await?
                    .ok_or_else(stale)?;
                if stored.metadata.generation != revision {
                    return Err(stale());
                }
                let next: Credential = serde_json::from_value(stored.credential.value().clone())
                    .map_err(|_| failed())?;
                p.jira_resources(d, &next.access_token).await?;
                if p.store.metadata(&owner, d.provider, &id).await?.generation != revision {
                    return Err(stale());
                }
                return Ok(Json(json!({"revision":revision,"connected":true})));
            } else {
                credential
            };
            p.jira_resources(d, &credential.access_token).await?;
        }
        if p.store.metadata(&owner, d.provider, &id).await?.generation != input.expected_revision {
            return Err(stale());
        }
        Ok(Json(
            json!({"revision":input.expected_revision,"connected":true}),
        ))
    })
    .await
    .map_err(|_| failed())?
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
