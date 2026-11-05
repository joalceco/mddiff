//! Loopback-only identity experiment; never deploy this fixture publicly.
//! shortcut: synthetic work is process-local; add durable storage in S04 before hosting.

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use openidconnect::{
    AccessTokenHash, AuthorizationCode, ClientId, CsrfToken, EndpointMaybeSet, EndpointNotSet,
    EndpointSet, IssuerUrl, Nonce, OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, Scope, TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
};
use rmcp::{
    ServerHandler,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Implementation, ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
    transport::{
        StreamableHttpServerConfig, StreamableHttpService,
        streamable_http_server::session::local::LocalSessionManager,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tower::ServiceExt;

const WEB: &str = "http://127.0.0.1:3850";
const MCP: &str = "http://127.0.0.1:3850/mcp";
const ISSUER: &str = "http://localhost:8848/realms/mddiff-spike";
const SCOPE: &str = "mddiff:compare";
type Oidc = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;
type Failure = (StatusCode, Json<Value>);

#[derive(Deserialize)]
struct Runtime {
    issuer: String,
    resource_client: ResourceClient,
}
#[derive(Deserialize)]
struct ResourceClient {
    client_id: String,
    secret: String,
}

struct Pending {
    verifier: PkceCodeVerifier,
    nonce: Nonce,
    created: Instant,
}
struct Session {
    token: String,
    created: Instant,
}
struct App {
    oidc: Oidc,
    http: reqwest::Client,
    resource: ResourceClient,
    pending: Mutex<HashMap<String, Pending>>,
    sessions: Mutex<HashMap<String, Session>>,
    works: Mutex<HashMap<String, Work>>,
}
#[derive(Clone, Default)]
struct Work {
    revision: u64,
    original: String,
    proposal: String,
}
#[derive(Clone)]
struct Principal {
    subject: String,
}

fn fail(status: StatusCode, message: &str) -> Failure {
    (status, Json(json!({"error":message})))
}

fn has_audience(value: &Value, expected: &str) -> bool {
    value.as_str() == Some(expected)
        || value
            .as_array()
            .is_some_and(|values| values.iter().any(|v| v.as_str() == Some(expected)))
}

fn principal(claims: &Value) -> Result<Principal, Failure> {
    if claims["active"] != true
        || claims["iss"].as_str() != Some(ISSUER)
        || !has_audience(&claims["aud"], MCP)
        || claims["sub"].as_str().is_none_or(str::is_empty)
    {
        return Err(fail(
            StatusCode::UNAUTHORIZED,
            "Token inactive or not issued for this resource.",
        ));
    }
    if !claims["scope"]
        .as_str()
        .unwrap_or("")
        .split_whitespace()
        .any(|s| s == SCOPE)
        || !claims["realm_access"]["roles"]
            .as_array()
            .is_some_and(|roles| roles.iter().any(|r| r.as_str() == Some("mddiff-user")))
    {
        return Err(fail(StatusCode::FORBIDDEN, "Admission or scope missing."));
    }
    Ok(Principal {
        subject: claims["sub"].as_str().unwrap().to_owned(),
    })
}

impl App {
    async fn verify(&self, token: &str) -> Result<Principal, Failure> {
        let response = self
            .http
            .post(format!("{ISSUER}/protocol/openid-connect/token/introspect"))
            .basic_auth(&self.resource.client_id, Some(&self.resource.secret))
            .form(&[("token", token)])
            .send()
            .await
            .map_err(|_| {
                fail(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Identity provider unavailable.",
                )
            })?;
        if !response.status().is_success() {
            return Err(fail(
                StatusCode::SERVICE_UNAVAILABLE,
                "Introspection unavailable.",
            ));
        }
        let claims: Value = response.json().await.map_err(|_| {
            fail(
                StatusCode::SERVICE_UNAVAILABLE,
                "Invalid introspection response.",
            )
        })?;
        principal(&claims)
    }

    async fn authenticate(&self, headers: &HeaderMap) -> Result<Principal, Failure> {
        if let Some(value) = headers.get(header::AUTHORIZATION) {
            let token = value
                .to_str()
                .ok()
                .and_then(|v| v.strip_prefix("Bearer "))
                .filter(|v| !v.is_empty())
                .ok_or_else(|| fail(StatusCode::UNAUTHORIZED, "Bearer required."))?;
            return self.verify(token).await;
        }
        let session_id = cookie(headers, "mddiff-spike-session")
            .ok_or_else(|| fail(StatusCode::UNAUTHORIZED, "Sign in first."))?;
        let token = {
            let mut sessions = self.sessions.lock().unwrap();
            sessions.retain(|_, s| s.created.elapsed() < Duration::from_secs(120));
            sessions.get(&session_id).map(|s| s.token.clone())
        }
        .ok_or_else(|| fail(StatusCode::UNAUTHORIZED, "Session expired."))?;
        self.verify(&token).await
    }

    fn state(&self, user: &Principal) -> Value {
        let work = self
            .works
            .lock()
            .unwrap()
            .get(&user.subject)
            .cloned()
            .unwrap_or_default();
        json!({"issuer":ISSUER,"subject":user.subject,"revision":work.revision,
            "original":work.original,"proposal":work.proposal,"viewer_url":format!("{WEB}/api/state")})
    }

    fn compare(&self, user: &Principal, input: CompareInput) -> Result<Value, &'static str> {
        if input.original.len() > 2 * 1024 * 1024 || input.proposal.len() > 2 * 1024 * 1024 {
            return Err("Document exceeds 2 MiB.");
        }
        let mut works = self.works.lock().unwrap();
        let work = works.entry(user.subject.clone()).or_default();
        if input.expected_revision.is_some_and(|r| r != work.revision) {
            return Err("Revision conflict; existing work preserved.");
        }
        work.revision += 1;
        work.original = input.original;
        work.proposal = input.proposal;
        Ok(
            json!({"issuer":ISSUER,"subject":user.subject,"revision":work.revision,"viewer_url":format!("{WEB}/api/state")}),
        )
    }
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then(|| value.to_owned())
        })
}

async fn login(State(app): State<Arc<App>>) -> Response {
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state, nonce) = app
        .oidc
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("profile".into()))
        .add_scope(Scope::new(SCOPE.into()))
        .set_pkce_challenge(challenge)
        .url();
    let mut pending = app.pending.lock().unwrap();
    pending.retain(|_, p| p.created.elapsed() < Duration::from_secs(300));
    pending.insert(
        state.secret().clone(),
        Pending {
            verifier,
            nonce,
            created: Instant::now(),
        },
    );
    let mut response = Redirect::to(url.as_str()).into_response();
    // This experiment binds loopback only; production requires HTTPS and Secure cookies.
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "mddiff-spike-login={}; HttpOnly; SameSite=Lax; Path=/; Max-Age=300",
            state.secret()
        ))
        .unwrap(),
    );
    response
}

#[derive(Deserialize)]
struct Callback {
    code: String,
    state: String,
}
async fn callback(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(query): Query<Callback>,
) -> Result<Response, Failure> {
    if cookie(&headers, "mddiff-spike-login").as_deref() != Some(query.state.as_str()) {
        return Err(fail(StatusCode::BAD_REQUEST, "State cookie mismatch."));
    }
    let pending = app
        .pending
        .lock()
        .unwrap()
        .remove(&query.state)
        .filter(|p| p.created.elapsed() < Duration::from_secs(300))
        .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "State expired or reused."))?;
    let response = app
        .oidc
        .exchange_code(AuthorizationCode::new(query.code))
        .map_err(|_| {
            fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "OIDC configuration invalid.",
            )
        })?
        .set_pkce_verifier(pending.verifier)
        .request_async(&app.http)
        .await
        .map_err(|_| fail(StatusCode::UNAUTHORIZED, "Code exchange rejected."))?;
    let id_token = response
        .id_token()
        .ok_or_else(|| fail(StatusCode::UNAUTHORIZED, "ID token missing."))?;
    let verifier = app.oidc.id_token_verifier();
    let claims = id_token
        .claims(&verifier, &pending.nonce)
        .map_err(|_| fail(StatusCode::UNAUTHORIZED, "ID token verification rejected."))?;
    if let Some(expected) = claims.access_token_hash() {
        let actual = AccessTokenHash::from_token(
            response.access_token(),
            id_token
                .signing_alg()
                .map_err(|_| fail(StatusCode::UNAUTHORIZED, "Signing algorithm rejected."))?,
            id_token
                .signing_key(&verifier)
                .map_err(|_| fail(StatusCode::UNAUTHORIZED, "Signing key rejected."))?,
        )
        .map_err(|_| fail(StatusCode::UNAUTHORIZED, "Access token hash rejected."))?;
        if actual != *expected {
            return Err(fail(
                StatusCode::UNAUTHORIZED,
                "Access token hash mismatch.",
            ));
        }
    }
    let user = app.verify(response.access_token().secret()).await?;
    if user.subject != claims.subject().as_str() {
        return Err(fail(
            StatusCode::UNAUTHORIZED,
            "Web and resource identities differ.",
        ));
    }
    let session = CsrfToken::new_random().secret().clone();
    app.sessions.lock().unwrap().insert(
        session.clone(),
        Session {
            token: response.access_token().secret().clone(),
            created: Instant::now(),
        },
    );
    let mut result = Redirect::to("/api/state").into_response();
    result.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "mddiff-spike-session={session}; HttpOnly; SameSite=Lax; Path=/; Max-Age=120"
        ))
        .unwrap(),
    );
    result.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_static("mddiff-spike-login=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0"),
    );
    Ok(result)
}

async fn get_state(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> Result<Json<Value>, Failure> {
    let user = app.authenticate(&headers).await?;
    Ok(Json(app.state(&user)))
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CompareInput {
    original: String,
    proposal: String,
    expected_revision: Option<u64>,
}

async fn compare(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(input): Json<CompareInput>,
) -> Result<Json<Value>, Failure> {
    if !headers.contains_key(header::AUTHORIZATION)
        && headers.get(header::ORIGIN).and_then(|h| h.to_str().ok()) != Some(WEB)
    {
        return Err(fail(
            StatusCode::FORBIDDEN,
            "Origin required for cookie-authenticated writes.",
        ));
    }
    let user = app.authenticate(&headers).await?;
    app.compare(&user, input)
        .map(Json)
        .map_err(|message| fail(StatusCode::CONFLICT, message))
}

#[derive(Clone)]
struct Mcp {
    app: Arc<App>,
    user: Principal,
}
#[tool_router]
impl Mcp {
    #[tool(
        description = "Identity experiment: deliver synthetic Markdown only to the authenticated user's in-memory comparison."
    )]
    async fn open_comparison(&self, Parameters(input): Parameters<CompareInput>) -> CallToolResult {
        match self.app.compare(&self.user, input) {
            Ok(value) => CallToolResult::structured(value),
            Err(message) => CallToolResult::error(vec![rmcp::model::ContentBlock::text(message)]),
        }
    }
}
#[tool_handler]
impl ServerHandler for Mcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("mddiff-identity-spike", "0.0.0"))
    }
}

async fn mcp(State(app): State<Arc<App>>, request: Request) -> Response {
    // MCP requires bearer authorization at the HTTP boundary, never a web-session fallback.
    if !request.headers().contains_key(header::AUTHORIZATION) {
        return challenge(StatusCode::UNAUTHORIZED, "Bearer required.");
    }
    let user = match app.authenticate(request.headers()).await {
        Ok(user) => user,
        Err((status, body)) => {
            return challenge(
                status,
                body.0["error"]
                    .as_str()
                    .unwrap_or("Authorization rejected."),
            );
        }
    };
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_max_request_body_bytes(5 * 1024 * 1024);
    let service = StreamableHttpService::new(
        move || {
            Ok(Mcp {
                app: Arc::clone(&app),
                user: user.clone(),
            })
        },
        Arc::new(LocalSessionManager::default()),
        config,
    );
    match service.oneshot(request).await {
        Ok(response) => response.map(Body::new),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn challenge(status: StatusCode, message: &str) -> Response {
    let mut response = fail(status, message).into_response();
    response.headers_mut().insert(header::WWW_AUTHENTICATE,HeaderValue::from_static("Bearer resource_metadata=\"http://127.0.0.1:3850/.well-known/oauth-protected-resource\", scope=\"openid profile mddiff:compare\""));
    response
}

async fn metadata() -> Json<Value> {
    Json(
        json!({"resource":MCP,"authorization_servers":[ISSUER],"scopes_supported":["openid","profile",SCOPE],"bearer_methods_supported":["header"]}),
    )
}

async fn boundary(request: Request, next: Next) -> Response {
    let headers = request.headers();
    if headers.get(header::HOST).and_then(|v| v.to_str().ok()) != Some("127.0.0.1:3850")
        || headers.get(header::ORIGIN).is_some_and(|o| o != WEB)
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'; base-uri 'none'"),
    );
    response
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/identity-spike/runtime.json".into());
    let file_metadata = std::fs::symlink_metadata(Path::new(&path))?;
    if !file_metadata.is_file() {
        return Err("Runtime must be a regular private file.".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file_metadata.permissions().mode() & 0o077 != 0 {
            return Err("Runtime file must have mode 0600.".into());
        }
    }
    let runtime: Runtime = serde_json::from_slice(&std::fs::read(path)?)?;
    if runtime.issuer != ISSUER {
        return Err("This experiment supports only its local Keycloak fixture.".into());
    }
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()?;
    let provider =
        CoreProviderMetadata::discover_async(IssuerUrl::new(ISSUER.into())?, &http).await?;
    let oidc =
        CoreClient::from_provider_metadata(provider, ClientId::new("mddiff-web".into()), None)
            .set_redirect_uri(RedirectUrl::new(format!("{WEB}/callback"))?);
    let app = Arc::new(App {
        oidc,
        http,
        resource: runtime.resource_client,
        pending: Mutex::new(HashMap::new()),
        sessions: Mutex::new(HashMap::new()),
        works: Mutex::new(HashMap::new()),
    });
    let router=Router::new()
        .route("/",get(||async{Html("<h1>mddiff identity experiment</h1><p>Synthetic accounts only. Work is in memory.</p><a href='/login'>Sign in with Keycloak</a><p><a href='/api/state'>Read my current work</a></p>")}))
        .route("/login",get(login)).route("/callback",get(callback))
        .route("/api/state",get(get_state)).route("/api/compare",post(compare))
        .route("/mcp",axum::routing::any(mcp))
        .route("/.well-known/oauth-protected-resource",get(metadata))
        .route("/.well-known/oauth-protected-resource/mcp",get(metadata))
        .layer(DefaultBodyLimit::max(5*1024*1024)).layer(middleware::from_fn(boundary)).with_state(app);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3850").await?;
    println!("Identity experiment: {WEB} (loopback only; synthetic data; not production)");
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn valid() -> Value {
        json!({"active":true,"iss":ISSUER,"sub":"synthetic-alice","aud":[MCP,"mddiff-resource"],"scope":"openid profile mddiff:compare","realm_access":{"roles":["mddiff-user"]}})
    }
    #[test]
    fn token_requires_resource_identity_scope_and_admission() {
        assert_eq!(principal(&valid()).unwrap().subject, "synthetic-alice");
        for (key, value, status) in [
            ("active", json!(false), StatusCode::UNAUTHORIZED),
            (
                "iss",
                json!("https://other.invalid"),
                StatusCode::UNAUTHORIZED,
            ),
            ("aud", json!(["mddiff-resource"]), StatusCode::UNAUTHORIZED),
            ("sub", json!(""), StatusCode::UNAUTHORIZED),
            ("scope", json!("openid"), StatusCode::FORBIDDEN),
            ("realm_access", json!({"roles":[]}), StatusCode::FORBIDDEN),
        ] {
            let mut claims = valid();
            claims[key] = value;
            assert_eq!(principal(&claims).err().unwrap().0, status);
        }
    }
}
