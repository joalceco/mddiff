//! Server-side Keycloak sessions. No provider tokens are returned to the browser.

use axum::{
    Json, Router,
    extract::{Query, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use mddiff::{AppError, workspace::VerifiedIdentity};
use openidconnect::{
    AccessTokenHash, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet,
    EndpointNotSet, EndpointSet, IssuerUrl, Nonce, OAuth2TokenResponse, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

use crate::App;

type Oidc = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;
const LOGIN_TTL: Duration = Duration::from_secs(300);
const SESSION_TTL: Duration = Duration::from_secs(3600);
const MAX_PENDING: usize = 256;
const MAX_SESSIONS: usize = 1024;
pub const REMOTE_SCOPE: &str = "mddiff:compare";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    issuer: String,
    pub public_origin: String,
    client_id: String,
    client_secret: Option<String>,
    introspection_client_id: String,
    introspection_client_secret: String,
    audience: String,
    #[serde(default)]
    allow_insecure_loopback: bool,
}

fn error(status: StatusCode, message: &str) -> AppError {
    AppError(status, message.into())
}
fn configuration_error() -> AppError {
    AppError::bad_request("Configuración OIDC inválida o no segura.")
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, AppError> {
        let metadata = std::fs::symlink_metadata(path).map_err(|_| configuration_error())?;
        if !metadata.is_file() || metadata.len() > 16 * 1024 {
            return Err(configuration_error());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(AppError::bad_request(
                    "El archivo OIDC debe tener permisos 0600.",
                ));
            }
        }
        let mut config: Self =
            serde_json::from_slice(&std::fs::read(path).map_err(|_| configuration_error())?)
                .map_err(|_| configuration_error())?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&mut self) -> Result<(), AppError> {
        let issuer = reqwest::Url::parse(&self.issuer).map_err(|_| configuration_error())?;
        let origin = reqwest::Url::parse(&self.public_origin).map_err(|_| configuration_error())?;
        if !valid_url(&issuer, self.allow_insecure_loopback)
            || !valid_url(&origin, self.allow_insecure_loopback)
            || (self.allow_insecure_loopback && (!loopback(&issuer) || !loopback(&origin)))
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
            || issuer.query().is_some()
            || issuer.fragment().is_some()
            || self
                .client_secret
                .as_ref()
                .is_some_and(|s| s.trim().is_empty())
            || [
                &self.client_id,
                &self.introspection_client_id,
                &self.introspection_client_secret,
                &self.audience,
            ]
            .iter()
            .any(|s| s.trim().is_empty())
        {
            return Err(configuration_error());
        }
        self.public_origin = origin.origin().ascii_serialization();
        Ok(())
    }
}

fn loopback(url: &reqwest::Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    )
}
fn valid_url(url: &reqwest::Url, insecure: bool) -> bool {
    url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && (url.scheme() == "https" || (insecure && url.scheme() == "http" && loopback(url)))
}

struct Pending {
    verifier: PkceCodeVerifier,
    nonce: Nonce,
    created: Instant,
}
struct Session {
    access_token: String,
    refresh_token: Option<String>,
    identity: VerifiedIdentity,
    user_label: String,
    csrf: CsrfToken,
    expires: Instant,
}

/// Opaque capability inserted only after a valid server-side session is checked.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionHandle(String);

pub struct Authenticator {
    config: Config,
    oidc: Oidc,
    http: reqwest::Client,
    introspection_url: String,
    revocation_url: String,
    pending: Mutex<HashMap<String, Pending>>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

impl Authenticator {
    pub fn public_origin(&self) -> &str {
        &self.config.public_origin
    }

    pub fn resource_url(&self) -> String {
        format!("{}/mcp", self.public_origin())
    }

    fn challenge(&self, err: AppError) -> Response {
        let status = err.0;
        let mut response = err.into_response();
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            let reason = if status == StatusCode::UNAUTHORIZED {
                "invalid_token"
            } else {
                "insufficient_scope"
            };
            let value = format!(
                "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\", scope=\"{}\", error=\"{}\"",
                self.public_origin(),
                REMOTE_SCOPE,
                reason
            );
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_str(&value).unwrap(),
            );
        }
        response
    }

    async fn remote_identity(&self, headers: &HeaderMap) -> Result<VerifiedIdentity, AppError> {
        if headers.contains_key(header::COOKIE) {
            return Err(error(
                StatusCode::FORBIDDEN,
                "No mezcles cookies y autorización Bearer.",
            ));
        }
        if headers.get_all(header::AUTHORIZATION).iter().count() != 1 {
            return Err(error(
                StatusCode::UNAUTHORIZED,
                "Envía un único token Bearer de usuario.",
            ));
        }
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split_once(' '))
            .filter(|(scheme, token)| {
                scheme.eq_ignore_ascii_case("bearer")
                    && !token.is_empty()
                    && token.len() <= 16 * 1024
                    && !token.chars().any(char::is_whitespace)
            })
            .map(|(_, token)| token)
            .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "Token Bearer inválido."))?;
        let claims = self.token_claims(token).await?;
        resource_identity(&self.config, &claims)
    }

    pub async fn new(config: Config) -> Result<Self, AppError> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| configuration_error())?;
        let issuer = IssuerUrl::new(config.issuer.clone()).map_err(|_| configuration_error())?;
        let provider = CoreProviderMetadata::discover_async(issuer, &http)
            .await
            .map_err(|_| {
                error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "No se pudo descubrir el proveedor OIDC.",
                )
            })?;
        let discovery: Value = http
            .get(format!(
                "{}/.well-known/openid-configuration",
                config.issuer.trim_end_matches('/')
            ))
            .send()
            .await
            .map_err(|_| {
                error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Proveedor OIDC no disponible.",
                )
            })?
            .error_for_status()
            .map_err(|_| configuration_error())?
            .json()
            .await
            .map_err(|_| configuration_error())?;
        let issuer_url = reqwest::Url::parse(&config.issuer).map_err(|_| configuration_error())?;
        let endpoint = |value: &str| -> Result<String, AppError> {
            let url = reqwest::Url::parse(value).map_err(|_| configuration_error())?;
            if !valid_url(&url, config.allow_insecure_loopback)
                || url.origin() != issuer_url.origin()
                || !url
                    .path()
                    .starts_with(&format!("{}/", issuer_url.path().trim_end_matches('/')))
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(configuration_error());
            }
            Ok(url.to_string())
        };
        endpoint(provider.authorization_endpoint().as_str())?;
        endpoint(
            provider
                .token_endpoint()
                .ok_or_else(configuration_error)?
                .as_str(),
        )?;
        endpoint(provider.jwks_uri().as_str())?;
        let introspection_url = endpoint(
            discovery["introspection_endpoint"]
                .as_str()
                .ok_or_else(configuration_error)?,
        )?;
        let revocation_url = endpoint(
            discovery["revocation_endpoint"]
                .as_str()
                .ok_or_else(configuration_error)?,
        )?;
        let oidc = CoreClient::from_provider_metadata(
            provider,
            ClientId::new(config.client_id.clone()),
            config.client_secret.clone().map(ClientSecret::new),
        )
        .set_redirect_uri(
            RedirectUrl::new(format!("{}/auth/callback", config.public_origin))
                .map_err(|_| configuration_error())?,
        );
        Ok(Self {
            config,
            oidc,
            http,
            introspection_url,
            revocation_url,
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
        })
    }

    pub fn validate_boundary(
        &self,
        method: &Method,
        path: &str,
        headers: &HeaderMap,
    ) -> Result<(), AppError> {
        let authority = self.config.public_origin.split_once("://").unwrap().1;
        if headers.get(header::HOST).and_then(|v| v.to_str().ok()) != Some(authority)
            || headers
                .get(header::ORIGIN)
                .is_some_and(|v| v.to_str().ok() != Some(self.config.public_origin.as_str()))
        {
            return Err(error(StatusCode::FORBIDDEN, "Origen o Host no autorizado."));
        }
        let navigation = method == Method::GET
            && (matches!(
                path,
                "/auth/login" | "/auth/callback" | "/connections/jira/callback"
            ) || (path == "/"
                && headers
                    .get("sec-fetch-mode")
                    .is_some_and(|v| v == "navigate")
                && headers
                    .get("sec-fetch-dest")
                    .is_some_and(|v| v == "document")));
        if headers
            .get("sec-fetch-site")
            .is_some_and(|v| v == "cross-site")
            && !navigation
        {
            return Err(error(
                StatusCode::FORBIDDEN,
                "Solicitud entre sitios no autorizada.",
            ));
        }
        if path == "/mcp" && method == Method::DELETE {
            return Ok(());
        }
        crate::require_json_body(method, headers)
    }

    fn secure(&self) -> bool {
        self.config.public_origin.starts_with("https://")
    }
    fn session_cookie(&self) -> &str {
        if self.secure() {
            "__Host-mddiff-session"
        } else {
            "mddiff-session"
        }
    }
    fn flow_cookie(&self) -> &str {
        if self.secure() {
            "__Host-mddiff-flow"
        } else {
            "mddiff-flow"
        }
    }
    fn cookie(&self, name: &str, value: &str, age: u64) -> HeaderValue {
        HeaderValue::from_str(&format!(
            "{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={age}{}",
            if self.secure() { "; Secure" } else { "" }
        ))
        .unwrap()
    }

    async fn local_session(&self, handle: &SessionHandle) -> Result<Arc<Session>, AppError> {
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, s| s.expires > Instant::now());
        sessions
            .get(&handle.0)
            .cloned()
            .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "La sesión no es válida o caducó."))
    }

    async fn token_claims(&self, access: &str) -> Result<Value, AppError> {
        let response = self
            .http
            .post(&self.introspection_url)
            .basic_auth(
                &self.config.introspection_client_id,
                Some(&self.config.introspection_client_secret),
            )
            .form(&[("token", access)])
            .send()
            .await
            .map_err(|_| {
                error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Proveedor de identidad no disponible.",
                )
            })?;
        if !response.status().is_success() {
            return Err(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "No se pudo verificar la sesión.",
            ));
        }
        let claims: Value = response.json().await.map_err(|_| {
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Respuesta de identidad inválida.",
            )
        })?;
        Ok(claims)
    }

    async fn introspect(&self, access: &str) -> Result<VerifiedIdentity, AppError> {
        identity_from_claims(&self.config, &self.token_claims(access).await?)
    }

    pub async fn revalidate(&self, handle: &SessionHandle) -> Result<VerifiedIdentity, AppError> {
        let session = self.local_session(handle).await?;
        let result = self.introspect(&session.access_token).await;
        match result {
            Ok(identity) if identity == session.identity => {
                // Logout or expiry during introspection must not authorize a completed request.
                let _ = self.local_session(handle).await?;
                Ok(identity)
            }
            Ok(_) => {
                self.sessions.lock().await.remove(&handle.0);
                Err(error(
                    StatusCode::UNAUTHORIZED,
                    "La identidad de la sesión no coincide.",
                ))
            }
            Err(err) => {
                if matches!(err.0, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
                    self.sessions.lock().await.remove(&handle.0);
                }
                Err(err)
            }
        }
    }

    fn handle(&self, headers: &HeaderMap) -> Result<SessionHandle, AppError> {
        cookie(headers, self.session_cookie())
            .map(SessionHandle)
            .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "Inicia sesión para continuar."))
    }

    fn csrf(&self, headers: &HeaderMap, session: &Session) -> Result<(), AppError> {
        if headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
            != Some(self.config.public_origin.as_str())
            || headers
                .get("x-csrf-token")
                .and_then(|v| v.to_str().ok())
                .is_none_or(|v| CsrfToken::new(v.to_owned()) != session.csrf)
        {
            return Err(error(
                StatusCode::FORBIDDEN,
                "Protección CSRF: origen o token inválido.",
            ));
        }
        Ok(())
    }

    async fn revoke(&self, refresh: Option<&str>) {
        if let Some(refresh) = refresh {
            let mut request = self.http.post(&self.revocation_url);
            let mut form = vec![("token", refresh), ("token_type_hint", "refresh_token")];
            if let Some(secret) = &self.config.client_secret {
                request = request.basic_auth(&self.config.client_id, Some(secret));
            } else {
                form.push(("client_id", &self.config.client_id));
            }
            // Local logout is authoritative even if the provider is unavailable.
            let _ = request.form(&form).send().await;
        }
    }
}

fn identity_from_claims(config: &Config, claims: &Value) -> Result<VerifiedIdentity, AppError> {
    if claims["client_id"]
        .as_str()
        .or_else(|| claims["azp"].as_str())
        != Some(config.client_id.as_str())
    {
        return Err(error(
            StatusCode::UNAUTHORIZED,
            "La sesión no es válida o caducó.",
        ));
    }
    verified_identity(config, claims, &config.audience)
}

fn resource_identity(config: &Config, claims: &Value) -> Result<VerifiedIdentity, AppError> {
    let identity = verified_identity(config, claims, &format!("{}/mcp", config.public_origin))?;
    if !claims["scope"].as_str().is_some_and(|s| {
        s.split_ascii_whitespace()
            .any(|scope| scope == REMOTE_SCOPE)
    }) {
        return Err(error(
            StatusCode::FORBIDDEN,
            "El token requiere el permiso mddiff:compare.",
        ));
    }
    Ok(identity)
}

fn verified_identity(
    config: &Config,
    claims: &Value,
    expected_audience: &str,
) -> Result<VerifiedIdentity, AppError> {
    let audience = &claims["aud"];
    let matches_audience = audience.as_str() == Some(expected_audience)
        || audience
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(expected_audience)));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if claims["active"] != true
        || claims["iss"].as_str() != Some(config.issuer.as_str())
        || !matches_audience
        || claims["exp"].as_u64().is_none_or(|e| e <= now)
        || claims["sub"].as_str().is_none_or(|s| s.trim().is_empty())
        || claims["client_id"]
            .as_str()
            .or_else(|| claims["azp"].as_str())
            .is_none_or(str::is_empty)
    {
        return Err(error(
            StatusCode::UNAUTHORIZED,
            "La sesión no es válida o caducó.",
        ));
    }
    if !claims["realm_access"]["roles"]
        .as_array()
        .is_some_and(|r| r.iter().any(|v| v.as_str() == Some("mddiff-user")))
    {
        return Err(error(
            StatusCode::FORBIDDEN,
            "Esta cuenta no tiene acceso a mddiff.",
        ));
    }
    VerifiedIdentity::from_verified_claims(
        config.issuer.clone(),
        claims["sub"].as_str().unwrap().to_owned(),
    )
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut found = None;
    for value in headers.get_all(header::COOKIE) {
        for pair in value.to_str().ok()?.split(';') {
            if let Some((key, value)) = pair.trim().split_once('=')
                && key == name
            {
                if found.is_some() {
                    return None;
                }
                found = Some(value.to_owned());
            }
        }
    }
    found.filter(|s| !s.is_empty())
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/session", get(session_info))
        .route("/auth/logout", post(logout))
        .route("/.well-known/oauth-protected-resource", get(metadata))
        .route("/.well-known/oauth-protected-resource/mcp", get(metadata))
}

async fn metadata(State(app): State<Arc<App>>) -> Result<Json<Value>, AppError> {
    let auth = app.auth.as_ref().ok_or_else(configuration_error)?;
    Ok(Json(
        json!({"resource":auth.resource_url(),"authorization_servers":[auth.config.issuer],
        "scopes_supported":["openid","profile",REMOTE_SCOPE],"bearer_methods_supported":["header"]}),
    ))
}

pub async fn authenticate(
    State(app): State<Arc<App>>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(auth) = &app.auth else {
        return next.run(request).await;
    };
    let path = request.uri().path().to_owned();
    if crate::is_health_request(request.method(), &path)
        || matches!(
            path.as_str(),
            "/.well-known/oauth-protected-resource" | "/.well-known/oauth-protected-resource/mcp"
        )
    {
        return next.run(request).await;
    }
    if request.headers().contains_key(header::AUTHORIZATION) {
        if !matches!(path.as_str(), "/mcp" | "/api/work")
            || (path == "/api/work"
                && !matches!(
                    *request.method(),
                    Method::GET | Method::HEAD | Method::PUT | Method::PATCH
                ))
        {
            return auth.challenge(error(
                StatusCode::FORBIDDEN,
                "Este endpoint no admite acceso con Bearer.",
            ));
        }
        let headers = request.headers().clone();
        let identity = match auth.remote_identity(&headers).await {
            Ok(identity) => identity,
            Err(err) => return auth.challenge(err),
        };
        request.extensions_mut().insert(identity);
        return next.run(request).await;
    }
    if path == "/mcp" {
        return auth.challenge(error(
            StatusCode::UNAUTHORIZED,
            "MCP requiere un token Bearer de usuario.",
        ));
    }
    if matches!(request.uri().path(), "/auth/login" | "/auth/callback")
        || (request.uri().path() == "/auth/logout" && request.method() == Method::POST)
    {
        return next.run(request).await;
    }
    let headers = request.headers().clone();
    let method = request.method().clone();
    let checked = async {
        let handle = auth.handle(&headers)?;
        let identity = auth.revalidate(&handle).await?;
        if !matches!(method, Method::GET | Method::HEAD | Method::OPTIONS) {
            let session = auth.local_session(&handle).await?;
            auth.csrf(&headers, &session)?;
        }
        Ok::<_, AppError>((handle, identity))
    }
    .await;
    match checked {
        Ok((handle, identity)) => {
            request.extensions_mut().insert(handle);
            request.extensions_mut().insert(identity);
            next.run(request).await
        }
        Err(err) if err.0 == StatusCode::UNAUTHORIZED && request.uri().path() == "/" => {
            Redirect::to("/auth/login").into_response()
        }
        Err(err) if path == "/api/work" => auth.challenge(err),
        Err(err) => err.into_response(),
    }
}

async fn login(State(app): State<Arc<App>>, headers: HeaderMap) -> Result<Response, AppError> {
    let auth = app.auth.as_ref().ok_or_else(configuration_error)?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state, nonce) = auth
        .oidc
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("profile".into()))
        .set_pkce_challenge(challenge)
        .url();
    let mut pending = auth.pending.lock().await;
    pending.retain(|_, p| p.created.elapsed() < LOGIN_TTL);
    if let Some(previous) = cookie(&headers, auth.flow_cookie()) {
        pending.remove(&previous);
    }
    if pending.len() >= MAX_PENDING {
        return Err(error(
            StatusCode::TOO_MANY_REQUESTS,
            "Hay demasiados inicios de sesión pendientes.",
        ));
    }
    pending.insert(
        state.secret().clone(),
        Pending {
            verifier,
            nonce,
            created: Instant::now(),
        },
    );
    let mut response = Redirect::to(url.as_str()).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        auth.cookie(auth.flow_cookie(), state.secret(), LOGIN_TTL.as_secs()),
    );
    Ok(response)
}

#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
}

async fn callback(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(query): Query<Callback>,
) -> Result<Response, AppError> {
    let auth = app.auth.as_ref().ok_or_else(configuration_error)?;
    let state = query
        .state
        .ok_or_else(|| error(StatusCode::BAD_REQUEST, "Inicio de sesión no válido."))?;
    if cookie(&headers, auth.flow_cookie())
        .is_none_or(|v| CsrfToken::new(v) != CsrfToken::new(state.clone()))
    {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Inicio de sesión no válido.",
        ));
    }
    let pending = auth
        .pending
        .lock()
        .await
        .remove(&state)
        .filter(|p| p.created.elapsed() < LOGIN_TTL)
        .ok_or_else(|| {
            error(
                StatusCode::BAD_REQUEST,
                "Inicio de sesión caducado o reutilizado.",
            )
        })?;
    let code = query
        .code
        .ok_or_else(|| error(StatusCode::BAD_REQUEST, "Inicio de sesión cancelado."))?;
    let tokens = auth
        .oidc
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|_| configuration_error())?
        .set_pkce_verifier(pending.verifier)
        .request_async(&auth.http)
        .await
        .map_err(|_| {
            error(
                StatusCode::UNAUTHORIZED,
                "No se pudo completar el inicio de sesión.",
            )
        })?;
    let id_token = tokens
        .id_token()
        .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "Identidad OIDC ausente."))?;
    let verifier = auth.oidc.id_token_verifier();
    let claims = id_token
        .claims(&verifier, &pending.nonce)
        .map_err(|_| error(StatusCode::UNAUTHORIZED, "La identidad OIDC no es válida."))?;
    if let Some(expected) = claims.access_token_hash() {
        let actual = AccessTokenHash::from_token(
            tokens.access_token(),
            id_token.signing_alg().map_err(|_| configuration_error())?,
            id_token
                .signing_key(&verifier)
                .map_err(|_| configuration_error())?,
        )
        .map_err(|_| configuration_error())?;
        if actual != *expected {
            return Err(error(
                StatusCode::UNAUTHORIZED,
                "La identidad OIDC no coincide.",
            ));
        }
    }
    let identity = auth.introspect(tokens.access_token().secret()).await?;
    let expected = VerifiedIdentity::from_verified_claims(
        auth.config.issuer.clone(),
        claims.subject().as_str().to_owned(),
    )?;
    if identity != expected {
        return Err(error(
            StatusCode::UNAUTHORIZED,
            "La identidad OIDC no coincide.",
        ));
    }
    let ttl = tokens.expires_in().unwrap_or_default().min(SESSION_TTL);
    if ttl.is_zero() {
        return Err(error(StatusCode::UNAUTHORIZED, "La sesión OIDC caducó."));
    }
    let user_label = claims
        .preferred_username()
        .map(|v| v.as_str())
        .or_else(|| claims.name().and_then(|n| n.get(None)).map(|v| v.as_str()))
        .unwrap_or("Sesión privada")
        .to_owned();
    let session = Arc::new(Session {
        access_token: tokens.access_token().secret().clone(),
        refresh_token: tokens.refresh_token().map(|v| v.secret().clone()),
        identity,
        user_label,
        csrf: CsrfToken::new_random(),
        expires: Instant::now() + ttl,
    });
    let id = CsrfToken::new_random().secret().clone();
    let mut sessions = auth.sessions.lock().await;
    sessions.retain(|_, s| s.expires > Instant::now());
    if let Ok(previous) = auth.handle(&headers) {
        sessions.remove(&previous.0);
    }
    if sessions.len() >= MAX_SESSIONS {
        return Err(error(
            StatusCode::TOO_MANY_REQUESTS,
            "Hay demasiadas sesiones activas.",
        ));
    }
    sessions.insert(id.clone(), session);
    let mut response = Redirect::to("/").into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        auth.cookie(auth.session_cookie(), &id, ttl.as_secs()),
    );
    response
        .headers_mut()
        .append(header::SET_COOKIE, auth.cookie(auth.flow_cookie(), "", 0));
    Ok(response)
}

async fn session_info(
    State(app): State<Arc<App>>,
    axum::Extension(handle): axum::Extension<SessionHandle>,
) -> Result<Json<Value>, AppError> {
    let auth = app.auth.as_ref().ok_or_else(configuration_error)?;
    let session = auth.local_session(&handle).await?;
    Ok(Json(
        json!({"user_label":session.user_label,"csrf_token":session.csrf.secret()}),
    ))
}

async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let Some(auth) = &app.auth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let result = async {
        let handle = auth.handle(&headers)?;
        let session = auth
            .sessions
            .lock()
            .await
            .get(&handle.0)
            .cloned()
            .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "La sesión no es válida."))?;
        auth.csrf(&headers, &session)?;
        auth.sessions.lock().await.remove(&handle.0);
        auth.revoke(session.refresh_token.as_deref()).await;
        Ok::<_, AppError>(())
    }
    .await;
    let mut response = match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => err.into_response(),
    };
    if response.status() != StatusCode::FORBIDDEN {
        response.headers_mut().append(
            header::SET_COOKIE,
            auth.cookie(auth.session_cookie(), "", 0),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        serde_json::from_value(json!({
            "issuer":"https://identity.example/realms/mddiff",
            "public_origin":"https://viewer.example",
            "client_id":"mddiff-web",
            "introspection_client_id":"mddiff-resource",
            "introspection_client_secret":"synthetic-test-only",
            "audience":"mddiff-resource"
        }))
        .unwrap()
    }

    #[test]
    fn remote_tokens_bind_resource_permissions_and_identity_independently_from_web_client() {
        let config = config();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = json!({"active":true,"iss":config.issuer,"sub":"alice","exp":now+60,
            "aud":[config.audience,format!("{}/mcp",config.public_origin)],"client_id":"mddiff-codex",
            "scope":"openid profile mddiff:compare","realm_access":{"roles":["mddiff-user"]}});
        let remote = resource_identity(&config, &claims).unwrap();
        let mut web = claims.clone();
        web["client_id"] = json!(config.client_id);
        assert_eq!(remote, identity_from_claims(&config, &web).unwrap());
        assert_eq!(
            identity_from_claims(&config, &claims).unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
        for (key, value, status) in [
            ("active", json!(false), StatusCode::UNAUTHORIZED),
            (
                "iss",
                json!("https://other.example/realms/test"),
                StatusCode::UNAUTHORIZED,
            ),
            ("aud", json!([config.audience]), StatusCode::UNAUTHORIZED),
            (
                "aud",
                json!(format!("{}/mcp/", config.public_origin)),
                StatusCode::UNAUTHORIZED,
            ),
            ("exp", json!(now), StatusCode::UNAUTHORIZED),
            ("sub", json!(" "), StatusCode::UNAUTHORIZED),
            ("client_id", json!(""), StatusCode::UNAUTHORIZED),
            (
                "scope",
                json!("openid mddiff:compare-extra"),
                StatusCode::FORBIDDEN,
            ),
            ("realm_access", json!({"roles":[]}), StatusCode::FORBIDDEN),
        ] {
            let mut invalid = claims.clone();
            invalid[key] = value;
            assert_eq!(resource_identity(&config, &invalid).unwrap_err().0, status);
        }
    }

    #[tokio::test]
    async fn malformed_or_mixed_remote_credentials_fail_before_introspection() {
        let auth = authenticator(config());
        let mut headers = HeaderMap::new();
        for value in ["Basic synthetic", "Bearer ", "Bearer a b"] {
            headers.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
            assert_eq!(
                auth.remote_identity(&headers).await.unwrap_err().0,
                StatusCode::UNAUTHORIZED
            );
        }
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer synthetic"),
        );
        headers.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer other"),
        );
        assert_eq!(
            auth.remote_identity(&headers).await.unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
        headers.remove(header::AUTHORIZATION);
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer synthetic"),
        );
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("__Host-mddiff-session=synthetic"),
        );
        assert_eq!(
            auth.remote_identity(&headers).await.unwrap_err().0,
            StatusCode::FORBIDDEN
        );
        let response = auth.challenge(error(StatusCode::UNAUTHORIZED, "Inicia sesión."));
        assert!(
            response.headers()[header::WWW_AUTHENTICATE]
                .to_str()
                .unwrap()
                .contains("https://viewer.example/.well-known/oauth-protected-resource/mcp")
        );
    }

    fn authenticator(config: Config) -> Authenticator {
        let provider: CoreProviderMetadata = serde_json::from_value(json!({
            "issuer":config.issuer,
            "authorization_endpoint":format!("{}/auth", config.issuer),
            "token_endpoint":format!("{}/token", config.issuer),
            "jwks_uri":format!("{}/certs", config.issuer),
            "response_types_supported":["code"],
            "subject_types_supported":["public"],
            "id_token_signing_alg_values_supported":["RS256"]
        }))
        .unwrap();
        Authenticator {
            oidc: CoreClient::from_provider_metadata(
                provider,
                ClientId::new(config.client_id.clone()),
                None,
            ),
            config,
            http: reqwest::Client::new(),
            introspection_url: String::new(),
            revocation_url: String::new(),
            pending: Mutex::default(),
            sessions: Mutex::default(),
        }
    }

    #[test]
    fn configuration_requires_https_and_explicit_loopback_exception() {
        assert!(config().validate().is_ok());
        for origin in [
            "http://viewer.example",
            "https://user:pass@viewer.example",
            "https://viewer.example/path",
            "https://viewer.example?query=1",
        ] {
            let mut c = config();
            c.public_origin = origin.into();
            assert!(c.validate().is_err());
        }
        let mut c = config();
        c.issuer = "http://localhost:8848/realms/test".into();
        c.public_origin = "http://127.0.0.1:3848".into();
        assert!(c.validate().is_err());
        c.allow_insecure_loopback = true;
        assert!(c.validate().is_ok());
        c.public_origin = "https://viewer.example".into();
        assert!(c.validate().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn configuration_file_rejects_public_permissions_and_symbolic_links() {
        use std::{
            fs,
            io::Write,
            os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink},
        };
        let folder =
            std::env::temp_dir().join(format!("mddiff-auth-{}", CsrfToken::new_random().secret()));
        fs::create_dir(&folder).unwrap();
        let path = folder.join("oidc.json");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        write!(file, "{{\"issuer\":\"https://identity.example/realms/mddiff\",\"public_origin\":\"https://viewer.example\",\"client_id\":\"mddiff-web\",\"introspection_client_id\":\"resource\",\"introspection_client_secret\":\"synthetic-test-only\",\"audience\":\"resource\"}}").unwrap();
        assert!(Config::load(&path).is_ok());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Config::load(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let link = folder.join("link.json");
        symlink(&path, &link).unwrap();
        assert!(Config::load(&link).is_err());
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn only_active_admitted_claims_for_this_issuer_audience_and_client_create_identity() {
        let config = config();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = json!({"active":true,"iss":config.issuer,"sub":"alice","exp":now+60,
            "aud":["account",config.audience],"client_id":config.client_id,
            "realm_access":{"roles":["mddiff-user"]}});
        let alice = identity_from_claims(&config, &claims).unwrap();
        for (key, value) in [
            ("active", json!(false)),
            ("iss", json!("https://other.example")),
            ("sub", json!("")),
            ("exp", json!(now)),
            ("aud", json!(["other"])),
            ("client_id", json!("other")),
        ] {
            let mut invalid = claims.clone();
            invalid[key] = value;
            assert_eq!(
                identity_from_claims(&config, &invalid).unwrap_err().0,
                StatusCode::UNAUTHORIZED
            );
        }
        let mut other = claims.clone();
        other["sub"] = json!("bob");
        assert_ne!(alice, identity_from_claims(&config, &other).unwrap());
        other["realm_access"]["roles"] = json!([]);
        assert_eq!(
            identity_from_claims(&config, &other).unwrap_err().0,
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn cookies_and_http_boundary_protect_sessions_and_allow_oidc_navigation() {
        let auth = authenticator(config());
        let value = auth.cookie(auth.session_cookie(), "opaque-test-only", 60);
        let value = value.to_str().unwrap();
        for required in [
            "__Host-mddiff-session=",
            "HttpOnly",
            "Secure",
            "SameSite=Lax",
            "Path=/",
        ] {
            assert!(value.contains(required));
        }
        assert!(!value.contains("Domain="));
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("viewer.example"));
        headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(
            auth.validate_boundary(&Method::GET, "/auth/callback", &headers)
                .is_ok()
        );
        assert!(
            auth.validate_boundary(&Method::GET, "/api/state", &headers)
                .is_err()
        );
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://other.example"),
        );
        assert!(
            auth.validate_boundary(&Method::GET, "/auth/callback", &headers)
                .is_err()
        );
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("__Host-mddiff-session=a; __Host-mddiff-session=b"),
        );
        assert!(auth.handle(&headers).is_err());
    }

    #[tokio::test]
    async fn csrf_is_bound_to_the_session_and_expired_or_removed_handles_are_rejected() {
        let auth = authenticator(config());
        let session = Arc::new(Session {
            access_token: "synthetic-test-only".into(),
            refresh_token: None,
            identity: VerifiedIdentity::from_verified_claims(
                auth.config.issuer.clone(),
                "alice".into(),
            )
            .unwrap(),
            user_label: "Alice".into(),
            csrf: CsrfToken::new_random(),
            expires: Instant::now() + SESSION_TTL,
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://viewer.example"),
        );
        assert!(auth.csrf(&headers, &session).is_err());
        headers.insert(
            "x-csrf-token",
            HeaderValue::from_str(session.csrf.secret()).unwrap(),
        );
        assert!(auth.csrf(&headers, &session).is_ok());
        headers.insert("x-csrf-token", HeaderValue::from_static("another-session"));
        assert!(auth.csrf(&headers, &session).is_err());
        let handle = SessionHandle("test-handle".into());
        auth.sessions.lock().await.insert(handle.0.clone(), session);
        assert!(auth.local_session(&handle).await.is_ok());
        auth.sessions.lock().await.remove(&handle.0);
        assert!(auth.local_session(&handle).await.is_err());
        let expired = Arc::new(Session {
            access_token: String::new(),
            refresh_token: None,
            identity: VerifiedIdentity::from_verified_claims(
                auth.config.issuer.clone(),
                "alice".into(),
            )
            .unwrap(),
            user_label: String::new(),
            csrf: CsrfToken::new_random(),
            expires: Instant::now() - Duration::from_secs(1),
        });
        auth.sessions.lock().await.insert(handle.0.clone(), expired);
        assert!(auth.local_session(&handle).await.is_err());
        assert!(auth.sessions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn public_health_uses_only_approved_hosts_and_never_authorizes_private_routes() {
        use axum::body::{Body, to_bytes};
        use mddiff::workspace::Workspaces;
        use tower::ServiceExt;

        let directory = std::env::temp_dir().join(format!(
            "mddiff-health-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let app = Arc::new(App {
            port: 3847,
            mode: crate::Mode::Hosted(Workspaces::persistent(&directory).unwrap()),
            auth: Some(Arc::new(authenticator(config()))),
            connections: None,
        });
        for method in [Method::GET, Method::HEAD] {
            for (host, origin) in [
                ("127.0.0.1:3847", None),
                ("localhost:3847", None),
                ("viewer.example", None),
                ("127.0.0.1:3847", Some("http://127.0.0.1:3847")),
                ("viewer.example", Some("https://viewer.example")),
            ] {
                let mut request = Request::builder()
                    .method(method.clone())
                    .uri("/healthz")
                    .header(header::HOST, host);
                if let Some(origin) = origin {
                    request = request.header(header::ORIGIN, origin);
                }
                let response = crate::router(Arc::clone(&app))
                    .oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::NO_CONTENT, "{method} {host}");
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
                assert!(
                    to_bytes(response.into_body(), 1024)
                        .await
                        .unwrap()
                        .is_empty()
                );
            }
        }
        for (path, host, origin, expected) in [
            ("/healthz", "evil.example", None, StatusCode::FORBIDDEN),
            ("/healthz", "127.0.0.1:3848", None, StatusCode::FORBIDDEN),
            (
                "/healthz",
                "127.0.0.1:3847",
                Some("https://evil.example"),
                StatusCode::FORBIDDEN,
            ),
            (
                "/healthz",
                "viewer.example",
                Some("http://127.0.0.1:3847"),
                StatusCode::FORBIDDEN,
            ),
            ("/api/work", "127.0.0.1:3847", None, StatusCode::FORBIDDEN),
            (
                "/api/work",
                "viewer.example",
                None,
                StatusCode::UNAUTHORIZED,
            ),
            (
                "/api/events",
                "viewer.example",
                None,
                StatusCode::UNAUTHORIZED,
            ),
        ] {
            let mut request = Request::builder()
                .uri(path)
                .header(header::HOST, host)
                .header("x-forwarded-host", "viewer.example")
                .header("x-forwarded-proto", "https")
                .header("forwarded", "for=127.0.0.1;host=viewer.example;proto=https")
                .header("x-user-id", "alice");
            if let Some(origin) = origin {
                request = request.header(header::ORIGIN, origin);
            }
            let response = crate::router(Arc::clone(&app))
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{path} {host}");
        }
        let response = crate::router(Arc::clone(&app))
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/healthz")
                    .header(header::HOST, "viewer.example")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let missing_storage = Arc::new(App {
            port: 3847,
            mode: crate::Mode::Hosted(Workspaces::default()),
            auth: Some(Arc::new(authenticator(config()))),
            connections: None,
        });
        let response = crate::router(missing_storage)
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .header(header::HOST, "127.0.0.1:3847")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn authenticated_cookies_select_private_workspaces_and_cannot_override_the_owner() {
        use axum::body::{Body, to_bytes};
        use mddiff::workspace::{Document, Workspaces};
        use tower::ServiceExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut auth = authenticator(config());
        auth.introspection_url = format!("http://{}/introspect", listener.local_addr().unwrap());
        let provider = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/introspect",
                    post(|body: String| async move {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs();
                        let sub = if body == "token=alice" {
                            "alice"
                        } else {
                            "bob"
                        };
                        Json(
                            json!({"active":true,"iss":"https://identity.example/realms/mddiff",
                    "sub":sub,"exp":now+60,"aud":"mddiff-resource","client_id":"mddiff-web",
                    "realm_access":{"roles":["mddiff-user"]}}),
                        )
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let auth = Arc::new(auth);
        for user in ["alice", "bob"] {
            auth.sessions.lock().await.insert(
                user.into(),
                Arc::new(Session {
                    access_token: user.into(),
                    refresh_token: None,
                    identity: VerifiedIdentity::from_verified_claims(
                        auth.config.issuer.clone(),
                        user.into(),
                    )
                    .unwrap(),
                    user_label: user.into(),
                    csrf: CsrfToken::new_random(),
                    expires: Instant::now() + SESSION_TTL,
                }),
            );
        }
        let app = Arc::new(App {
            port: 3848,
            mode: crate::Mode::Hosted(Workspaces::default()),
            auth: Some(auth.clone()),
            connections: None,
        });
        let alice = auth
            .local_session(&SessionHandle("alice".into()))
            .await
            .unwrap();
        app.workspace(Some(axum::Extension(alice.identity.clone())))
            .await
            .unwrap()
            .replace_content(
                Document {
                    label: "alice-private.md".into(),
                    markdown: "# Only Alice".into(),
                },
                None,
            )
            .await
            .unwrap();
        for (user, expected) in [("alice", Some("alice-private.md")), ("bob", None)] {
            let response = crate::router(app.clone())
                .oneshot(
                    Request::builder()
                        .uri("/api/state?owner_id=alice")
                        .header(header::HOST, "viewer.example")
                        .header(header::COOKIE, format!("__Host-mddiff-session={user}"))
                        .header("x-user-id", "alice")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                    .unwrap();
            assert_eq!(body["base_path"].as_str(), expected);
            if user == "bob" {
                assert!(!body.to_string().contains("Only Alice"));
            }
        }
        provider.abort();
    }
}
