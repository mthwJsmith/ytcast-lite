/// Stream resolver — all audio goes through ytresolve's SABR proxy.
///
/// ytresolve runs locally on the Pi (or on a VPS) and streams audio using
/// YouTube's native SABR (Server Adaptive Bit Rate) protocol via the
/// googlevideo library. This is the same protocol the official YouTube app
/// uses — it always works regardless of what YouTube does with direct URLs.
///
/// YTRESOLVE_URL must be set (e.g. "http://localhost:3033").

static RESOLVE_URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static RESOLVE_SECRET: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn resolve_url() -> &'static str {
    RESOLVE_URL.get_or_init(|| {
        std::env::var("YTRESOLVE_URL")
            .unwrap_or_else(|_| "http://localhost:3033".to_owned())
    })
}

fn resolve_secret() -> &'static str {
    RESOLVE_SECRET.get_or_init(|| std::env::var("YTRESOLVE_SECRET").unwrap_or_default())
}

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub title: String,
    pub duration: u64,
    pub stream_url: String,
}

/// Resolve a video ID to a playable audio stream URL via ytresolve SABR proxy.
pub async fn resolve_stream(
    _client: &reqwest::Client,
    video_id: &str,
    _ctt: Option<&str>,
    _playlist_id: Option<&str>,
) -> Result<Option<StreamInfo>> {
    let base = resolve_url().trim_end_matches('/');
    let secret = resolve_secret();

    let stream_url = if secret.is_empty() {
        format!("{}/stream/{}", base, video_id)
    } else {
        format!("{}/stream/{}?secret={}", base, video_id, secret)
    };

    tracing::info!("[stream] {video_id} → SABR stream via ytresolve");
    Ok(Some(StreamInfo {
        title: video_id.to_owned(),
        duration: 0,
        stream_url,
    }))
}
