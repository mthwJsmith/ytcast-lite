//! YouTube OAuth 2.0 TV device flow.
//!
//! Implements the same flow as YouTube.js's OAuth2 module:
//!   1. Scrape youtube.com/tv to extract client_id + client_secret
//!   2. Request a device code (user visits youtube.com/activate, enters code)
//!   3. Poll for access_token + refresh_token
//!   4. Auto-refresh access_token when expired
//!
//! Premium subscribers with a valid Bearer token get stream_protection_status=1,
//! bypassing the PoToken requirement entirely. This eliminates the need for the
//! bgutil-pot sidecar (~66MB RAM) on the Pi Zero 2 W.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Persistent OAuth credentials (saved to disk).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OAuthCredentials {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    pub access_token: String,
    /// ISO 8601 expiry time for the access token.
    pub expiry_date: String,
}

impl OAuthCredentials {
    /// Whether we have a refresh token (i.e. user has completed login).
    pub fn is_logged_in(&self) -> bool {
        !self.refresh_token.is_empty()
    }

    /// Whether the access token has expired (or will within 60s).
    pub fn is_expired(&self) -> bool {
        if self.access_token.is_empty() || self.expiry_date.is_empty() {
            return true;
        }
        let Ok(expiry) = chrono::DateTime::parse_from_rfc3339(&self.expiry_date) else {
            return true;
        };
        let now = chrono::Utc::now();
        expiry < now + chrono::Duration::seconds(60)
    }
}

/// Device code response from YouTube.
#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_url: String,
    #[allow(dead_code)]
    expires_in: u64,
    interval: u64,
    #[serde(default)]
    error_code: Option<String>,
}

/// Token response from YouTube.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    error: Option<String>,
}

/// Runtime OAuth state, shared across the application.
pub struct OAuthState {
    pub credentials: RwLock<OAuthCredentials>,
    http: reqwest::Client,
}

// ---------------------------------------------------------------------------
// YouTube OAuth endpoints
// ---------------------------------------------------------------------------

const DEVICE_CODE_URL: &str = "https://www.youtube.com/o/oauth2/device/code";
const TOKEN_URL: &str = "https://www.youtube.com/o/oauth2/token";
const TV_URL: &str = "https://www.youtube.com/tv";
const TV_USER_AGENT: &str = "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/Version";

// Grant types
const DEVICE_GRANT_TYPE: &str = "http://oauth.net/grant_type/device/1.0";
const REFRESH_GRANT_TYPE: &str = "refresh_token";

// OAuth scope — includes paid content access for Premium
const OAUTH_SCOPE: &str = "http://gdata.youtube.com https://www.googleapis.com/auth/youtube-paid-content";

// ---------------------------------------------------------------------------
// Client ID extraction
// ---------------------------------------------------------------------------

/// Scrape youtube.com/tv to extract the OAuth client_id and client_secret.
///
/// YouTube.js does this by:
/// 1. Fetching /tv to find the base.js script URL
/// 2. Fetching base.js and extracting client_id/secret via regex
async fn extract_client_credentials(
    http: &reqwest::Client,
) -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
    // Step 1: Fetch the TV page
    let tv_html = http
        .get(TV_URL)
        .header("User-Agent", TV_USER_AGENT)
        .header("Referer", "https://www.youtube.com/tv")
        .header("Accept-Language", "en-US")
        .send()
        .await?
        .text()
        .await?;

    // Step 2: Find the base.js script URL
    // Pattern: <script id="base-js" src="/s/player/.../tv-player-ias.vflset/tv-player-ias.js" ...>
    // or simply <script ... src="..." ...> where src contains the base JS
    let base_js_url = extract_base_js_url(&tv_html)?;

    // Make absolute URL if relative
    let full_url = if base_js_url.starts_with("http") {
        base_js_url
    } else {
        format!("https://www.youtube.com{}", base_js_url)
    };

    tracing::info!("[oauth] fetching base.js from {}", full_url);

    // Step 3: Fetch base.js
    let base_js = http
        .get(&full_url)
        .header("User-Agent", TV_USER_AGENT)
        .header("Referer", "https://www.youtube.com/tv")
        .send()
        .await?
        .text()
        .await?;

    // Step 4: Extract client_id and client_secret
    // YouTube.js regex: clientId:"(?<client_id>[^"]+)",[^"]*?:"(?<client_secret>[^"]+)"
    extract_credentials_from_js(&base_js)
}

fn extract_base_js_url(html: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    // Look for: <script id="base-js" src="..." ...>
    // or: <script ... src="...base.js..." ...>
    // YouTube.js pattern: <script\s+id="base-js"\s+src="([^"]+)"
    for line in html.lines() {
        if line.contains("id=\"base-js\"") || line.contains("id='base-js'") {
            if let Some(start) = line.find("src=\"") {
                let rest = &line[start + 5..];
                if let Some(end) = rest.find('"') {
                    return Ok(rest[..end].to_string());
                }
            }
            if let Some(start) = line.find("src='") {
                let rest = &line[start + 5..];
                if let Some(end) = rest.find('\'') {
                    return Ok(rest[..end].to_string());
                }
            }
        }
    }

    // Fallback: look for any script src that looks like a player JS
    for line in html.lines() {
        if let Some(start) = line.find("src=\"") {
            let rest = &line[start + 5..];
            if let Some(end) = rest.find('"') {
                let url = &rest[..end];
                if url.contains("player") && url.ends_with(".js") {
                    return Ok(url.to_string());
                }
            }
        }
    }

    Err("could not find base.js URL in /tv page".into())
}

fn extract_credentials_from_js(
    js: &str,
) -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
    // YouTube.js regex: clientId:"(?<client_id>[^"]+)",[^"]*?:"(?<client_secret>[^"]+)"
    // We search for: clientId:"..." followed shortly by :"..."
    if let Some(cid_pos) = js.find("clientId:\"") {
        let rest = &js[cid_pos + 10..]; // skip 'clientId:"'
        if let Some(cid_end) = rest.find('"') {
            let client_id = rest[..cid_end].to_string();

            // Now find the client_secret — it's the next quoted value after a ":"
            let after_cid = &rest[cid_end + 1..];
            // Look for :"..." pattern within the next ~200 chars
            let search_range = &after_cid[..after_cid.len().min(200)];
            if let Some(sec_pos) = search_range.find(":\"") {
                let sec_rest = &search_range[sec_pos + 2..];
                if let Some(sec_end) = sec_rest.find('"') {
                    let client_secret = sec_rest[..sec_end].to_string();

                    if !client_id.is_empty() && !client_secret.is_empty() {
                        tracing::info!(
                            "[oauth] extracted client_id={}, secret={}...",
                            &client_id[..client_id.len().min(20)],
                            &client_secret[..client_secret.len().min(10)],
                        );
                        return Ok((client_id, client_secret));
                    }
                }
            }
        }
    }

    Err("could not extract client_id/client_secret from base.js".into())
}

// ---------------------------------------------------------------------------
// Visitor data generation
// ---------------------------------------------------------------------------

/// Generate a visitor_data protobuf (same as YouTube.js ProtoUtils).
///
/// Structure: VisitorData { id: random 11-char string, timestamp: unix seconds }
/// Encoded as protobuf, then base64url.
pub fn generate_visitor_data() -> String {
    use rand::Rng;

    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();

    // Random 11-character ID
    let id: String = (0..11)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect();

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Manual protobuf encoding (field 1 = string, field 2 = int64)
    // This avoids needing a .proto file for this simple message.
    let mut buf = Vec::new();

    // Field 1 (string): wire type 2 (length-delimited), field number 1
    // Tag = (1 << 3) | 2 = 0x0A
    buf.push(0x0A);
    buf.push(id.len() as u8);
    buf.extend_from_slice(id.as_bytes());

    // Field 2 (int64): wire type 0 (varint), field number 2
    // Tag = (2 << 3) | 0 = 0x10
    buf.push(0x10);
    encode_varint(&mut buf, timestamp);

    // Base64url encode (YouTube uses URL-safe with no padding)
    use base64::Engine;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&buf);

    // URL-encode (YouTube.js does encodeURIComponent)
    encoded
}

fn encode_varint(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            buf.push(byte);
            break;
        } else {
            buf.push(byte | 0x80);
        }
    }
}

// ---------------------------------------------------------------------------
// OAuth flow
// ---------------------------------------------------------------------------

impl OAuthState {
    pub fn new(http: reqwest::Client, credentials: OAuthCredentials) -> Arc<Self> {
        Arc::new(Self {
            credentials: RwLock::new(credentials),
            http,
        })
    }

    /// Get a valid access token, refreshing if expired.
    /// Returns None if not logged in.
    pub async fn get_access_token(&self) -> Option<String> {
        {
            let creds = self.credentials.read().await;
            if !creds.is_logged_in() {
                return None;
            }
            if !creds.is_expired() {
                return Some(creds.access_token.clone());
            }
        }

        // Token expired — refresh it
        match self.refresh_access_token().await {
            Ok(()) => {
                let creds = self.credentials.read().await;
                Some(creds.access_token.clone())
            }
            Err(e) => {
                tracing::error!("[oauth] token refresh failed: {}", e);
                None
            }
        }
    }

    /// Run the full device login flow. Blocks until user completes auth.
    ///
    /// Prints instructions to stdout for the user.
    pub async fn device_login(
        &self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Step 1: Get or extract client credentials
        let (client_id, client_secret) = {
            let creds = self.credentials.read().await;
            if !creds.client_id.is_empty() && !creds.client_secret.is_empty() {
                (creds.client_id.clone(), creds.client_secret.clone())
            } else {
                drop(creds);
                tracing::info!("[oauth] extracting client credentials from youtube.com/tv");
                let result = extract_client_credentials(&self.http).await?;
                let mut creds = self.credentials.write().await;
                creds.client_id = result.0.clone();
                creds.client_secret = result.1.clone();
                result
            }
        };

        // Step 2: Request device code
        let device_code_resp: DeviceCodeResponse = self
            .http
            .post(DEVICE_CODE_URL)
            .json(&serde_json::json!({
                "client_id": client_id,
                "scope": OAUTH_SCOPE,
                "device_id": uuid::Uuid::new_v4().to_string(),
                "device_model": "ytlr::",
            }))
            .send()
            .await?
            .json()
            .await?;

        if let Some(ref err) = device_code_resp.error_code {
            return Err(format!("device code error: {}", err).into());
        }

        // Step 3: Show user instructions
        tracing::info!(
            "[oauth] Go to {} and enter code: {}",
            device_code_resp.verification_url,
            device_code_resp.user_code,
        );
        // Also print to stdout so it's visible even without RUST_LOG
        println!();
        println!("=== YouTube Login ===");
        println!("Go to: {}", device_code_resp.verification_url);
        println!("Enter code: {}", device_code_resp.user_code);
        println!("Waiting for authorization...");
        println!();

        // Step 4: Poll for tokens
        let poll_interval = std::time::Duration::from_secs(device_code_resp.interval.max(5));

        loop {
            tokio::time::sleep(poll_interval).await;

            let resp: TokenResponse = self
                .http
                .post(TOKEN_URL)
                .json(&serde_json::json!({
                    "client_id": client_id,
                    "client_secret": client_secret,
                    "code": device_code_resp.device_code,
                    "grant_type": DEVICE_GRANT_TYPE,
                }))
                .send()
                .await?
                .json()
                .await?;

            if let Some(ref err) = resp.error {
                match err.as_str() {
                    "authorization_pending" => {
                        tracing::debug!("[oauth] waiting for user authorization...");
                        continue;
                    }
                    "slow_down" => {
                        tracing::debug!("[oauth] rate limited, slowing down");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        continue;
                    }
                    "access_denied" => {
                        return Err("user denied authorization".into());
                    }
                    "expired_token" => {
                        return Err("device code expired — please try again".into());
                    }
                    other => {
                        return Err(format!("OAuth error: {}", other).into());
                    }
                }
            }

            // Success — we have tokens
            if let (Some(access_token), Some(refresh_token)) =
                (resp.access_token, resp.refresh_token)
            {
                let expiry = chrono::Utc::now()
                    + chrono::Duration::seconds(resp.expires_in.unwrap_or(3600) as i64);

                let mut creds = self.credentials.write().await;
                creds.access_token = access_token;
                creds.refresh_token = refresh_token;
                creds.expiry_date = expiry.to_rfc3339();

                tracing::info!("[oauth] login successful! Token expires at {}", creds.expiry_date);
                println!("Login successful!");
                return Ok(());
            }
        }
    }

    /// Refresh the access token using the stored refresh token.
    async fn refresh_access_token(
        &self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (client_id, client_secret, refresh_token) = {
            let creds = self.credentials.read().await;
            (
                creds.client_id.clone(),
                creds.client_secret.clone(),
                creds.refresh_token.clone(),
            )
        };

        tracing::info!("[oauth] refreshing access token...");

        let resp: TokenResponse = self
            .http
            .post(TOKEN_URL)
            .json(&serde_json::json!({
                "client_id": client_id,
                "client_secret": client_secret,
                "refresh_token": refresh_token,
                "grant_type": REFRESH_GRANT_TYPE,
            }))
            .send()
            .await?
            .json()
            .await?;

        if let Some(ref err) = resp.error {
            return Err(format!("token refresh failed: {}", err).into());
        }

        if let Some(access_token) = resp.access_token {
            let expiry = chrono::Utc::now()
                + chrono::Duration::seconds(resp.expires_in.unwrap_or(3600) as i64);

            let mut creds = self.credentials.write().await;
            creds.access_token = access_token;
            creds.expiry_date = expiry.to_rfc3339();

            tracing::info!("[oauth] token refreshed, expires at {}", creds.expiry_date);
            Ok(())
        } else {
            Err("refresh response missing access_token".into())
        }
    }
}

// ---------------------------------------------------------------------------
// Config persistence
// ---------------------------------------------------------------------------

/// Path for OAuth credentials: ~/.config/ytcast-oauth.json
pub fn credentials_path() -> Result<std::path::PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let config_dir = dirs::config_dir().ok_or("could not determine config directory")?;
    Ok(config_dir.join("ytcast-oauth.json"))
}

/// Load OAuth credentials from disk.
pub fn load_credentials() -> OAuthCredentials {
    let path = match credentials_path() {
        Ok(p) => p,
        Err(_) => return OAuthCredentials::default(),
    };

    match std::fs::read_to_string(&path) {
        Ok(data) => match serde_json::from_str(&data) {
            Ok(creds) => {
                tracing::info!("[oauth] loaded credentials from {}", path.display());
                creds
            }
            Err(e) => {
                tracing::warn!("[oauth] failed to parse {}: {}", path.display(), e);
                OAuthCredentials::default()
            }
        },
        Err(_) => {
            tracing::info!("[oauth] no credentials at {}", path.display());
            OAuthCredentials::default()
        }
    }
}

/// Save OAuth credentials to disk (atomic write).
pub fn save_credentials(
    creds: &OAuthCredentials,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = credentials_path()?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_string_pretty(creds)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &path)?;
    tracing::info!("[oauth] saved credentials to {}", path.display());
    Ok(())
}
