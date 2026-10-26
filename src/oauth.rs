//! OAuth 2.1 authorization server for MCP clients (e.g. Claude custom connectors).
//!
//! Entra does not support dynamic client registration, so apphost brokers: clients register
//! and authorize here, the user signs in with Entra via the portal session, and apphost issues
//! its own opaque access/refresh tokens for the /mcp resource.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::{Form, Json, Router, routing::get, routing::post};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::AppState;
use crate::auth::{self, TokenKind};
use crate::error::AppResult;
use crate::login::{error_page, login_redirect, page};
use crate::util::{b64url, html_escape, random_token, sha256, sign, verify};

pub const SCOPE: &str = "apphost";
const ACCESS_TTL_SECS: i64 = 3600;
const REFRESH_TTL_DAYS: i64 = 30;

/// Endpoints called by MCP clients directly (no cookies); safe to expose with open CORS.
pub fn machine_routes() -> Router<AppState> {
    Router::new()
        .route("/.well-known/oauth-protected-resource", get(protected_resource))
        .route("/.well-known/oauth-protected-resource/mcp", get(protected_resource))
        .route("/.well-known/oauth-authorization-server", get(authorization_server))
        .route("/.well-known/oauth-authorization-server/mcp", get(authorization_server))
        .route("/.well-known/openid-configuration", get(authorization_server))
        .route("/oauth/register", post(register))
        .route("/oauth/token", post(token))
}

/// The consent screen, which uses the portal session cookie.
pub fn browser_routes() -> Router<AppState> {
    Router::new().route("/oauth/authorize", get(authorize).post(authorize_decision))
}

/// RFC 9728 metadata; advertised in WWW-Authenticate on 401s from /mcp.
async fn protected_resource(State(app): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({
        "resource": app.cfg.mcp_url(),
        "authorization_servers": [app.cfg.public_url],
        "scopes_supported": [SCOPE],
        "bearer_methods_supported": ["header"],
        "resource_name": "apphost",
    }))
}

/// RFC 8414 metadata.
async fn authorization_server(State(app): State<AppState>) -> Json<serde_json::Value> {
    let base = &app.cfg.public_url;
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "scopes_supported": [SCOPE],
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "token_endpoint_auth_methods_supported": ["none"],
        "code_challenge_methods_supported": ["S256"],
    }))
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    (status, Json(json!({ "error": error, "error_description": description }))).into_response()
}

/// Matches `uri` against the allowlist: same scheme and host, same port unless the entry has
/// none, and a path that starts with the entry's path.
fn redirect_allowed(allowlist: &[String], uri: &str) -> bool {
    let Ok(u) = url::Url::parse(uri) else { return false };
    if u.fragment().is_some() {
        return false;
    }
    allowlist.iter().filter_map(|e| url::Url::parse(e).ok()).any(|e| {
        e.scheme() == u.scheme()
            && e.host_str() == u.host_str()
            && (e.port().is_none() || e.port() == u.port())
            && u.path().starts_with(e.path())
    })
}

#[derive(Deserialize)]
struct RegisterRequest {
    client_name: Option<String>,
    #[serde(default)]
    redirect_uris: Vec<String>,
}

async fn register(State(app): State<AppState>, Json(req): Json<RegisterRequest>) -> AppResult<Response> {
    if req.redirect_uris.is_empty() {
        return Ok(oauth_error(StatusCode::BAD_REQUEST, "invalid_redirect_uri", "redirect_uris is required"));
    }
    if let Some(bad) = req.redirect_uris.iter().find(|u| !redirect_allowed(&app.cfg.oauth_redirect_allowlist, u)) {
        tracing::warn!(redirect_uri = %bad, "rejected OAuth client registration");
        return Ok(oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            &format!("Redirect URI {bad} is not allowed by this apphost instance"),
        ));
    }
    let client_id = format!("aph_client_{}", random_token(16));
    let name: String = req.client_name.unwrap_or_else(|| "MCP client".into()).chars().take(100).collect();
    sqlx::query("INSERT INTO oauth_clients (client_id, client_name, redirect_uris) VALUES ($1, $2, $3)")
        .bind(&client_id)
        .bind(&name)
        .bind(&req.redirect_uris)
        .execute(&app.db)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "client_id": client_id,
            "client_name": name,
            "redirect_uris": req.redirect_uris,
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
            "client_id_issued_at": Utc::now().timestamp(),
        })),
    )
        .into_response())
}

#[derive(Deserialize, Serialize, Clone)]
struct AuthorizeRequest {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    state: Option<String>,
    scope: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    resource: Option<String>,
}

struct ValidRequest {
    client_name: String,
    redirect_uri: String,
}

async fn validate(app: &AppState, q: &AuthorizeRequest) -> AppResult<Result<ValidRequest, Response>> {
    let bad = |m: &str| Ok(Err(error_page(StatusCode::BAD_REQUEST, "Invalid authorization request", m)));
    let Some(client_id) = &q.client_id else { return bad("client_id is missing") };
    let client: Option<(String, Vec<String>)> =
        sqlx::query_as("SELECT client_name, redirect_uris FROM oauth_clients WHERE client_id = $1")
            .bind(client_id)
            .fetch_optional(&app.db)
            .await?;
    let Some((client_name, uris)) = client else { return bad("Unknown client. Remove and re-add the connector.") };
    let redirect_uri = match &q.redirect_uri {
        Some(r) if uris.contains(r) => r.clone(),
        None if uris.len() == 1 => uris[0].clone(),
        _ => return bad("redirect_uri does not match the registered client"),
    };
    if q.response_type.as_deref() != Some("code") {
        return bad("response_type must be code");
    }
    if q.code_challenge.as_deref().is_none_or(str::is_empty) || q.code_challenge_method.as_deref() != Some("S256") {
        return bad("PKCE with S256 is required");
    }
    if let Some(res) = &q.resource {
        if res.trim_end_matches('/') != app.cfg.mcp_url() && res.trim_end_matches('/') != app.cfg.public_url {
            return bad("Unknown resource");
        }
    }
    Ok(Ok(ValidRequest { client_name, redirect_uri }))
}

#[derive(Serialize, Deserialize)]
struct ConsentTicket {
    user_id: Uuid,
    req: AuthorizeRequest,
}

async fn authorize(
    State(app): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    Query(q): Query<AuthorizeRequest>,
) -> AppResult<Response> {
    let valid = match validate(&app, &q).await? {
        Ok(v) => v,
        Err(resp) => return Ok(resp),
    };
    let Some(user) = auth::session_user(&app, &headers).await? else {
        return Ok(login_redirect(&uri.to_string()));
    };
    let ticket = sign(&app.cfg.secret, "consent", &ConsentTicket { user_id: user.id, req: q }, 600);
    let host = url::Url::parse(&valid.redirect_uri).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    let body = format!(
        r#"<p><strong>{client}</strong> wants to access apphost as <strong>{email}</strong>.</p>
<p>It will be able to publish, read, share and delete your artifacts and read and write their shared data.</p>
<p class="muted">You will be returned to {host}.</p>
<form method="post" action="/oauth/authorize" class="row">
  <input type="hidden" name="ticket" value="{ticket}">
  <button type="submit" name="decision" value="allow">Allow</button>
  <button type="submit" name="decision" value="deny" class="secondary">Deny</button>
</form>"#,
        client = html_escape(&valid.client_name),
        email = html_escape(&user.email),
        host = html_escape(&host),
        ticket = html_escape(&ticket),
    );
    let mut res = Html(page("Connect to apphost", &body)).into_response();
    res.headers_mut().insert("x-frame-options", "DENY".parse().unwrap());
    Ok(res)
}

#[derive(Deserialize)]
struct DecisionForm {
    ticket: String,
    decision: String,
}

fn with_params(base: &str, params: &[(&str, &str)]) -> String {
    let mut u = url::Url::parse(base).expect("validated redirect uri");
    {
        let mut qp = u.query_pairs_mut();
        for (k, v) in params {
            qp.append_pair(k, v);
        }
    }
    u.into()
}

async fn authorize_decision(State(app): State<AppState>, headers: HeaderMap, Form(f): Form<DecisionForm>) -> AppResult<Response> {
    let Some(ticket) = verify::<ConsentTicket>(&app.cfg.secret, "consent", &f.ticket) else {
        return Ok(error_page(StatusCode::BAD_REQUEST, "Request expired", "Start the connection again from Claude."));
    };
    // The ticket is bound to the signed-in user, which also protects this form from CSRF.
    let user = auth::session_user(&app, &headers).await?;
    if user.as_ref().map(|u| u.id) != Some(ticket.user_id) {
        return Ok(error_page(StatusCode::FORBIDDEN, "Session changed", "Start the connection again from Claude."));
    }
    let q = ticket.req;
    let valid = match validate(&app, &q).await? {
        Ok(v) => v,
        Err(resp) => return Ok(resp),
    };
    let state = q.state.clone().unwrap_or_default();
    let mut params: Vec<(&str, &str)> = Vec::new();
    if !state.is_empty() {
        params.push(("state", &state));
    }
    params.push(("iss", &app.cfg.public_url));
    if f.decision != "allow" {
        params.push(("error", "access_denied"));
        return Ok(Redirect::to(&with_params(&valid.redirect_uri, &params)).into_response());
    }
    let code = random_token(32);
    sqlx::query(
        "INSERT INTO oauth_codes (code_hash, client_id, user_id, redirect_uri, code_challenge, scope, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(sha256(code.as_bytes()))
    .bind(q.client_id.as_deref().unwrap_or_default())
    .bind(ticket.user_id)
    .bind(&valid.redirect_uri)
    .bind(q.code_challenge.as_deref().unwrap_or_default())
    .bind(SCOPE)
    .bind(Utc::now() + Duration::minutes(5))
    .execute(&app.db)
    .await?;
    tracing::info!(client = %valid.client_name, user = %ticket.user_id, "OAuth access granted");
    params.push(("code", &code));
    Ok(Redirect::to(&with_params(&valid.redirect_uri, &params)).into_response())
}

#[derive(Deserialize)]
struct TokenRequest {
    grant_type: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
}

async fn issue_pair(app: &AppState, user_id: Uuid, client_id: &str, client_name: &str) -> AppResult<Response> {
    let (_, access) = auth::create_token(
        app,
        user_id,
        TokenKind::OauthAccess,
        client_name,
        Some(client_id),
        Some(Utc::now() + Duration::seconds(ACCESS_TTL_SECS)),
    )
    .await?;
    let (_, refresh) = auth::create_token(
        app,
        user_id,
        TokenKind::OauthRefresh,
        client_name,
        Some(client_id),
        Some(Utc::now() + Duration::days(REFRESH_TTL_DAYS)),
    )
    .await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": ACCESS_TTL_SECS,
            "refresh_token": refresh,
            "scope": SCOPE,
        })),
    )
        .into_response())
}

async fn token(State(app): State<AppState>, Form(f): Form<TokenRequest>) -> AppResult<Response> {
    let invalid = |d: &str| Ok(oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", d));
    let client_id = f.client_id.clone().unwrap_or_default();
    match f.grant_type.as_str() {
        "authorization_code" => {
            let (Some(code), Some(verifier)) = (&f.code, &f.code_verifier) else {
                return Ok(oauth_error(StatusCode::BAD_REQUEST, "invalid_request", "code and code_verifier are required"));
            };
            let row: Option<(String, Uuid, String, String, String)> = sqlx::query_as(
                "DELETE FROM oauth_codes c USING oauth_clients cl
                 WHERE c.code_hash = $1 AND c.expires_at > now() AND cl.client_id = c.client_id
                 RETURNING c.client_id, c.user_id, c.redirect_uri, c.code_challenge, cl.client_name",
            )
            .bind(sha256(code.as_bytes()))
            .fetch_optional(&app.db)
            .await?;
            let Some((code_client, user_id, redirect_uri, challenge, client_name)) = row else {
                return invalid("Authorization code is invalid or expired");
            };
            if code_client != client_id {
                return invalid("client_id mismatch");
            }
            if f.redirect_uri.as_deref().is_some_and(|r| r != redirect_uri) {
                return invalid("redirect_uri mismatch");
            }
            if b64url(&sha256(verifier.as_bytes())) != challenge {
                return invalid("PKCE verification failed");
            }
            issue_pair(&app, user_id, &code_client, &client_name).await
        }
        "refresh_token" => {
            let Some(rt) = &f.refresh_token else {
                return Ok(oauth_error(StatusCode::BAD_REQUEST, "invalid_request", "refresh_token is required"));
            };
            let Some(found) = auth::lookup_token(&app, rt, &[TokenKind::OauthRefresh]).await? else {
                return invalid("Refresh token is invalid or expired");
            };
            let token_client = found.client_id.clone().unwrap_or_default();
            if !client_id.is_empty() && client_id != token_client {
                return invalid("client_id mismatch");
            }
            // Rotate: the presented refresh token can be used only once.
            sqlx::query("UPDATE tokens SET revoked_at = now() WHERE id = $1").bind(found.token_id).execute(&app.db).await?;
            let name: String = sqlx::query_scalar("SELECT client_name FROM oauth_clients WHERE client_id = $1")
                .bind(&token_client)
                .fetch_optional(&app.db)
                .await?
                .unwrap_or_else(|| "MCP client".into());
            issue_pair(&app, found.id, &token_client, &name).await
        }
        _ => Ok(oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", "Unsupported grant_type")),
    }
}

#[cfg(test)]
mod tests {
    use super::redirect_allowed;

    #[test]
    fn redirect_allowlist() {
        let list: Vec<String> =
            ["https://claude.ai/api/mcp/auth_callback", "http://localhost/"].iter().map(|s| s.to_string()).collect();
        assert!(redirect_allowed(&list, "https://claude.ai/api/mcp/auth_callback"));
        assert!(redirect_allowed(&list, "http://localhost:53123/callback"));
        assert!(!redirect_allowed(&list, "http://localhost.evil.com/callback"));
        assert!(!redirect_allowed(&list, "https://claude.ai.evil.com/api/mcp/auth_callback"));
        assert!(!redirect_allowed(&list, "https://claude.ai/other"));
    }
}
