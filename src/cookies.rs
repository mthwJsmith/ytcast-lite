//! YouTube cookie loading and YouTube Music session extraction.
//!
//! Loads cookies from a Netscape-format cookies.txt file, then fetches
//! music.youtube.com to extract the live INNERTUBE_CONTEXT (ytcfg) needed
//! for authenticated WEB_REMIX /player calls.
//!
//! yt-dlp does the same thing: it downloads the page first to get the full
//! client context (22+ fields), visitor_data, DATASYNC_ID, etc. Without this,
//! WEB_REMIX /player returns UNPLAYABLE.

/// Parsed YouTube cookies: raw Cookie header + auth components.
pub struct YtCookies {
    /// Raw "name=value; name=value; ..." header string for youtube.com.
    pub cookie_header: String,
    /// SAPISID value (from SAPISID or __Secure-3PAPISID cookie).
    pub sapisid: String,
    /// __Secure-1PAPISID value (for SAPISID1PHASH).
    pub sapisid_1p: Option<String>,
    /// __Secure-3PAPISID value (for SAPISID3PHASH).
    pub sapisid_3p: Option<String>,
}

/// Live session data extracted from music.youtube.com page.
/// Contains the full INNERTUBE_CONTEXT and auth metadata that yt-dlp
/// uses for WEB_REMIX /player calls.
pub struct YtMusicSession {
    /// Full INNERTUBE_CONTEXT from ytcfg (22+ client fields).
    pub innertube_context: serde_json::Value,
    /// DATASYNC_ID user part (goes in X-Goog-Pageid header + SAPISIDHASH hash).
    pub user_session_id: Option<String>,
    /// SESSION_INDEX (goes in X-Goog-Authuser header).
    pub session_index: Option<String>,
    /// Whether the user is logged in.
    pub logged_in: bool,
    /// Signature timestamp from ytcfg STS field (goes in playbackContext).
    pub signature_timestamp: Option<u64>,
    /// Raw base.js player JavaScript (cached for nsig solving).
    pub player_js: Option<String>,
    /// Player hash (base.js version identifier, e.g. "4e67f8a0").
    pub player_hash: Option<String>,
}

/// Load cookies from a Netscape-format cookies.txt file.
pub fn load_cookie_file(
    path: &str,
) -> Result<Option<YtCookies>, Box<dyn std::error::Error + Send + Sync>> {
    let content = std::fs::read_to_string(path)?;
    let mut cookies = Vec::new();
    let mut sapisid: Option<String> = None;
    let mut sapisid_1p: Option<String> = None;
    let mut sapisid_3p: Option<String> = None;

    for line in content.lines() {
        let line = line.trim();

        let line = if let Some(stripped) = line.strip_prefix("#HttpOnly_") {
            stripped
        } else {
            line
        };

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 7 {
            continue;
        }

        let name = parts[5];
        let value = parts[6];

        cookies.push(format!("{}={}", name, value));

        match name {
            "SAPISID" => {
                if sapisid.is_none() {
                    sapisid = Some(value.split('/').next().unwrap_or(value).to_string());
                }
            }
            "__Secure-1PAPISID" => {
                sapisid_1p = Some(value.split('/').next().unwrap_or(value).to_string());
            }
            "__Secure-3PAPISID" => {
                let val = value.split('/').next().unwrap_or(value).to_string();
                sapisid_3p = Some(val.clone());
                if sapisid.is_none() {
                    sapisid = Some(val);
                }
            }
            _ => {}
        }
    }

    if let Some(sapisid) = sapisid {
        tracing::info!("[cookies] loaded {} cookies, SAPISID=true, 1P={}, 3P={}",
            cookies.len(), sapisid_1p.is_some(), sapisid_3p.is_some());
        Ok(Some(YtCookies {
            cookie_header: cookies.join("; "),
            sapisid,
            sapisid_1p,
            sapisid_3p,
        }))
    } else {
        tracing::warn!("[cookies] loaded {} cookies but no SAPISID found", cookies.len());
        Ok(None)
    }
}

/// Fetch music.youtube.com and extract the live INNERTUBE_CONTEXT from ytcfg.
///
/// This replicates what yt-dlp does: download the page HTML, find
/// `ytcfg.set({...})` blocks, extract INNERTUBE_CONTEXT, DATASYNC_ID, etc.
/// Without this, WEB_REMIX /player returns UNPLAYABLE.
pub async fn fetch_music_session(
    http: &reqwest::Client,
    cookies: &YtCookies,
) -> Option<YtMusicSession> {
    let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
              (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

    let resp = http
        .get("https://music.youtube.com/")
        .header("User-Agent", ua)
        .header("Cookie", &cookies.cookie_header)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        tracing::warn!("[ytcfg] music.youtube.com returned HTTP {}", resp.status());
        return None;
    }

    let page = resp.text().await.ok()?;
    if page.len() < 10_000 {
        tracing::warn!("[ytcfg] page too small ({}B) — probably 'browser deprecated' page", page.len());
        return None;
    }

    // Extract ytcfg values using regex (page is minified JS, not parseable HTML)
    let innertube_context = extract_json_field(&page, "INNERTUBE_CONTEXT");
    let datasync_id = extract_string_field(&page, "DATASYNC_ID");
    let session_index = extract_string_field(&page, "SESSION_INDEX");
    let mut signature_timestamp = extract_number_field(&page, "STS");
    let logged_in = page.contains("\"LOGGED_IN\":true") || page.contains("\"LOGGED_IN\": true");

    // STS is usually NOT in the page ytcfg — it's in the player JavaScript (base.js).
    // Extract the player JS URL from the page and fetch STS from it.
    // Also cache the base.js content for nsig solving.
    let mut player_js: Option<String> = None;
    let mut player_hash: Option<String> = None;
    if signature_timestamp.is_none() {
        if let Some((sts, js, hash)) = fetch_sts_and_player_js(http, &page, ua).await {
            signature_timestamp = Some(sts);
            player_js = Some(js);
            player_hash = Some(hash);
        }
    }

    let ctx = match innertube_context {
        Some(mut ctx) => {
            // Add yt-dlp's standard additions
            if let Some(client) = ctx.get_mut("client").and_then(|c| c.as_object_mut()) {
                client.entry("hl").or_insert(serde_json::json!("en"));
                client.entry("timeZone").or_insert(serde_json::json!("UTC"));
                client.entry("utcOffsetMinutes").or_insert(serde_json::json!(0));
            }
            // Add user + request context like yt-dlp
            if let Some(obj) = ctx.as_object_mut() {
                obj.entry("user").or_insert(serde_json::json!({"lockedSafetyMode": false}));
                obj.entry("request").or_insert(serde_json::json!({"useSsl": true}));
            }
            ctx
        }
        None => {
            tracing::warn!("[ytcfg] could not extract INNERTUBE_CONTEXT from page");
            return None;
        }
    };

    let user_session_id = datasync_id.as_ref().map(|d| {
        d.split("||").next().unwrap_or(d).to_string()
    });

    let client_version = ctx.get("client")
        .and_then(|c| c.get("clientVersion"))
        .and_then(|v| v.as_str())
        .unwrap_or("?");

    tracing::info!(
        "[ytcfg] session extracted: clientVersion={}, visitorData={}, logged_in={}, dsid={}, sts={:?}",
        client_version,
        ctx.get("client").and_then(|c| c.get("visitorData")).is_some(),
        logged_in,
        user_session_id.as_deref().unwrap_or("none"),
        signature_timestamp,
    );

    Some(YtMusicSession {
        innertube_context: ctx,
        user_session_id,
        session_index,
        logged_in,
        signature_timestamp,
        player_js,
        player_hash,
    })
}

/// Generate the full Authorization header with triple SAPISIDHASH like yt-dlp.
///
/// yt-dlp sends: `SAPISIDHASH ts_hash_u SAPISID1PHASH ts_hash_u SAPISID3PHASH ts_hash_u`
/// where the hash includes user_session_id when available.
pub fn full_sapisidhash(cookies: &YtCookies, origin: &str, user_session_id: Option<&str>) -> String {
    use sha1::{Digest, Sha1};
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let mut parts = Vec::new();

    let compute = |scheme: &str, sid: &str| -> String {
        let (input, suffix) = if let Some(usid) = user_session_id {
            (format!("{} {} {} {}", usid, timestamp, sid, origin), "_u")
        } else {
            (format!("{} {} {}", timestamp, sid, origin), "")
        };
        let hash = Sha1::digest(input.as_bytes());
        format!("{} {}_{:x}{}", scheme, timestamp, hash, suffix)
    };

    parts.push(compute("SAPISIDHASH", &cookies.sapisid));
    if let Some(ref s1p) = cookies.sapisid_1p {
        parts.push(compute("SAPISID1PHASH", s1p));
    }
    if let Some(ref s3p) = cookies.sapisid_3p {
        parts.push(compute("SAPISID3PHASH", s3p));
    }

    parts.join(" ")
}

/// Fetch STS (signatureTimestamp) and raw JS from YouTube's player JavaScript (base.js).
///
/// The page HTML contains a reference to `/s/player/{hash}/...base.js`.
/// We download that JS, extract the STS value, and return the full JS content
/// so it can be cached for nsig solving via ytdlp-ejs.
/// Returns (sts, player_js, player_hash).
async fn fetch_sts_and_player_js(http: &reqwest::Client, page: &str, ua: &str) -> Option<(u64, String, String)> {
    // Find player hash from page: /s/player/HASH/ pattern
    let player_hash = {
        let mut found = None;
        for part in page.split("/s/player/") {
            let hash: String = part.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
            if hash.len() >= 6 {
                found = Some(hash);
                break;
            }
        }
        found
    };

    let hash = match player_hash {
        Some(h) => h,
        None => {
            tracing::warn!("[ytcfg] could not find player hash in page HTML");
            return None;
        }
    };

    let base_js_url = format!(
        "https://www.youtube.com/s/player/{}/player_ias.vflset/en_US/base.js",
        hash
    );
    tracing::info!("[ytcfg] fetching base.js from player hash={}", hash);

    let resp = http
        .get(&base_js_url)
        .header("User-Agent", ua)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        tracing::warn!("[ytcfg] base.js returned HTTP {}", resp.status());
        return None;
    }

    let js = resp.text().await.ok()?;
    tracing::info!("[ytcfg] base.js size: {}B", js.len());

    // Search for signatureTimestamp:NNNNN
    for part in js.split("signatureTimestamp") {
        let trimmed = part.trim_start();
        if let Some(rest) = trimmed.strip_prefix(':') {
            let rest = rest.trim_start();
            let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if num.len() == 5 {
                if let Ok(val) = num.parse::<u64>() {
                    tracing::info!("[ytcfg] extracted STS={} from base.js", val);
                    return Some((val, js, hash.clone()));
                }
            }
        }
    }

    tracing::warn!("[ytcfg] could not find signatureTimestamp in base.js");
    None
}

// --- Internal helpers ---

/// Extract a JSON object field from ytcfg in the page HTML.
/// Looks for `"FIELD_NAME":{...}` patterns in ytcfg.set() blocks.
fn extract_json_field(page: &str, field: &str) -> Option<serde_json::Value> {
    // Look for ytcfg.set({...}) blocks containing the field
    let pattern = format!("\"{}\"", field);
    for chunk in page.split("ytcfg.set(") {
        if !chunk.contains(&pattern) {
            continue;
        }
        // Find the field start
        if let Some(field_pos) = chunk.find(&pattern) {
            let after_key = &chunk[field_pos + pattern.len()..];
            // Skip ":" and whitespace
            let after_colon = after_key.trim_start().strip_prefix(':')?;
            let after_colon = after_colon.trim_start();
            // Parse the JSON value starting here
            if after_colon.starts_with('{') {
                // Find matching brace
                if let Some(json_str) = find_matching_brace(after_colon) {
                    if let Ok(val) = serde_json::from_str(json_str) {
                        return Some(val);
                    }
                }
            }
        }
    }
    None
}

/// Extract a string field from ytcfg: `"FIELD":"value"`.
fn extract_string_field(page: &str, field: &str) -> Option<String> {
    let pattern = format!("\"{}\"", field);
    for chunk in page.split(&pattern) {
        // chunk starts right after the field name
        let rest = chunk.trim_start();
        if let Some(rest) = rest.strip_prefix(':') {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('"') {
                if let Some(end) = rest.find('"') {
                    return Some(rest[..end].to_string());
                }
            }
        }
    }
    None
}

/// Extract a numeric field from ytcfg: `"FIELD":12345`.
fn extract_number_field(page: &str, field: &str) -> Option<u64> {
    let pattern = format!("\"{}\"", field);
    for chunk in page.split(&pattern) {
        let rest = chunk.trim_start();
        if let Some(rest) = rest.strip_prefix(':') {
            let rest = rest.trim_start();
            let num_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !num_str.is_empty() {
                return num_str.parse().ok();
            }
        }
    }
    None
}

/// Find the substring containing a balanced JSON object starting with `{`.
fn find_matching_brace(s: &str) -> Option<&str> {
    if !s.starts_with('{') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, ch) in s.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' if in_string => escape = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}
