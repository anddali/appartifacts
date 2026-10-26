//! Users, browser sessions and bearer tokens (personal access tokens and OAuth tokens).

use axum::extract::FromRequestParts;
use axum::http::{Method, header, request::Parts};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::AppState;
use crate::error::{AppError, AppResult};
use crate::util::{get_cookie, random_token, sha256};

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub name: String,
    #[serde(skip)]
    pub groups: Vec<String>,
}

pub const SESSION_COOKIE: &str = "apphost_session";
pub const CSRF_HEADER: &str = "x-apphost-csrf";

pub async fn upsert_user(app: &AppState, external_id: &str, email: &str, name: &str, groups: &[String]) -> AppResult<User> {
    Ok(sqlx::query_as::<_, User>(
        "INSERT INTO users (id, external_id, email, name, groups) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (external_id) DO UPDATE
           SET email = EXCLUDED.email, name = EXCLUDED.name, groups = EXCLUDED.groups, last_login_at = now()
         RETURNING id, email, name, groups",
    )
    .bind(Uuid::new_v4())
    .bind(external_id)
    .bind(email.to_lowercase())
    .bind(name)
    .bind(groups)
    .fetch_one(&app.db)
    .await?)
}

pub async fn get_user(app: &AppState, id: Uuid) -> AppResult<Option<User>> {
    Ok(sqlx::query_as::<_, User>("SELECT id, email, name, groups FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(&app.db)
        .await?)
}

/// Creates a session and returns the Set-Cookie header value.
pub async fn create_session(app: &AppState, user_id: Uuid) -> AppResult<String> {
    let token = random_token(32);
    let hours = app.cfg.session_hours;
    sqlx::query("INSERT INTO sessions (token_hash, user_id, expires_at) VALUES ($1, $2, $3)")
        .bind(sha256(token.as_bytes()))
        .bind(user_id)
        .bind(Utc::now() + Duration::hours(hours))
        .execute(&app.db)
        .await?;
    Ok(crate::util::set_cookie(&app.cfg.cookie_name(SESSION_COOKIE), &token, hours * 3600, app.cfg.secure))
}

pub async fn session_user(app: &AppState, parts_headers: &axum::http::HeaderMap) -> AppResult<Option<User>> {
    let Some(token) = get_cookie(parts_headers, &app.cfg.cookie_name(SESSION_COOKIE)) else {
        return Ok(None);
    };
    Ok(sqlx::query_as::<_, User>(
        "SELECT u.id, u.email, u.name, u.groups FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.token_hash = $1 AND s.expires_at > now()",
    )
    .bind(sha256(token.as_bytes()))
    .fetch_optional(&app.db)
    .await?)
}

pub async fn delete_session(app: &AppState, headers: &axum::http::HeaderMap) -> AppResult<()> {
    if let Some(token) = get_cookie(headers, &app.cfg.cookie_name(SESSION_COOKIE)) {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1").bind(sha256(token.as_bytes())).execute(&app.db).await?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenKind {
    Pat,
    OauthAccess,
    OauthRefresh,
}

impl TokenKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenKind::Pat => "pat",
            TokenKind::OauthAccess => "oauth_access",
            TokenKind::OauthRefresh => "oauth_refresh",
        }
    }
    fn prefix(self) -> &'static str {
        match self {
            TokenKind::Pat => "aph_pat_",
            TokenKind::OauthAccess => "aph_at_",
            TokenKind::OauthRefresh => "aph_rt_",
        }
    }
}

pub async fn create_token(
    app: &AppState,
    user_id: Uuid,
    kind: TokenKind,
    name: &str,
    client_id: Option<&str>,
    expires_at: Option<DateTime<Utc>>,
) -> AppResult<(Uuid, String)> {
    let id = Uuid::new_v4();
    let token = format!("{}{}", kind.prefix(), random_token(32));
    sqlx::query(
        "INSERT INTO tokens (id, token_hash, user_id, kind, name, client_id, expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(sha256(token.as_bytes()))
    .bind(user_id)
    .bind(kind.as_str())
    .bind(name)
    .bind(client_id)
    .bind(expires_at)
    .execute(&app.db)
    .await?;
    Ok((id, token))
}

#[derive(sqlx::FromRow)]
pub struct TokenLookup {
    pub token_id: Uuid,
    pub client_id: Option<String>,
    pub id: Uuid,
    pub email: String,
    pub name: String,
    pub groups: Vec<String>,
}

pub async fn lookup_token(app: &AppState, token: &str, kinds: &[TokenKind]) -> AppResult<Option<TokenLookup>> {
    let kinds: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
    let found = sqlx::query_as::<_, TokenLookup>(
        "UPDATE tokens t SET last_used_at = now() FROM users u
         WHERE u.id = t.user_id AND t.token_hash = $1 AND t.kind = ANY($2)
           AND t.revoked_at IS NULL AND (t.expires_at IS NULL OR t.expires_at > now())
         RETURNING t.id AS token_id, t.client_id, u.id, u.email, u.name, u.groups",
    )
    .bind(sha256(token.as_bytes()))
    .bind(&kinds)
    .fetch_optional(&app.db)
    .await?;
    Ok(found)
}

/// The signed-in user on the portal origin, from a bearer token or the session cookie.
pub struct AuthUser {
    pub user: User,
}

pub fn bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token.trim())
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, Self::Rejection> {
        if let Some(token) = bearer(&parts.headers) {
            let found = lookup_token(app, token, &[TokenKind::Pat, TokenKind::OauthAccess]).await?;
            return match found {
                Some(t) => Ok(AuthUser { user: User { id: t.id, email: t.email, name: t.name, groups: t.groups } }),
                None => Err(AppError::new(axum::http::StatusCode::UNAUTHORIZED, "invalid_token", "Invalid or expired token")),
            };
        }
        let user = session_user(app, &parts.headers).await?.ok_or_else(AppError::unauthorized)?;
        // Cookie-authenticated writes must carry a custom header, which cross-site pages cannot send.
        let safe = matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS);
        if !safe && !parts.headers.contains_key(CSRF_HEADER) {
            return Err(AppError::forbidden("Missing CSRF header"));
        }
        Ok(AuthUser { user })
    }
}

/// Periodically removes expired sessions, OAuth codes and tokens.
pub fn spawn_cleanup(app: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            tick.tick().await;
            let r = async {
                sqlx::query("DELETE FROM sessions WHERE expires_at < now()").execute(&app.db).await?;
                sqlx::query("DELETE FROM oauth_codes WHERE expires_at < now()").execute(&app.db).await?;
                sqlx::query(
                    "DELETE FROM tokens WHERE kind <> 'pat' AND (expires_at < now() - interval '1 day' OR revoked_at < now() - interval '1 day')",
                )
                .execute(&app.db)
                .await?;
                Ok::<_, sqlx::Error>(())
            }
            .await;
            if let Err(e) = r {
                tracing::warn!(error = %e, "cleanup failed");
            }
        }
    });
}
