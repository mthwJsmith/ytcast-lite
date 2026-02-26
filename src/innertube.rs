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
    ctt: Option<&str>,
    _playlist_id: Option<&str>,
    yt_cookies: Option<&crate::cookies::YtCookies>,
    yt_session: Option<&crate::cookies::YtMusicSession>,
    oauth_token: Option<&str>,
) -> Result<Option<StreamInfo>> {
    let stream_url = format!("http://localhost:8008/stream/{}", video_id);

    // When ctt or session is available, use WEB_REMIX (YouTube Music web client).
    // WEB_REMIX is the only client that accepts credentialTransferTokens from cast sessions.
    let (title, duration) = if ctt.is_some() || yt_session.is_some() {
        match web_remix_player(client, video_id, ctt, yt_cookies, yt_session, oauth_token).await {
            Ok(td) => td,
            Err(e) => {
                tracing::warn!("[innertube] WEB_REMIX failed: {}, trying IOS", e);
                ios_player(client, video_id).await.unwrap_or_else(|e2| {
                    tracing::warn!("[innertube] IOS failed: {}, using fallback", e2);
                    (video_id.to_owned(), 0)
                })
            }
        }
    } else {
        ios_player(client, video_id).await.unwrap_or_else(|e| {
            tracing::warn!("[innertube] IOS failed: {}, using fallback", e);
            (video_id.to_owned(), 0)
        })
    };

    tracing::info!("[stream] {video_id} → \"{}\" [{}s] via SABR", title, duration);
    Ok(Some(StreamInfo {
        title,
        duration,
        stream_url,
    }))
}

async fn web_remix_player(
    client: &reqwest::Client,
    video_id: &str,
    ctt: Option<&str>,
    yt_cookies: Option<&crate::cookies::YtCookies>,
    yt_session: Option<&crate::cookies::YtMusicSession>,
    oauth_token: Option<&str>,
) -> Result<(String, u64)> {
    // Build context: use full INNERTUBE_CONTEXT from session if available
    let body = if let Some(session) = yt_session {
        let mut ctx = session.innertube_context.clone();
        if let Some(token) = ctt {
            if let Some(user) = ctx.get_mut("user").and_then(|u| u.as_object_mut()) {
                user.insert(
                    "credentialTransferTokens".to_owned(),
                    serde_json::json!([{
                        "scope": "VIDEO",
                        "token": token,
                    }]),
                );
            }
        }
        let mut playback_ctx = serde_json::json!({
            "contentPlaybackContext": {
                "html5Preference": "HTML5_PREF_WANTS",
            }
        });
        if let Some(sts) = session.signature_timestamp {
            playback_ctx["contentPlaybackContext"]["signatureTimestamp"] = serde_json::json!(sts);
        }
        serde_json::json!({
            "videoId": video_id,
            "context": ctx,
            "contentCheckOk": true,
            "racyCheckOk": true,
            "playbackContext": playback_ctx,
        })
    } else {
        // Minimal context (may return UNPLAYABLE without full ytcfg)
        let mut context = serde_json::json!({
            "client": {
                "clientName": "WEB_REMIX",
                "clientVersion": "1.20260114.03.00",
                "hl": "en",
                "gl": "US",
            }
        });
        if let Some(token) = ctt {
            context["user"] = serde_json::json!({
                "credentialTransferTokens": [{
                    "scope": "VIDEO",
                    "token": token,
                }]
            });
        }
        serde_json::json!({
            "videoId": video_id,
            "context": context,
            "contentCheckOk": true,
            "racyCheckOk": true,
        })
    };

    // Authenticated requests don't need API key (matches yt-dlp)
    let url = if yt_cookies.is_some() {
        "https://music.youtube.com/youtubei/v1/player?prettyPrint=false"
    } else {
        "https://music.youtube.com/youtubei/v1/player?key=AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30&prettyPrint=false"
    };

    let mut req = client
        .post(url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36")
        .header("Content-Type", "application/json")
        .header("Origin", "https://music.youtube.com")
        .header("Referer", "https://music.youtube.com/")
        .header("X-YouTube-Client-Name", "67")
        .header("X-YouTube-Client-Version",
            yt_session
                .and_then(|s| s.innertube_context.get("client")
                    .and_then(|c| c.get("clientVersion"))
                    .and_then(|v| v.as_str()))
                .unwrap_or("1.20260114.03.00")
        );

    // Visitor data from session
    if let Some(session) = yt_session {
        if let Some(vd) = session.innertube_context.get("client")
            .and_then(|c| c.get("visitorData"))
            .and_then(|v| v.as_str())
        {
            req = req.header("X-Goog-Visitor-Id", vd);
        }
    }

    // Auth: OAuth Bearer > cookie-based triple SAPISIDHASH
    if let Some(token) = oauth_token {
        req = req.header("Authorization", format!("Bearer {}", token));
    } else if let Some(c) = yt_cookies {
        let user_sid = yt_session.and_then(|s| s.user_session_id.as_deref());
        req = req
            .header("Cookie", &c.cookie_header)
            .header(
                "Authorization",
                crate::cookies::full_sapisidhash(c, "https://music.youtube.com", user_sid),
            )
            .header("X-Origin", "https://music.youtube.com");

        if let Some(session) = yt_session {
            if let Some(ref idx) = session.session_index {
                req = req.header("X-Goog-Authuser", idx.as_str());
            }
            if let Some(ref uid) = session.user_session_id {
                req = req.header("X-Goog-Pageid", uid.as_str());
            }
            if session.logged_in {
                req = req.header("X-Youtube-Bootstrap-Logged-In", "true");
            }
        }
    }

    let resp = req.json(&body).send().await?;

    if !resp.status().is_success() {
        return Err(format!("/player HTTP {}", resp.status()).into());
    }

    parse_player_response(video_id, resp).await
}

async fn ios_player(
    client: &reqwest::Client,
    video_id: &str,
) -> Result<(String, u64)> {
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
        .header("User-Agent", "com.google.ios.youtube/19.45.4 (iPhone16,2; U; CPU iOS 18_1_0 like Mac OS X;)")
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(format!("/player HTTP {}", resp.status()).into());
    }

    parse_player_response(video_id, resp).await
}

async fn parse_player_response(
    video_id: &str,
    resp: reqwest::Response,
) -> Result<(String, u64)> {
    let json: serde_json::Value = resp.json().await?;
    let status = json["playabilityStatus"]["status"]
        .as_str()
        .unwrap_or("UNKNOWN");
    if status != "OK" {
        let reason = json["playabilityStatus"]["reason"]
            .as_str()
            .unwrap_or("unknown");
        tracing::warn!("[innertube] {} not playable: {} ({})", video_id, status, reason);
        return Err(format!("{}: {} ({})", video_id, status, reason).into());
    }
    let title = json["videoDetails"]["title"]
        .as_str()
        .unwrap_or(video_id)
        .to_owned();
    let duration = json["videoDetails"]["lengthSeconds"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    Ok((title, duration))
}
