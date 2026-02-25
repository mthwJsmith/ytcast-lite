use serde_json::{json, Value};

const INNERTUBE_URL: &str = "https://www.youtube.com/youtubei/v1/player";

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

// ---------------------------------------------------------------------------
// Client identities — tried in order until one returns a playable stream.
//
// ANDROID_VR: Direct URLs (no signature decryption), no tokens needed.
//   Fails on: "made for kids" content, some A/B bot detection.
//
// WEB_EMBEDDED_PLAYER: Handles kids content + most restrictions.
//   Returns direct URLs for many videos (no JS needed).
//   Some videos may only have signatureCipher (we skip those).
// ---------------------------------------------------------------------------

struct ClientIdentity {
    name: &'static str,
    payload: Value,
    user_agent: &'static str,
}

fn android_vr_client(video_id: &str, ctt: Option<&str>, playlist_id: Option<&str>) -> ClientIdentity {
    let mut context = json!({
        "client": {
            "clientName": "ANDROID_VR",
            "clientVersion": "1.71.26",
            "androidSdkVersion": 32,
            "deviceMake": "Oculus",
            "deviceModel": "Quest 3"
        }
    });

    if let Some(token) = ctt {
        context["user"] = json!({
            "enableSafetyMode": false,
            "lockedSafetyMode": false,
            "credentialTransferTokens": [{
                "scope": "VIDEO",
                "token": token
            }]
        });
    }

    let mut payload = json!({
        "context": context,
        "videoId": video_id
    });

    if let Some(pid) = playlist_id {
        payload["playlistId"] = json!(pid);
    }

    ClientIdentity {
        name: "ANDROID_VR",
        payload,
        user_agent: "com.google.android.apps.youtube.vr.oculus/1.71.26 \
            (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip",
    }
}

fn web_embedded_client(video_id: &str, playlist_id: Option<&str>) -> ClientIdentity {
    let mut payload = json!({
        "context": {
            "client": {
                "clientName": "WEB_EMBEDDED_PLAYER",
                "clientVersion": "1.20260115.01.00"
            },
            "thirdParty": {
                "embedUrl": "https://www.youtube.com/"
            }
        },
        "videoId": video_id
    });

    if let Some(pid) = playlist_id {
        payload["playlistId"] = json!(pid);
    }

    ClientIdentity {
        name: "WEB_EMBEDDED",
        payload,
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
            (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    }
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub title: String,
    pub duration: u64,
    pub stream_url: String,
}

// ---------------------------------------------------------------------------
// Resolver
// ---------------------------------------------------------------------------

/// Resolve a playable audio stream URL for a YouTube video.
///
/// Tries ANDROID_VR first (direct URLs, no tokens). Falls back to
/// WEB_EMBEDDED_PLAYER for videos that ANDROID_VR can't handle
/// (made-for-kids, some bot detection).
///
/// Returns `Ok(None)` when no client can provide a playable stream.
pub async fn resolve_stream(
    client: &reqwest::Client,
    video_id: &str,
    ctt: Option<&str>,
    playlist_id: Option<&str>,
) -> Result<Option<StreamInfo>> {
    let clients = [
        android_vr_client(video_id, ctt, playlist_id),
        web_embedded_client(video_id, playlist_id),
    ];

    for identity in &clients {
        match try_client(client, video_id, identity).await? {
            Some(info) => return Ok(Some(info)),
            None => continue,
        }
    }

    Ok(None)
}

/// Try a single InnerTube client identity. Returns None if unplayable or
/// no direct audio URL available.
async fn try_client(
    client: &reqwest::Client,
    video_id: &str,
    identity: &ClientIdentity,
) -> Result<Option<StreamInfo>> {
    let res = client
        .post(INNERTUBE_URL)
        .header("User-Agent", identity.user_agent)
        .json(&identity.payload)
        .send()
        .await?;

    if !res.status().is_success() {
        tracing::warn!("[stream] {} {} for {}", identity.name, res.status(), video_id);
        return Ok(None);
    }

    let data: Value = res.json().await?;

    let status = data["playabilityStatus"]["status"].as_str().unwrap_or("");
    if status != "OK" {
        let reason = data["playabilityStatus"]["reason"]
            .as_str()
            .unwrap_or("unknown");
        tracing::warn!("[stream] {} {}: {} -- {}", identity.name, video_id, status, reason);
        return Ok(None);
    }

    let title = data["videoDetails"]["title"]
        .as_str()
        .unwrap_or(video_id)
        .to_owned();

    let duration: u64 = data["videoDetails"]["lengthSeconds"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    // Pick highest-bitrate audio stream with a direct URL
    if let Some(url) = best_audio_url(&data["streamingData"]["adaptiveFormats"]) {
        tracing::info!("[stream] {} resolved {} via {}", identity.name, video_id, identity.name);
        return Ok(Some(StreamInfo {
            title,
            duration,
            stream_url: url,
        }));
    }

    // Fallback: progressive formats
    if let Some(url) = first_playable_url(&data["streamingData"]["formats"]) {
        tracing::info!("[stream] {} resolved {} (progressive) via {}", identity.name, video_id, identity.name);
        return Ok(Some(StreamInfo {
            title,
            duration,
            stream_url: url,
        }));
    }

    tracing::warn!("[stream] {} returned OK but no direct URLs for {}", identity.name, video_id);
    Ok(None)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// From adaptive formats, pick the audio stream with the highest bitrate that
/// has a direct `url` (i.e. not signature-ciphered).
fn best_audio_url(formats: &Value) -> Option<String> {
    let arr = formats.as_array()?;

    let mut audio_formats: Vec<&Value> = arr
        .iter()
        .filter(|f| {
            f["mimeType"]
                .as_str()
                .is_some_and(|m| m.starts_with("audio/"))
                && f["url"].is_string()
        })
        .collect();

    // Sort by bitrate descending (highest first)
    audio_formats.sort_by(|a, b| {
        let br_a = a["bitrate"].as_u64().unwrap_or(0);
        let br_b = b["bitrate"].as_u64().unwrap_or(0);
        br_b.cmp(&br_a)
    });

    audio_formats
        .first()
        .and_then(|f| f["url"].as_str())
        .map(String::from)
}

/// From progressive formats, return the first one with a direct URL.
fn first_playable_url(formats: &Value) -> Option<String> {
    formats
        .as_array()?
        .iter()
        .find(|f| f["url"].is_string())
        .and_then(|f| f["url"].as_str())
        .map(String::from)
}
