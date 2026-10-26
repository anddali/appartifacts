use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use url::Url;

#[derive(Clone, Debug)]
pub enum StorageConfig {
    S3 { bucket: String, prefix: String, force_path_style: bool },
    Fs { root: String },
}

#[derive(Clone, Debug)]
pub struct EntraConfig {
    pub tenant_id: String,
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: String,
    /// Portal origin, e.g. https://apphost.corp.example (no trailing slash).
    pub public_url: String,
    /// Artifact pages are served from https://<artifact-id>.<content_domain>.
    pub content_domain: String,
    pub secure: bool,
    pub database_url: String,
    /// HMAC key used for signed cookies and hand-off tokens.
    pub secret: Vec<u8>,
    pub storage: StorageConfig,
    pub entra: Option<EntraConfig>,
    pub dev_login: bool,
    pub session_hours: i64,
    pub csp_script_src: String,
    pub csp_style_src: String,
    pub csp_font_src: String,
    pub csp_img_src: String,
    pub csp_connect_src: String,
    /// Redirect URIs OAuth clients may register (prefix match; a URL without a port allows any port).
    pub oauth_redirect_allowlist: Vec<String>,
    pub max_publish_bytes: usize,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn env_or(name: &str, default: &str) -> String {
    env(name).unwrap_or_else(|| default.to_string())
}

fn env_bool(name: &str) -> bool {
    matches!(env(name).as_deref(), Some("1" | "true" | "yes" | "on"))
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let public_url = env_or("APPHOST_PUBLIC_URL", "http://localhost:8080");
        let parsed = Url::parse(&public_url).context("APPHOST_PUBLIC_URL is not a valid URL")?;
        let secure = parsed.scheme() == "https";
        let public_url = public_url.trim_end_matches('/').to_string();

        let default_content = parsed[url::Position::BeforeHost..url::Position::AfterPort].to_string();
        let content_domain = env_or("APPHOST_CONTENT_DOMAIN", &default_content).to_ascii_lowercase();

        let dev_login = env_bool("APPHOST_DEV_LOGIN");
        let secret = match env("APPHOST_SECRET") {
            Some(s) if s.len() >= 32 => Sha256::digest(s.as_bytes()).to_vec(),
            Some(_) => bail!("APPHOST_SECRET must be at least 32 characters"),
            None if dev_login => {
                tracing::warn!("APPHOST_SECRET not set; using a random secret (content sessions reset on restart)");
                let mut b = vec![0u8; 32];
                getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("{e}"))?;
                b
            }
            None => bail!("APPHOST_SECRET is required"),
        };

        let storage = match env_or("APPHOST_STORAGE", "s3").as_str() {
            "s3" => StorageConfig::S3 {
                bucket: env("APPHOST_S3_BUCKET").context("APPHOST_S3_BUCKET is required for s3 storage")?,
                prefix: env_or("APPHOST_S3_PREFIX", "").trim_matches('/').to_string(),
                force_path_style: env_bool("APPHOST_S3_FORCE_PATH_STYLE"),
            },
            "fs" => StorageConfig::Fs { root: env_or("APPHOST_FS_ROOT", "./data") },
            other => bail!("unknown APPHOST_STORAGE '{other}' (expected s3 or fs)"),
        };

        let entra = match (env("ENTRA_TENANT_ID"), env("ENTRA_CLIENT_ID"), env("ENTRA_CLIENT_SECRET")) {
            (Some(tenant_id), Some(client_id), Some(client_secret)) => {
                Some(EntraConfig { tenant_id, client_id, client_secret })
            }
            (None, None, None) => None,
            _ => bail!("ENTRA_TENANT_ID, ENTRA_CLIENT_ID and ENTRA_CLIENT_SECRET must all be set"),
        };
        if entra.is_none() && !dev_login {
            bail!("configure Entra SSO (ENTRA_*) or set APPHOST_DEV_LOGIN=true for local development");
        }
        if dev_login {
            tracing::warn!("APPHOST_DEV_LOGIN is enabled: anyone can sign in as any email. Never use this in production.");
        }

        let cdns = "https://cdnjs.cloudflare.com https://cdn.jsdelivr.net https://unpkg.com";
        Ok(Config {
            bind: env_or("APPHOST_BIND", "0.0.0.0:8080"),
            public_url,
            content_domain,
            secure,
            database_url: env("DATABASE_URL").context("DATABASE_URL is required")?,
            secret,
            storage,
            entra,
            dev_login,
            session_hours: env_or("APPHOST_SESSION_HOURS", "12").parse().context("APPHOST_SESSION_HOURS")?,
            csp_script_src: env_or("APPHOST_CSP_SCRIPT_SRC", cdns),
            csp_style_src: env_or("APPHOST_CSP_STYLE_SRC", &format!("{cdns} https://fonts.googleapis.com")),
            csp_font_src: env_or("APPHOST_CSP_FONT_SRC", &format!("{cdns} https://fonts.gstatic.com")),
            csp_img_src: env_or("APPHOST_CSP_IMG_SRC", ""),
            csp_connect_src: env_or("APPHOST_CSP_CONNECT_SRC", ""),
            oauth_redirect_allowlist: env_or(
                "APPHOST_OAUTH_REDIRECT_ALLOWLIST",
                "https://claude.ai/api/mcp/auth_callback,https://claude.com/api/mcp/auth_callback,http://localhost/,http://127.0.0.1/",
            )
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
            max_publish_bytes: env_or("APPHOST_MAX_PUBLISH_MB", "100").parse::<usize>().context("APPHOST_MAX_PUBLISH_MB")?
                * 1024
                * 1024,
        })
    }

    fn scheme(&self) -> &'static str {
        if self.secure { "https" } else { "http" }
    }

    pub fn content_origin(&self, artifact_id: &str) -> String {
        format!("{}://{}.{}", self.scheme(), artifact_id, self.content_domain)
    }

    pub fn mcp_url(&self) -> String {
        format!("{}/mcp", self.public_url)
    }

    /// Returns the artifact id when `host` is an artifact content host.
    pub fn artifact_id_from_host<'a>(&self, host: &'a str) -> Option<&'a str> {
        let host = host.strip_suffix('.').unwrap_or(host);
        let lower_ok = host.len() > self.content_domain.len() + 1
            && host[host.len() - self.content_domain.len()..].eq_ignore_ascii_case(&self.content_domain);
        if !lower_ok {
            return None;
        }
        let label = host[..host.len() - self.content_domain.len()].strip_suffix('.')?;
        crate::util::is_valid_artifact_id(label).then_some(label)
    }

    pub fn cookie_name(&self, base: &str) -> String {
        // __Host- cookies cannot be shadowed by cookies set for a parent domain by a sibling artifact.
        if self.secure { format!("__Host-{base}") } else { base.to_string() }
    }
}
