//! Generic OAuth2 helpers (device code + PKCE). Platform-agnostic; no Kimi identity.

use base64::Engine;
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const DEFAULT_KIMI_OAUTH_HOST: &str = "https://auth.kimi.com";
pub const DEFAULT_KIMI_CODE_BASE_URL: &str = "https://api.kimi.com/coding/v1";
pub const KIMI_CODE_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";

pub fn kimi_oauth_config(oauth_host: Option<&str>) -> OAuthFlowConfig {
    let host = oauth_host
        .unwrap_or(DEFAULT_KIMI_OAUTH_HOST)
        .trim_end_matches('/');
    OAuthFlowConfig {
        client_id: KIMI_CODE_CLIENT_ID.into(),
        client_secret: None,
        auth_url: format!("{host}/api/oauth/authorize"),
        token_url: format!("{host}/api/oauth/token"),
        device_auth_url: Some(format!("{host}/api/oauth/device_authorization")),
        redirect_uri: String::new(),
        scopes: Vec::new(),
        default_headers: kimi_identity_headers(),
    }
}

pub fn kimi_identity_headers() -> std::collections::HashMap<String, String> {
    let version = env!("CARGO_PKG_VERSION").to_string();
    let device_name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".into());
    let device_id_path = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".kkagent")
        .join("device_id");
    let device_id = std::fs::read_to_string(&device_id_path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            let id = uuid::Uuid::new_v4().to_string();
            if let Some(parent) = device_id_path.parent() {
                let _ = std::fs::create_dir_all(parent);
                let _ = set_private_dir_permissions(parent);
            }
            let _ = std::fs::write(&device_id_path, &id);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &device_id_path,
                    std::fs::Permissions::from_mode(0o600),
                );
            }
            id
        });
    std::collections::HashMap::from([
        ("User-Agent".into(), format!("kkagent/{version}")),
        ("X-Msh-Platform".into(), "kimi_code_cli".into()),
        ("X-Msh-Version".into(), version),
        ("X-Msh-Device-Name".into(), ascii_header(&device_name)),
        (
            "X-Msh-Device-Model".into(),
            format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        ),
        ("X-Msh-Os-Version".into(), std::env::consts::OS.into()),
        ("X-Msh-Device-Id".into(), device_id),
    ])
}

fn ascii_header(value: &str) -> String {
    let value: String = value
        .chars()
        .filter(|character| character.is_ascii() && !character.is_ascii_control())
        .collect();
    let value = value.trim();
    if value.is_empty() {
        "unknown".into()
    } else {
        value.into()
    }
}

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
    pub default_headers: std::collections::HashMap<String, String>,
}

fn request_headers(cfg: &OAuthFlowConfig) -> Result<reqwest::header::HeaderMap, OAuthError> {
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in &cfg.default_headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| OAuthError::Other(error.into()))?;
        let value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|error| OAuthError::Other(error.into()))?;
        headers.insert(name, value);
    }
    Ok(headers)
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
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize());
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
        .headers(request_headers(cfg)?)
        .form(&form)
        .send()
        .await
        .map_err(|e| OAuthError::Connection(e.to_string()))?;
    if !resp.status().is_success() {
        let t = resp.text().await.unwrap_or_default();
        return Err(OAuthError::Unauthorized(t));
    }
    parse_token_response(
        resp.json()
            .await
            .map_err(|e| OAuthError::Connection(e.to_string()))?,
    )
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
        .headers(request_headers(cfg)?)
        .form(&form)
        .send()
        .await
        .map_err(|e| OAuthError::Connection(e.to_string()))?;
    if !resp.status().is_success() {
        let t = resp.text().await.unwrap_or_default();
        return Err(OAuthError::Unauthorized(t));
    }
    parse_token_response(
        resp.json()
            .await
            .map_err(|e| OAuthError::Connection(e.to_string()))?,
    )
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
        .headers(request_headers(cfg)?)
        .form(&[
            ("client_id", cfg.client_id.as_str()),
            ("scope", scope.as_str()),
        ])
        .send()
        .await
        .map_err(|e| OAuthError::Connection(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(OAuthError::Connection(
            resp.text().await.unwrap_or_default(),
        ));
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
            .headers(request_headers(cfg)?)
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
        scope: v.get("scope").and_then(|x| x.as_str()).map(str::to_string),
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

    pub fn for_key(key: &str) -> anyhow::Result<Self> {
        if key.is_empty()
            || key.starts_with('.')
            || key.contains('/')
            || key.contains('\\')
            || key.contains("..")
        {
            anyhow::bail!("invalid OAuth credential key: {key}");
        }
        Ok(Self::new(
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".kkagent")
                .join("credentials")
                .join(format!("{key}.json")),
        ))
    }

    pub fn load(&self) -> Option<TokenInfo> {
        let data = std::fs::read_to_string(&self.path).ok()?;
        serde_json::from_str(&data).ok()
    }

    pub fn save(&self, token: &TokenInfo) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
            set_private_dir_permissions(parent)?;
        }
        let tmp = self.path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let data = serde_json::to_vec_pretty(token)?;
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        use std::io::Write;
        file.write_all(&data)?;
        file.sync_all()?;
        drop(file);
        if let Err(error) = std::fs::rename(&tmp, &self.path) {
            #[cfg(windows)]
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                std::fs::remove_file(&self.path)?;
                std::fs::rename(&tmp, &self.path)?;
            } else {
                let _ = std::fs::remove_file(&tmp);
                return Err(error.into());
            }
            #[cfg(not(windows))]
            {
                let _ = std::fs::remove_file(&tmp);
                return Err(error.into());
            }
        }
        Ok(())
    }

    pub fn clear(&self) -> anyhow::Result<()> {
        let _ = std::fs::remove_file(&self.path);
        Ok(())
    }
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

pub async fn load_fresh_kimi_token(
    storage: &FileTokenStorage,
    oauth_host: Option<&str>,
) -> Result<Option<TokenInfo>, OAuthError> {
    let Some(token) = storage.load() else {
        return Ok(None);
    };
    let needs_refresh = token
        .expires_at
        .is_some_and(|expires| expires <= Utc::now() + chrono::Duration::seconds(60));
    if !needs_refresh {
        return Ok(Some(token));
    }
    let refresh = token
        .refresh_token
        .as_deref()
        .ok_or_else(|| OAuthError::Unauthorized("OAuth token has no refresh_token".into()))?;
    let refreshed = refresh_access_token(&kimi_oauth_config(oauth_host), refresh).await?;
    storage.save(&refreshed).map_err(OAuthError::Other)?;
    Ok(Some(refreshed))
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

    #[test]
    fn kimi_flow_uses_managed_endpoints() {
        let config = kimi_oauth_config(Some("https://auth.example.test/"));
        assert_eq!(config.client_id, KIMI_CODE_CLIENT_ID);
        assert_eq!(
            config.device_auth_url.as_deref(),
            Some("https://auth.example.test/api/oauth/device_authorization")
        );
        assert_eq!(
            config.token_url,
            "https://auth.example.test/api/oauth/token"
        );
    }

    #[test]
    fn token_storage_is_private_and_rejects_traversal_keys() {
        assert!(FileTokenStorage::for_key("../escape").is_err());
        let root = std::env::temp_dir().join(format!("kkagent-oauth-{}", uuid::Uuid::new_v4()));
        let path = root.join("credential.json");
        let storage = FileTokenStorage::new(&path);
        let token = TokenInfo {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            token_type: "Bearer".into(),
            expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
            scope: Some("openid".into()),
        };
        storage.save(&token).unwrap();
        assert_eq!(storage.load().unwrap().access_token, "access");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
