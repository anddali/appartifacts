//! Portal sign-in routes: Entra SSO, optional development login, logout.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::{Form, Router, routing::get, routing::post};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::auth;
use crate::error::{AppError, AppResult};
use crate::util::{b64url, get_cookie, html_escape, random_token, safe_return_path, set_cookie, sha256, sign, verify};

const OIDC_COOKIE: &str = "apphost_oidc";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/dev-login", post(dev_login))
        .route("/auth/logout", post(logout).get(logout_page))
}

pub fn login_redirect(return_to: &str) -> Response {
    Redirect::to(&format!("/auth/login?return={}", urlencoding::encode(return_to))).into_response()
}

#[derive(Deserialize)]
struct LoginQuery {
    #[serde(rename = "return")]
    ret: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct OidcState {
    state: String,
    nonce: String,
    verifier: String,
    ret: String,
}

async fn login(State(app): State<AppState>, Query(q): Query<LoginQuery>) -> AppResult<Response> {
    let ret = safe_return_path(q.ret.as_deref());
    if let Some(entra) = &app.entra {
        let st = OidcState { state: random_token(16), nonce: random_token(16), verifier: random_token(32), ret };
        let challenge = b64url(&sha256(st.verifier.as_bytes()));
        let url = entra
            .authorize_url(&format!("{}/auth/callback", app.cfg.public_url), &st.state, &st.nonce, &challenge)
            .await?;
        let cookie = set_cookie(&app.cfg.cookie_name(OIDC_COOKIE), &sign(&app.cfg.secret, "oidc", &st, 600), 600, app.cfg.secure);
        return Ok(([(header::SET_COOKIE, cookie)], Redirect::to(&url)).into_response());
    }
    // Development login (APPHOST_DEV_LOGIN=true and no Entra configuration).
    Ok(Html(page(
        "Sign in",
        &format!(
            r#"<p class="warn">Development login — no password is checked. Do not enable in production.</p>
<form method="post" action="/auth/dev-login">
  <input type="hidden" name="ret" value="{}">
  <label>Email <input name="email" type="email" required autofocus placeholder="you@example.com"></label>
  <label>Name <input name="name" placeholder="Optional"></label>
  <button type="submit">Sign in</button>
</form>"#,
            html_escape(&ret)
        ),
    ))
    .into_response())
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn callback(State(app): State<AppState>, headers: HeaderMap, Query(q): Query<CallbackQuery>) -> AppResult<Response> {
    let entra = app.entra.as_ref().ok_or_else(|| AppError::not_found("SSO"))?;
    if let Some(err) = q.error {
        return Ok(error_page(StatusCode::UNAUTHORIZED, "Sign-in failed", &format!("{err}: {}", q.error_description.unwrap_or_default())));
    }
    let cookie_name = app.cfg.cookie_name(OIDC_COOKIE);
    let st: OidcState = get_cookie(&headers, &cookie_name)
        .and_then(|c| verify(&app.cfg.secret, "oidc", &c))
        .ok_or_else(|| AppError::bad_request("Sign-in session expired, please try again"))?;
    if q.state.as_deref() != Some(st.state.as_str()) {
        return Err(AppError::bad_request("Sign-in state mismatch, please try again"));
    }
    let code = q.code.ok_or_else(|| AppError::bad_request("Missing authorization code"))?;
    let claims = entra
        .exchange_code(&code, &format!("{}/auth/callback", app.cfg.public_url), &st.verifier, &st.nonce)
        .await
        .map_err(|e| {
            tracing::warn!(error = %format!("{e:#}"), "Entra sign-in failed");
            AppError::new(StatusCode::UNAUTHORIZED, "sso_failed", "Sign-in could not be verified")
        })?;
    let email = claims.email.clone().or(claims.preferred_username.clone()).unwrap_or_else(|| claims.oid.clone());
    let name = claims.name.clone().unwrap_or_else(|| email.clone());
    let user =
        auth::upsert_user(&app, &format!("entra:{}:{}", claims.tid, claims.oid), &email, &name, &claims.groups).await?;
    tracing::info!(user = %user.email, "signed in");
    let session = auth::create_session(&app, user.id).await?;
    let clear = set_cookie(&cookie_name, "", 0, app.cfg.secure);
    let mut res = Redirect::to(&st.ret).into_response();
    res.headers_mut().append(header::SET_COOKIE, session.parse().unwrap());
    res.headers_mut().append(header::SET_COOKIE, clear.parse().unwrap());
    Ok(res)
}

#[derive(Deserialize)]
struct DevLoginForm {
    email: String,
    name: Option<String>,
    ret: Option<String>,
}

async fn dev_login(State(app): State<AppState>, Form(f): Form<DevLoginForm>) -> AppResult<Response> {
    if !app.cfg.dev_login || app.entra.is_some() {
        return Err(AppError::not_found("Page"));
    }
    let email = f.email.trim().to_lowercase();
    if !email.contains('@') {
        return Err(AppError::bad_request("Enter an email address"));
    }
    let name = f.name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| email.split('@').next().unwrap_or("").to_string());
    let user = auth::upsert_user(&app, &format!("dev:{email}"), &email, &name, &[]).await?;
    let cookie = auth::create_session(&app, user.id).await?;
    Ok(([(header::SET_COOKIE, cookie)], Redirect::to(&safe_return_path(f.ret.as_deref()))).into_response())
}

async fn logout(State(app): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    auth::delete_session(&app, &headers).await?;
    let clear = set_cookie(&app.cfg.cookie_name(auth::SESSION_COOKIE), "", 0, app.cfg.secure);
    Ok(([(header::SET_COOKIE, clear)], Redirect::to("/auth/logout")).into_response())
}

async fn logout_page() -> Html<String> {
    Html(page("Signed out", r#"<p>You have been signed out.</p><p><a class="btn" href="/">Sign in again</a></p>"#))
}

pub fn error_page(status: StatusCode, title: &str, message: &str) -> Response {
    (status, Html(page(title, &format!("<p>{}</p><p><a href=\"/\">Back to apphost</a></p>", html_escape(message))))).into_response()
}

/// Minimal standalone page for server-rendered screens (login, consent, errors).
pub fn page(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{t} · apphost</title><link rel="icon" href="/assets/favicon.svg"><link rel="stylesheet" href="/assets/app.css"></head>
<body class="standalone"><main class="card narrow"><div class="brand"><span class="logo"></span>apphost</div><h1>{t}</h1>{body}</main></body></html>"#,
        t = html_escape(title)
    )
}
