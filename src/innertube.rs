use serde_json::{json, Value};

const INNERTUBE_URL: &str = "https://www.youtube.com/youtubei/v1/player";

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

// ---------------------------------------------------------------------------
// Client identities — tried in order until one returns a playable stream.
//
// IOS: Direct URLs (no sig decryption), no PO token required (for now).
//   Best coverage in testing: 20/20 videos OK.
//
// ANDROID: Same direct URLs, same coverage. Backup if IOS gets blocked.
//
// ANDROID_VR: Was the original client. YouTube started returning
//   LOGIN_REQUIRED ("Sign in to confirm you're not a bot") on ~70% of
//   videos as of 25 Feb 2026. Kept as fallback.
//
// TVHTML5_SIMPLY: Lightweight TV client. No PO token, no SABR, returns
//   direct URLs. Untested coverage — added as extra fallback before giving up.
// ---------------------------------------------------------------------------

struct ClientIdentity {
    name: &'static str,
    context: Value,
    user_agent: &'static str,
}

fn ios_client(video_id: &str, ctt: Option<&str>, playlist_id: Option<&str>) -> ClientIdentity {
    let mut context = json!({
        "client": {
            "clientName": "IOS",
            "clientVersion": "21.02.3",
            "deviceMake": "Apple",
            "deviceModel": "iPhone16,2",
            "osName": "iPhone",
            "osVersion": "18.3.2.22D82"
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
        name: "IOS",
        context: payload,
        user_agent: "com.google.ios.youtube/21.02.3 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)",
    }
}

fn android_client(video_id: &str, ctt: Option<&str>, playlist_id: Option<&str>) -> ClientIdentity {
    let mut context = json!({
        "client": {
            "clientName": "ANDROID",
            "clientVersion": "21.02.35",
            "androidSdkVersion": 30,
            "osName": "Android",
            "osVersion": "11"
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
        name: "ANDROID",
        context: payload,
        user_agent: "com.google.android.youtube/21.02.35 (Linux; U; Android 11) gzip",
    }
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
        context: payload,
        user_agent: "com.google.android.apps.youtube.vr.oculus/1.71.26 \
            (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip",
    }
}

fn tv_simply_client(video_id: &str, ctt: Option<&str>, playlist_id: Option<&str>) -> ClientIdentity {
    let mut context = json!({
        "client": {
            "clientName": "TVHTML5_SIMPLY",
            "clientVersion": "2.0",
            "deviceMake": "",
            "deviceModel": ""
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
        name: "TVHTML5_SIMPLY",
        context: payload,
        user_agent: "Mozilla/5.0 (SMART-TV; LINUX; Tizen 6.5)",
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

/// Resolve a playable audio stream URL via InnerTube.
///
/// Tries IOS → ANDROID → ANDROID_VR → TVHTML5_SIMPLY in order. All return
/// direct URLs (no signature decryption needed). Returns `Ok(None)` when
/// no client can provide a playable stream.
pub async fn resolve_stream(
    client: &reqwest::Client,
    video_id: &str,
    ctt: Option<&str>,
    playlist_id: Option<&str>,
) -> Result<Option<StreamInfo>> {
    let clients = [
        ios_client(video_id, ctt, playlist_id),
        android_client(video_id, ctt, playlist_id),
        android_vr_client(video_id, ctt, playlist_id),
        tv_simply_client(video_id, ctt, playlist_id),
    ];

    for identity in &clients {
        match try_client(client, video_id, identity).await? {
            Some(info) => return Ok(Some(info)),
            None => continue,
        }
    }

    tracing::error!("[stream] all clients failed for {video_id}");
    Ok(None)
}

/// Try a single InnerTube client identity.
async fn try_client(
    client: &reqwest::Client,
    video_id: &str,
    identity: &ClientIdentity,
) -> Result<Option<StreamInfo>> {
    let res = client
        .post(INNERTUBE_URL)
        .header("User-Agent", identity.user_agent)
        .json(&identity.context)
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

    if let Some(url) = best_audio_url(&data["streamingData"]["adaptiveFormats"]) {
        tracing::info!("[stream] resolved {video_id} via {}", identity.name);
        return Ok(Some(StreamInfo { title, duration, stream_url: url }));
    }

    if let Some(url) = first_playable_url(&data["streamingData"]["formats"]) {
        tracing::info!("[stream] resolved {video_id} (progressive) via {}", identity.name);
        return Ok(Some(StreamInfo { title, duration, stream_url: url }));
    }

    tracing::warn!("[stream] {} OK but no direct URLs for {}", identity.name, video_id);
    Ok(None)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// From adaptive formats, pick the audio stream with the highest bitrate
/// that has a direct `url` (not signature-ciphered).
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
