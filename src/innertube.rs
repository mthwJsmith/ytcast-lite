pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub title: String,
    pub duration: u64,
    pub stream_url: String,
}

/// Resolve a video ID to a playable audio stream URL.
///
/// Makes a lightweight InnerTube /player call to get the real title and
/// duration (needed for YouTube seekbar sync), then returns a localhost URL
/// that the DIAL server will handle via SABR when MPD connects.
pub async fn resolve_stream(
    client: &reqwest::Client,
    video_id: &str,
    _ctt: Option<&str>,
    _playlist_id: Option<&str>,
) -> Result<Option<StreamInfo>> {
    let stream_url = format!("http://localhost:8008/stream/{}", video_id);

    // Quick InnerTube /player call for metadata only (title + duration).
    // The SABR handler will make its own /player call for the streaming URL.
    let body = serde_json::json!({
        "videoId": video_id,
        "context": {
            "client": {
                "clientName": "IOS",
                "clientVersion": "19.45.4",
                "deviceMake": "Apple",
                "deviceModel": "iPhone16,2",
                "osName": "iPhone",
                "osVersion": "18.1.0.22B83",
                "hl": "en",
                "gl": "US",
            }
        },
        "contentCheckOk": true,
        "racyCheckOk": true,
    });

    let resp = client
        .post("https://www.youtube.com/youtubei/v1/player?prettyPrint=false")
        .header(
            "User-Agent",
            "com.google.ios.youtube/19.45.4 (iPhone16,2; U; CPU iOS 18_1_0 like Mac OS X;)",
        )
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await;

    let (title, duration) = match resp {
        Ok(r) if r.status().is_success() => {
            let json: serde_json::Value = r.json().await.unwrap_or_default();
            let status = json["playabilityStatus"]["status"]
                .as_str()
                .unwrap_or("UNKNOWN");
            if status != "OK" {
                let reason = json["playabilityStatus"]["reason"]
                    .as_str()
                    .unwrap_or("unknown");
                tracing::warn!("[innertube] {} not playable: {} ({})", video_id, status, reason);
                return Ok(None);
            }
            let t = json["videoDetails"]["title"]
                .as_str()
                .unwrap_or(video_id)
                .to_owned();
            let d = json["videoDetails"]["lengthSeconds"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            (t, d)
        }
        Ok(r) => {
            tracing::warn!("[innertube] /player HTTP {}, using fallback metadata", r.status());
            (video_id.to_owned(), 0)
        }
        Err(e) => {
            tracing::warn!("[innertube] /player request failed: {}, using fallback metadata", e);
            (video_id.to_owned(), 0)
        }
    };

    tracing::info!("[stream] {video_id} → \"{}\" [{}s] via SABR", title, duration);
    Ok(Some(StreamInfo {
        title,
        duration,
        stream_url,
    }))
}
