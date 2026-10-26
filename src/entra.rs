//! Microsoft Entra ID (OpenID Connect) sign-in: authorization code flow with PKCE and
//! ID token validation against the tenant's JWKS.

use anyhow::{Context, Result, anyhow, bail};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use std::time::{Duration, Instant};
use tokio::sync::{OnceCell, RwLock};

use crate::config::EntraConfig;

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

#[derive(Deserialize, Debug)]
pub struct IdClaims {
    pub oid: String,
    pub tid: String,
    pub name: Option<String>,
    pub preferred_username: Option<String>,
    pub email: Option<String>,
    pub nonce: Option<String>,
    /// Present when the app registration emits group claims (Token configuration > Add groups claim).
    #[serde(default)]
    pub groups: Vec<String>,
}

pub struct Entra {
    cfg: EntraConfig,
    http: reqwest::Client,
    discovery: OnceCell<Discovery>,
    jwks: RwLock<Option<(Instant, JwkSet)>>,
}

impl Entra {
    pub fn new(cfg: EntraConfig) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().expect("http client");
        Self { cfg, http, discovery: OnceCell::new(), jwks: RwLock::new(None) }
    }

    async fn discovery(&self) -> Result<&Discovery> {
        self.discovery
            .get_or_try_init(|| async {
                let url = format!(
                    "https://login.microsoftonline.com/{}/v2.0/.well-known/openid-configuration",
                    self.cfg.tenant_id
                );
                let d: Discovery = self.http.get(&url).send().await?.error_for_status()?.json().await?;
                Ok::<_, anyhow::Error>(d)
            })
            .await
            .context("loading Entra OpenID configuration")
    }

    pub async fn authorize_url(&self, redirect_uri: &str, state: &str, nonce: &str, code_challenge: &str) -> Result<String> {
        let d = self.discovery().await?;
        let mut url = url::Url::parse(&d.authorization_endpoint)?;
        url.query_pairs_mut()
            .append_pair("client_id", &self.cfg.client_id)
            .append_pair("response_type", "code")
            .append_pair("response_mode", "query")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", "openid profile email")
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("code_challenge", code_challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(url.into())
    }

    pub async fn exchange_code(&self, code: &str, redirect_uri: &str, verifier: &str, nonce: &str) -> Result<IdClaims> {
        #[derive(Deserialize)]
        struct TokenResponse {
            id_token: Option<String>,
            error: Option<String>,
            error_description: Option<String>,
        }
        let d = self.discovery().await?;
        let res: TokenResponse = self
            .http
            .post(&d.token_endpoint)
            .form(&[
                ("client_id", self.cfg.client_id.as_str()),
                ("client_secret", self.cfg.client_secret.as_str()),
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("code_verifier", verifier),
                ("scope", "openid profile email"),
            ])
            .send()
            .await?
            .json()
            .await?;
        if let Some(err) = res.error {
            bail!("Entra token error: {err}: {}", res.error_description.unwrap_or_default());
        }
        let id_token = res.id_token.ok_or_else(|| anyhow!("Entra returned no id_token"))?;
        let claims = self.validate(&id_token).await?;
        if claims.nonce.as_deref() != Some(nonce) {
            bail!("ID token nonce mismatch");
        }
        Ok(claims)
    }

    async fn validate(&self, token: &str) -> Result<IdClaims> {
        let d = self.discovery().await?;
        let header = decode_header(token)?;
        if header.alg != Algorithm::RS256 {
            bail!("unexpected ID token algorithm {:?}", header.alg);
        }
        let kid = header.kid.ok_or_else(|| anyhow!("ID token has no kid"))?;
        let key = self.key_for(&kid).await?;
        let mut v = Validation::new(Algorithm::RS256);
        v.set_audience(&[&self.cfg.client_id]);
        v.set_issuer(&[&d.issuer]);
        v.leeway = 120;
        let claims = decode::<IdClaims>(token, &key, &v)?.claims;
        if !d.issuer.contains(&claims.tid) {
            bail!("ID token tenant {} does not match configured tenant", claims.tid);
        }
        Ok(claims)
    }

    async fn key_for(&self, kid: &str) -> Result<DecodingKey> {
        for force in [false, true] {
            if force || self.jwks.read().await.as_ref().is_none_or(|(at, _)| at.elapsed() > Duration::from_secs(3600)) {
                let d = self.discovery().await?;
                let set: JwkSet = self.http.get(&d.jwks_uri).send().await?.error_for_status()?.json().await?;
                *self.jwks.write().await = Some((Instant::now(), set));
            }
            if let Some((_, set)) = self.jwks.read().await.as_ref() {
                if let Some(jwk) = set.find(kid) {
                    return Ok(DecodingKey::from_jwk(jwk)?);
                }
            }
        }
        bail!("signing key {kid} not found in Entra JWKS")
    }
}
