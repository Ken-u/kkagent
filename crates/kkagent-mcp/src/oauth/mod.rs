//! MCP OAuth credential store + authorize helpers.

mod store;

pub use store::{McpOAuthStore, OAuthTokens, StoredClientInfo};

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use url::Url;

/// Start a one-shot loopback callback server and return (redirect_uri, code_rx).
pub async fn start_oauth_callback_listener(
    preferred_port: u16,
) -> Result<(String, oneshot::Receiver<CallbackResult>)> {
    let listener = match TcpListener::bind(("127.0.0.1", preferred_port)).await {
        Ok(l) => l,
        Err(_) => TcpListener::bind("127.0.0.1:0").await?,
    };
    let addr = listener.local_addr()?;
    let redirect_uri = format!("http://127.0.0.1:{}/callback", addr.port());
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let result = parse_callback_request(&req);
            let body = if result.error.is_some() {
                "OAuth failed. You can close this window."
            } else {
                "Authorization complete. You can close this window and return to kkagent."
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = tx.send(result);
        }
    });
    Ok((redirect_uri, rx))
}

#[derive(Debug, Clone)]
pub struct CallbackResult {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

fn parse_callback_request(req: &str) -> CallbackResult {
    let first = req.lines().next().unwrap_or("");
    let path = first.split_whitespace().nth(1).unwrap_or("/");
    let url = format!("http://127.0.0.1{path}");
    let Ok(parsed) = Url::parse(&url) else {
        return CallbackResult {
            code: None,
            state: None,
            error: Some("invalid callback".into()),
        };
    };
    let pairs: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
    CallbackResult {
        code: pairs.get("code").cloned(),
        state: pairs.get("state").cloned(),
        error: pairs.get("error").cloned(),
    }
}

/// Open the system browser to an authorization URL when possible.
pub fn open_authorization_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()?;
        return Ok(());
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = url;
        Err(anyhow!("cannot open browser on this platform"))
    }
}

pub async fn exchange_code_for_tokens(
    token_url: &str,
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    redirect_uri: &str,
    code_verifier: Option<&str>,
) -> Result<OAuthTokens> {
    let client = reqwest::Client::new();
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("client_id", client_id.to_string()),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret.to_string()));
    }
    if let Some(verifier) = code_verifier {
        form.push(("code_verifier", verifier.to_string()));
    }
    let resp = client
        .post(token_url)
        .form(&form)
        .send()
        .await
        .context("token exchange request")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("token exchange failed ({status}): {body}");
    }
    let value: serde_json::Value = resp.json().await?;
    Ok(OAuthTokens {
        access_token: value
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing access_token"))?
            .to_string(),
        refresh_token: value
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(String::from),
        expires_at: value.get("expires_in").and_then(|v| v.as_u64()).map(|secs| {
            chrono::Utc::now().timestamp() + secs as i64
        }),
        token_type: value
            .get("token_type")
            .and_then(|v| v.as_str())
            .unwrap_or("Bearer")
            .to_string(),
        scope: value
            .get("scope")
            .and_then(|v| v.as_str())
            .map(String::from),
    })
}

/// Discover OAuth authorization server metadata (RFC 8414 / MCP).
pub async fn discover_auth_metadata(resource_url: &str) -> Result<AuthMetadata> {
    let client = reqwest::Client::new();
    let base = Url::parse(resource_url).context("parse resource url")?;
    let mut candidates = Vec::new();
    if let Some(host) = base.host_str() {
        let origin = format!("{}://{}", base.scheme(), host);
        let port = base
            .port()
            .map(|p| format!(":{p}"))
            .unwrap_or_default();
        let origin = format!("{origin}{port}");
        candidates.push(format!(
            "{origin}/.well-known/oauth-protected-resource"
        ));
        candidates.push(format!(
            "{origin}/.well-known/oauth-authorization-server"
        ));
    }
    // Also try path-based well-known
    candidates.push(format!(
        "{}/.well-known/oauth-authorization-server",
        resource_url.trim_end_matches('/')
    ));

    let mut auth_server: Option<String> = None;
    for url in &candidates {
        if let Ok(resp) = client.get(url).send().await {
            if resp.status().is_success() {
                if let Ok(v) = resp.json::<serde_json::Value>().await {
                    if let Some(as_url) = v
                        .get("authorization_servers")
                        .and_then(|a| a.as_array())
                        .and_then(|a| a.first())
                        .and_then(|x| x.as_str())
                    {
                        auth_server = Some(as_url.to_string());
                        break;
                    }
                    if v.get("authorization_endpoint").is_some() {
                        return Ok(AuthMetadata::from_json(v));
                    }
                }
            }
        }
    }

    if let Some(as_url) = auth_server {
        let meta_url = format!(
            "{}/.well-known/oauth-authorization-server",
            as_url.trim_end_matches('/')
        );
        let resp = client.get(&meta_url).send().await?;
        let v: serde_json::Value = resp.json().await?;
        return Ok(AuthMetadata::from_json(v));
    }

    Err(anyhow!(
        "could not discover OAuth metadata for {resource_url}"
    ))
}

#[derive(Debug, Clone)]
pub struct AuthMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub scopes_supported: Vec<String>,
}

impl AuthMetadata {
    fn from_json(v: serde_json::Value) -> Self {
        Self {
            authorization_endpoint: v
                .get("authorization_endpoint")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            token_endpoint: v
                .get("token_endpoint")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            registration_endpoint: v
                .get("registration_endpoint")
                .and_then(|x| x.as_str())
                .map(String::from),
            scopes_supported: v
                .get("scopes_supported")
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

pub async fn dynamic_client_register(
    registration_endpoint: &str,
    client_name: &str,
    redirect_uri: &str,
) -> Result<StoredClientInfo> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "client_name": client_name,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let resp = client
        .post(registration_endpoint)
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("DCR failed ({status}): {text}");
    }
    let v: serde_json::Value = resp.json().await?;
    Ok(StoredClientInfo {
        client_id: v
            .get("client_id")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("DCR missing client_id"))?
            .to_string(),
        client_secret: v
            .get("client_secret")
            .and_then(|x| x.as_str())
            .map(String::from),
        redirect_uris: vec![redirect_uri.to_string()],
    })
}

/// Build an authorization URL (PKCE S256).
pub fn build_authorize_url(
    metadata: &AuthMetadata,
    client_id: &str,
    redirect_uri: &str,
    scopes: &[String],
    state: &str,
    code_challenge: &str,
) -> Result<String> {
    let mut url = Url::parse(&metadata.authorization_endpoint)?;
    {
        let mut qp = url.query_pairs_mut();
        qp.append_pair("response_type", "code");
        qp.append_pair("client_id", client_id);
        qp.append_pair("redirect_uri", redirect_uri);
        qp.append_pair("state", state);
        qp.append_pair("code_challenge", code_challenge);
        qp.append_pair("code_challenge_method", "S256");
        if !scopes.is_empty() {
            qp.append_pair("scope", &scopes.join(" "));
        } else if !metadata.scopes_supported.is_empty() {
            qp.append_pair("scope", &metadata.scopes_supported.join(" "));
        }
    }
    Ok(url.to_string())
}

pub fn generate_pkce() -> (String, String) {
    let verifier = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    // S256 challenge via sha256 base64url — lightweight without extra dep:
    // use a deterministic fallback hash if sha2 unavailable; prefer proper when possible.
    let challenge = pkce_challenge_s256(&verifier);
    (verifier, challenge)
}

fn pkce_challenge_s256(verifier: &str) -> String {
    // Minimal SHA-256 via a tiny implementation for PKCE (avoid new workspace dep churn).
    // Prefer openssl-free pure rust: use `sha2` if present; else fall back.
    #[cfg(any())]
    {
        let _ = verifier;
    }
    // Use std-only: HMAC-like stretch is NOT S256; ship with `sha2` via rmcp already.
    use sha2_compat::sha256_base64url;
    sha256_base64url(verifier.as_bytes())
}

mod sha2_compat {
    /// Tiny SHA-256 + base64url for PKCE (no external sha2 import required).
    pub fn sha256_base64url(data: &[u8]) -> String {
        let digest = sha256(data);
        base64url(&digest)
    }

    fn base64url(data: &[u8]) -> String {
        const T: &[u8] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        let mut i = 0;
        while i + 3 <= data.len() {
            let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | data[i + 2] as u32;
            out.push(T[((n >> 18) & 63) as usize] as char);
            out.push(T[((n >> 12) & 63) as usize] as char);
            out.push(T[((n >> 6) & 63) as usize] as char);
            out.push(T[(n & 63) as usize] as char);
            i += 3;
        }
        if i < data.len() {
            let a = data[i] as u32;
            if i + 1 < data.len() {
                let b = data[i + 1] as u32;
                let n = (a << 16) | (b << 8);
                out.push(T[((n >> 18) & 63) as usize] as char);
                out.push(T[((n >> 12) & 63) as usize] as char);
                out.push(T[((n >> 6) & 63) as usize] as char);
            } else {
                let n = a << 16;
                out.push(T[((n >> 18) & 63) as usize] as char);
                out.push(T[((n >> 12) & 63) as usize] as char);
            }
        }
        out
    }

    // Compact SHA-256 (public domain style).
    pub fn sha256(mut msg: &[u8]) -> [u8; 32] {
        let mut h: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        let bit_len = (msg.len() as u64) * 8;
        let mut data = msg.to_vec();
        data.push(0x80);
        while (data.len() % 64) != 56 {
            data.push(0);
        }
        data.extend_from_slice(&bit_len.to_be_bytes());
        for chunk in data.chunks(64) {
            let mut w = [0u32; 64];
            for i in 0..16 {
                w[i] = u32::from_be_bytes([
                    chunk[i * 4],
                    chunk[i * 4 + 1],
                    chunk[i * 4 + 2],
                    chunk[i * 4 + 3],
                ]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let mut a = h[0];
            let mut b = h[1];
            let mut c = h[2];
            let mut d = h[3];
            let mut e = h[4];
            let mut f = h[5];
            let mut g = h[6];
            let mut hh = h[7];
            const K: [u32; 64] = [
                0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
                0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
                0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
                0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
                0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
                0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
                0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
                0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
                0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
                0xc67178f2,
            ];
            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ ((!e) & g);
                let t1 = hh
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                hh = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            h[0] = h[0].wrapping_add(a);
            h[1] = h[1].wrapping_add(b);
            h[2] = h[2].wrapping_add(c);
            h[3] = h[3].wrapping_add(d);
            h[4] = h[4].wrapping_add(e);
            h[5] = h[5].wrapping_add(f);
            h[6] = h[6].wrapping_add(g);
            h[7] = h[7].wrapping_add(hh);
        }
        let mut out = [0u8; 32];
        for (i, val) in h.iter().enumerate() {
            out[i * 4..(i + 1) * 4].copy_from_slice(&val.to_be_bytes());
        }
        let _ = &mut msg;
        out
    }
}

/// Full interactive OAuth login for one MCP server.
pub async fn interactive_oauth_login(
    server_name: &str,
    server_url: &str,
    store: &McpOAuthStore,
    oauth_cfg: Option<&kkagent_config::McpOAuthConfig>,
) -> Result<OAuthTokens> {
    let metadata = discover_auth_metadata(server_url).await?;
    let (redirect_uri, code_rx) = start_oauth_callback_listener(3118).await?;
    let client_label = oauth_cfg
        .and_then(|c| c.client_label.clone())
        .unwrap_or_else(|| format!("kkagent ({server_name})"));

    let client_info = if let Some(id) = oauth_cfg.and_then(|c| c.client_id.clone()) {
        StoredClientInfo {
            client_id: id,
            client_secret: oauth_cfg.and_then(|c| c.client_secret.clone()),
            redirect_uris: vec![redirect_uri.clone()],
        }
    } else if let Some(existing) = store.read_client(server_name, server_url).await? {
        existing
    } else if let Some(reg) = &metadata.registration_endpoint {
        let info = dynamic_client_register(reg, &client_label, &redirect_uri).await?;
        store.write_client(server_name, server_url, &info).await?;
        info
    } else {
        anyhow::bail!("OAuth requires client_id or dynamic registration endpoint");
    };

    let (verifier, challenge) = generate_pkce();
    let state = uuid::Uuid::new_v4().to_string();
    let scopes = oauth_cfg
        .map(|c| c.scopes.clone())
        .unwrap_or_default();
    let auth_url = build_authorize_url(
        &metadata,
        &client_info.client_id,
        &redirect_uri,
        &scopes,
        &state,
        &challenge,
    )?;

    tracing::info!("MCP OAuth: open authorization URL for {server_name}: {auth_url}");
    let _ = open_authorization_url(&auth_url);

    let callback = tokio::time::timeout(std::time::Duration::from_secs(300), code_rx)
        .await
        .context("OAuth callback timed out")?
        .context("OAuth callback closed")?;
    if let Some(err) = callback.error {
        anyhow::bail!("OAuth error: {err}");
    }
    if callback.state.as_deref() != Some(state.as_str()) {
        anyhow::bail!("OAuth state mismatch");
    }
    let code = callback.code.ok_or_else(|| anyhow!("missing code"))?;
    let tokens = exchange_code_for_tokens(
        &metadata.token_endpoint,
        &client_info.client_id,
        client_info.client_secret.as_deref(),
        &code,
        &redirect_uri,
        Some(&verifier),
    )
    .await?;
    store
        .write_tokens(server_name, server_url, &tokens)
        .await?;
    Ok(tokens)
}

/// Shared store handle type used by McpManager.
pub type SharedOAuthStore = Arc<McpOAuthStore>;
