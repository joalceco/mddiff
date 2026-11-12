//! Short-lived, read-only capabilities for one revision; never store Markdown here.
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Extension, Json,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use mddiff::{
    AppError,
    workspace::{VerifiedIdentity, Workspace},
};
use openidconnect::CsrfToken;
use ring::digest;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{App, Mode};

const TTL: Duration = Duration::from_secs(3600);
const MAX_LINKS: usize = 256;
const MAX_PER_USER: usize = 16;

#[derive(Clone)]
struct Grant {
    owner: VerifiedIdentity,
    revision: u64,
    expires: Instant,
}

#[derive(Default)]
pub struct Links {
    grants: Mutex<HashMap<[u8; 32], Grant>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkRequest {
    pub expected_revision: u64,
}

#[derive(Serialize)]
pub struct LinkResponse {
    pub viewer_url: String,
    pub expires_at: u64,
    pub expires_in_seconds: u64,
}

fn unavailable() -> AppError {
    AppError(
        StatusCode::GONE,
        "El enlace de revisión caducó o dejó de estar disponible.".into(),
    )
}

fn key(token: &str) -> Result<[u8; 32], AppError> {
    if token.len() != 43
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(unavailable());
    }
    Ok(digest::digest(&digest::SHA256, token.as_bytes())
        .as_ref()
        .try_into()
        .unwrap())
}

impl Links {
    async fn issue(
        &self,
        owner: VerifiedIdentity,
        revision: u64,
        origin: &str,
    ) -> Result<LinkResponse, AppError> {
        let now = Instant::now();
        let expires_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| {
                AppError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "No se pudo crear el enlace.".into(),
                )
            })?
            .as_secs()
            + TTL.as_secs();
        let mut grants = self.grants.lock().await;
        grants.retain(|_, g| g.expires > now && (g.owner != owner || g.revision == revision));
        if grants.len() >= MAX_LINKS
            || grants.values().filter(|g| g.owner == owner).count() >= MAX_PER_USER
        {
            return Err(AppError(
                StatusCode::TOO_MANY_REQUESTS,
                "Se alcanzó la cuota de enlaces temporales. Espera a que caduquen.".into(),
            ));
        }
        let token = CsrfToken::new_random_len(32).secret().clone();
        grants.insert(
            key(&token)?,
            Grant {
                owner,
                revision,
                expires: now + TTL,
            },
        );
        Ok(LinkResponse {
            viewer_url: format!("{origin}/review#{token}"),
            expires_at,
            expires_in_seconds: TTL.as_secs(),
        })
    }

    async fn resolve(&self, token: &str, now: Instant) -> Result<Grant, AppError> {
        let mut grants = self.grants.lock().await;
        grants.retain(|_, g| g.expires > now);
        grants.get(&key(token)?).cloned().ok_or_else(unavailable)
    }
}

pub async fn create(
    links: &Links,
    workspace: &Workspace,
    owner: VerifiedIdentity,
    revision: u64,
    origin: &str,
) -> Result<LinkResponse, AppError> {
    let snapshot = workspace.snapshot().await;
    if snapshot.revision != revision || snapshot.comparison.is_none() {
        return Err(AppError(
            StatusCode::CONFLICT,
            "El trabajo cambió o está vacío. Lee la revisión antes de crear el enlace.".into(),
        ));
    }
    links.issue(owner, revision, origin).await
}

pub async fn create_link(
    State(app): State<Arc<App>>,
    Extension(owner): Extension<VerifiedIdentity>,
    payload: Result<Json<LinkRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<LinkResponse>, AppError> {
    let input = crate::json_payload(payload)?;
    let workspace = app.workspace(Some(Extension(owner.clone()))).await?;
    let origin = app
        .auth
        .as_ref()
        .expect("review routes require OIDC")
        .public_origin();
    Ok(Json(
        create(
            &app.review,
            &workspace,
            owner,
            input.expected_revision,
            origin,
        )
        .await?,
    ))
}

pub async fn read(State(app): State<Arc<App>>, headers: HeaderMap) -> Result<Response, AppError> {
    if headers.contains_key(header::AUTHORIZATION)
        || headers.get_all("x-mddiff-review").iter().count() != 1
    {
        return Err(unavailable());
    }
    let token = headers
        .get("x-mddiff-review")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(unavailable)?;
    let grant = app.review.resolve(token, Instant::now()).await?;
    let Mode::Hosted(store) = &app.mode else {
        return Err(unavailable());
    };
    let permits = store
        .request_permits(&grant.owner, mddiff::workspace::RequestKind::Content)
        .await?;
    let snapshot = store.for_user(&grant.owner).await?.snapshot().await;
    if snapshot.revision != grant.revision
        || snapshot.comparison.is_none()
        || grant.expires <= Instant::now()
    {
        return Err(unavailable());
    }
    let etag = format!(
        "\"review-{}\"",
        key(token)?
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let mut response = if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|h| h.to_str().ok())
        == Some(&etag)
    {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        Json(snapshot).into_response()
    };
    response
        .headers_mut()
        .insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    response.headers_mut().insert(
        "x-mddiff-review-remaining",
        HeaderValue::from_str(
            &grant
                .expires
                .saturating_duration_since(Instant::now())
                .as_secs()
                .to_string(),
        )
        .unwrap(),
    );
    Ok(crate::retain_capacity(response, Arc::new(permits)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mddiff::workspace::{Document, Workspaces};

    fn owner(name: &str) -> VerifiedIdentity {
        VerifiedIdentity::from_verified_claims("https://id.example".into(), name.into()).unwrap()
    }

    #[tokio::test]
    async fn opaque_links_expire_exactly_after_one_hour_and_do_not_survive_restart() {
        let links = Links::default();
        let start = Instant::now();
        let response = links
            .issue(owner("alice"), 1, "https://viewer.example")
            .await
            .unwrap();
        let issued = links.grants.lock().await.values().next().unwrap().expires - TTL;
        let token = response.viewer_url.split_once('#').unwrap().1;
        assert_eq!(response.expires_in_seconds, 3600);
        assert!(issued >= start);
        assert_eq!(
            links
                .resolve(token, issued + TTL - Duration::from_nanos(1))
                .await
                .unwrap()
                .owner,
            owner("alice")
        );
        assert!(links.resolve(token, issued + TTL).await.is_err());
        assert!(Links::default().resolve(token, start).await.is_err());
        for invalid in ["", "../private", "Bearer test", &"a".repeat(44)] {
            assert!(links.resolve(invalid, start).await.is_err());
        }
    }

    #[tokio::test]
    async fn links_require_current_work_and_have_bounded_metadata_only() {
        let links = Links::default();
        let workspaces = Workspaces::default();
        let workspace = workspaces.for_user(&owner("alice")).await.unwrap();
        assert!(
            create(
                &links,
                &workspace,
                owner("alice"),
                0,
                "https://viewer.example"
            )
            .await
            .is_err()
        );
        workspace
            .replace_if_revision(
                0,
                Document {
                    label: "previo.md".into(),
                    markdown: "# Privado".into(),
                },
                None,
            )
            .await
            .unwrap();
        assert!(
            create(
                &links,
                &workspace,
                owner("alice"),
                0,
                "https://viewer.example"
            )
            .await
            .is_err()
        );
        for _ in 0..MAX_PER_USER {
            create(
                &links,
                &workspace,
                owner("alice"),
                1,
                "https://viewer.example",
            )
            .await
            .unwrap();
        }
        assert_eq!(
            create(
                &links,
                &workspace,
                owner("alice"),
                1,
                "https://viewer.example"
            )
            .await
            .err()
            .unwrap()
            .0,
            StatusCode::TOO_MANY_REQUESTS
        );
        links
            .issue(owner("alice"), 2, "https://viewer.example")
            .await
            .unwrap();
        assert_eq!(links.grants.lock().await.len(), 1);
        assert!(links.grants.lock().await.values().all(|g| g.revision == 2));
    }
    #[tokio::test]
    async fn total_grant_capacity_is_bounded_and_expired_slots_are_reclaimed() {
        let links = Links::default();
        let mut last = String::new();
        for n in 0..MAX_LINKS {
            last = links
                .issue(
                    owner(&format!("user-{}", n / MAX_PER_USER)),
                    1,
                    "https://viewer.example",
                )
                .await
                .unwrap()
                .viewer_url;
        }
        assert_eq!(
            links
                .issue(owner("overflow"), 1, "https://viewer.example")
                .await
                .err()
                .unwrap()
                .0,
            StatusCode::TOO_MANY_REQUESTS
        );
        assert!(
            links
                .resolve(last.split_once('#').unwrap().1, Instant::now() + TTL)
                .await
                .is_err()
        );
        assert!(links.grants.lock().await.is_empty());
        assert!(
            links
                .issue(owner("fresh"), 1, "https://viewer.example")
                .await
                .is_ok()
        );
    }
}
