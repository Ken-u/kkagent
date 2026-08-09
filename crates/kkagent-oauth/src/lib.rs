//! Generic OAuth2 helpers (device code + PKCE). Platform-agnostic; no Kimi identity.

use base64::Engine;
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("connection: {0}")]
    Connection(String),
    #[error("device code expired")]
    DeviceExpired,
    #[error("device code timed out")]
    DeviceTimeout,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenInfo {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub scope: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OAuthFlowConfig {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub auth_url: String,
    pub token_url: String,
    pub device_auth_url: Option<String>,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Debug, Clone)]
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_pkce() -> PkcePair {
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize());
    PkcePair {
        verifier,
        challenge,
    }
}

pub fn authorize_url(cfg: &OAuthFlowConfig, pkce: &PkcePair, state: &str) -> String {
    let scope = cfg.scopes.join(" ");
    format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        cfg.auth_url,
        urlencoding_lite(&cfg.client_id),
        urlencoding_lite(&cfg.redirect_uri),
        urlencoding_lite(&scope),
        urlencoding_lite(state),
        urlencoding_lite(&pkce.challenge),
    )
}

fn urlencoding_lite(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

pub async fn exchange_code(
    cfg: &OAuthFlowConfig,
    code: &str,
    pkce: &PkcePair,
) -> Result<TokenInfo, OAuthError> {
    let client = reqwest::Client::new();
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("redirect_uri", cfg.redirect_uri.clone()),
        ("client_id", cfg.client_id.clone()),
        ("code_verifier", pkce.verifier.clone()),
    ];
    if let Some(secret) = &cfg.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    let resp = client
        .post(&cfg.token_url)
        .form(&form)
        .send()
        .await
        .map_err(|e| OAuthError::Connection(e.to_string()))?;
    if !resp.status().is_success() {
        let t = resp.text().await.unwrap_or_default();
        return Err(OAuthError::Unauthorized(t));
    }
    parse_token_response(resp.json().await.map_err(|e| OAuthError::Connection(e.to_string()))?)
}

pub async fn refresh_access_token(
    cfg: &OAuthFlowConfig,
    refresh_token: &str,
) -> Result<TokenInfo, OAuthError> {
    let client = reqwest::Client::new();
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
        ("client_id", cfg.client_id.clone()),
    ];
    if let Some(secret) = &cfg.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    let resp = client
        .post(&cfg.token_url)
        .form(&form)
        .send()
        .await
        .map_err(|e| OAuthError::Connection(e.to_string()))?;
    if !resp.status().is_success() {
        let t = resp.text().await.unwrap_or_default();
        return Err(OAuthError::Unauthorized(t));
    }
    parse_token_response(resp.json().await.map_err(|e| OAuthError::Connection(e.to_string()))?)
}

pub async fn request_device_authorization(
    cfg: &OAuthFlowConfig,
) -> Result<DeviceAuthorization, OAuthError> {
    let url = cfg
        .device_auth_url
        .clone()
        .ok_or_else(|| OAuthError::Other(anyhow::anyhow!("device_auth_url not configured")))?;
    let client = reqwest::Client::new();
    let scope = cfg.scopes.join(" ");
    let resp = client
        .post(&url)
        .form(&[
            ("client_id", cfg.client_id.as_str()),
            ("scope", scope.as_str()),
        ])
        .send()
        .await
        .map_err(|e| OAuthError::Connection(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(OAuthError::Connection(resp.text().await.unwrap_or_default()));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| OAuthError::Connection(e.to_string()))?;
    Ok(DeviceAuthorization {
        device_code: v["device_code"].as_str().unwrap_or("").into(),
        user_code: v["user_code"].as_str().unwrap_or("").into(),
        verification_uri: v["verification_uri"].as_str().unwrap_or("").into(),
        verification_uri_complete: v
            .get("verification_uri_complete")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        expires_in: v["expires_in"].as_u64().unwrap_or(600),
        interval: v["interval"].as_u64().unwrap_or(5),
    })
}

pub async fn poll_device_token(
    cfg: &OAuthFlowConfig,
    device: &DeviceAuthorization,
) -> Result<TokenInfo, OAuthError> {
    let client = reqwest::Client::new();
    let started = std::time::Instant::now();
    loop {
        if started.elapsed().as_secs() > device.expires_in {
            return Err(OAuthError::DeviceTimeout);
        }
        tokio::time::sleep(std::time::Duration::from_secs(device.interval.max(1))).await;
        let resp = client
            .post(&cfg.token_url)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device.device_code.as_str()),
                ("client_id", cfg.client_id.as_str()),
            ])
            .send()
            .await
            .map_err(|e| OAuthError::Connection(e.to_string()))?;
        let status = resp.status();
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| OAuthError::Connection(e.to_string()))?;
        if status.is_success() && v.get("access_token").is_some() {
            return parse_token_response(v);
        }
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("");
        match err {
            "authorization_pending" | "slow_down" => continue,
            "expired_token" => return Err(OAuthError::DeviceExpired),
            other => {
                return Err(OAuthError::Unauthorized(other.to_string()));
            }
        }
    }
}

fn parse_token_response(v: serde_json::Value) -> Result<TokenInfo, OAuthError> {
    let access = v["access_token"]
        .as_str()
        .ok_or_else(|| OAuthError::Unauthorized("missing access_token".into()))?
        .to_string();
    let expires_in = v["expires_in"].as_i64();
    Ok(TokenInfo {
        access_token: access,
        refresh_token: v
            .get("refresh_token")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        token_type: v
            .get("token_type")
            .and_then(|x| x.as_str())
            .unwrap_or("Bearer")
            .to_string(),
        expires_at: expires_in.map(|s| Utc::now() + chrono::Duration::seconds(s)),
        scope: v
            .get("scope")
            .and_then(|x| x.as_str())
            .map(str::to_string),
    })
}

#[derive(Debug, Default)]
pub struct FileTokenStorage {
    path: PathBuf,
}

impl FileTokenStorage {
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".kkagent")
            .join("oauth-tokens.json")
    }

    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    pub fn load(&self) -> Option<TokenInfo> {
        let data = std::fs::read_to_string(&self.path).ok()?;
        serde_json::from_str(&data).ok()
    }

    pub fn save(&self, token: &TokenInfo) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, serde_json::to_string_pretty(token)?)?;
        Ok(())
    }

    pub fn clear(&self) -> anyhow::Result<()> {
        let _ = std::fs::remove_file(&self.path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_shape() {
        let p = generate_pkce();
        assert!(p.verifier.len() >= 32);
        assert!(!p.challenge.is_empty());
    }
}
