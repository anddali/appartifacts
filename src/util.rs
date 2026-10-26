use axum::http::{HeaderMap, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{Hmac, Mac};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

const ID_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
pub const ARTIFACT_ID_LEN: usize = 12;

pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("OS random number generator failed");
    b
}

/// URL-safe random token with `n` bytes of entropy.
pub fn random_token(n: usize) -> String {
    B64.encode(random_bytes(n))
}

/// Lower-case alphanumeric id, safe to use as a DNS label.
pub fn random_id(len: usize) -> String {
    random_bytes(len).iter().map(|b| ID_ALPHABET[(*b as usize) % ID_ALPHABET.len()] as char).collect()
}

pub fn is_valid_artifact_id(s: &str) -> bool {
    s.len() == ARTIFACT_ID_LEN && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

pub fn sha256(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn b64url(data: &[u8]) -> String {
    B64.encode(data)
}

#[derive(Serialize, serde::Deserialize)]
struct Envelope<T> {
    p: String,
    exp: i64,
    d: T,
}

fn mac(secret: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(secret).expect("hmac accepts any key length");
    m.update(msg);
    m.finalize().into_bytes().to_vec()
}

/// Signs `data` for `purpose`, valid for `ttl_secs`. Output is URL and cookie safe.
pub fn sign<T: Serialize>(secret: &[u8], purpose: &str, data: &T, ttl_secs: i64) -> String {
    let env = Envelope { p: purpose.to_string(), exp: chrono::Utc::now().timestamp() + ttl_secs, d: data };
    let body = B64.encode(serde_json::to_vec(&env).expect("serializable"));
    let sig = B64.encode(mac(secret, body.as_bytes()));
    format!("{body}.{sig}")
}

pub fn verify<T: DeserializeOwned>(secret: &[u8], purpose: &str, token: &str) -> Option<T> {
    let (body, sig) = token.split_once('.')?;
    let sig = B64.decode(sig).ok()?;
    let mut m = Hmac::<Sha256>::new_from_slice(secret).ok()?;
    m.update(body.as_bytes());
    m.verify_slice(&sig).ok()?;
    let env: Envelope<T> = serde_json::from_slice(&B64.decode(body).ok()?).ok()?;
    (env.p == purpose && env.exp > chrono::Utc::now().timestamp()).then_some(env.d)
}

pub fn get_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

pub fn set_cookie(name: &str, value: &str, max_age_secs: i64, secure: bool) -> String {
    format!(
        "{name}={value}; Path=/; Max-Age={max_age_secs}; HttpOnly; SameSite=Lax{}",
        if secure { "; Secure" } else { "" }
    )
}

pub fn request_host(headers: &HeaderMap, uri: &axum::http::Uri) -> String {
    headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(str::to_string)
        .or_else(|| uri.authority().map(|a| a.to_string()))
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Only allow same-site relative return paths ("/x", not "//evil" or "/\evil").
pub fn safe_return_path(p: Option<&str>) -> String {
    match p {
        Some(p) if p.starts_with('/') && !p.starts_with("//") && !p.starts_with("/\\") && !p.contains(['\r', '\n']) => {
            p.to_string()
        }
        _ => "/".to_string(),
    }
}

pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_roundtrip_and_purpose() {
        let k = b"k".repeat(32);
        let t = sign(&k, "a", &"hello", 60);
        assert_eq!(verify::<String>(&k, "a", &t).as_deref(), Some("hello"));
        assert!(verify::<String>(&k, "b", &t).is_none());
        assert!(verify::<String>(b"other", "a", &t).is_none());
        assert!(verify::<String>(&k, "a", &sign(&k, "a", &"x", -1)).is_none());
    }

    #[test]
    fn return_paths() {
        assert_eq!(safe_return_path(Some("/a?b=1")), "/a?b=1");
        assert_eq!(safe_return_path(Some("//evil.com")), "/");
        assert_eq!(safe_return_path(Some("/\\evil.com")), "/");
        assert_eq!(safe_return_path(Some("https://evil.com")), "/");
    }

    #[test]
    fn ids() {
        let id = random_id(ARTIFACT_ID_LEN);
        assert!(is_valid_artifact_id(&id));
        assert!(!is_valid_artifact_id("ABC"));
    }
}
