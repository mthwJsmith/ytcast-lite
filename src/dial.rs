use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, HeaderName, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use futures_util::StreamExt;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

pub struct DialState {
    /// Whether the "YouTube" DIAL app is currently running.
    pub running: AtomicBool,
    /// Channel to deliver pairing codes for the "cl" (YouTube) session.
    pub pairing_tx: mpsc::Sender<String>,
    /// Channel to deliver pairing codes for the "m" (YouTube Music) session.
    pub pairing_tx_m: mpsc::Sender<String>,
    /// Player command channel for debug endpoint.
    pub player_cmd_tx: Option<mpsc::Sender<crate::lounge::PlayerCommand>>,
    /// Human-readable name shown during discovery (e.g. "Living Room Pi").
    pub device_name: String,
    /// UPnP UUID for this device (without the "uuid:" prefix).
    pub uuid: String,
    /// Local IP for building absolute Application-URL.
    pub local_ip: std::net::Ipv4Addr,
    /// Port this DIAL server listens on.
    pub port: u16,
    /// Shared HTTP client for outbound requests (e.g. SABR streaming).
    pub http_client: reqwest::Client,
    /// Credential transfer token from the casting session (YTM Premium auth).
    pub ctt: std::sync::RwLock<Option<String>>,
    /// YouTube cookies for Premium auth (only sent with WEB_REMIX /player calls).
    pub yt_cookies: Option<std::sync::Arc<crate::cookies::YtCookies>>,
    /// Live YouTube Music session (full INNERTUBE_CONTEXT from page).
    pub yt_session: Option<std::sync::Arc<crate::cookies::YtMusicSession>>,
    /// OAuth state for Premium Bearer token auth.
    pub oauth_state: Option<std::sync::Arc<crate::oauth::OAuthState>>,
    /// Screen ID for the "cl" (YouTube) Lounge session, exposed in DIAL additionalData.
    pub screen_id: String,
}

// ---------------------------------------------------------------------------
// Server entry point
// ---------------------------------------------------------------------------

/// Start the DIAL HTTP server on the given port.
///
/// This serves the UPnP device description and the `/apps/YouTube` DIAL
/// endpoints that the YouTube Music app talks to after SSDP discovery.
pub async fn run_dial_server(
    port: u16,
    state: Arc<DialState>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let app = Router::new()
        .route("/ssdp/device-desc.xml", get(device_description))
        .route("/apps/YouTube", get(app_status))
        .route("/apps/YouTube", post(app_launch))
        .route("/apps/YouTube/run", delete(app_stop))
        .route("/stream/{video_id}", get(stream_audio))
        .route("/debug/play/{video_id}", get(debug_play))
        .route("/debug/pause", get(debug_pause))
        .route("/debug/resume", get(debug_resume))
        .route("/debug/seek/{seconds}", get(debug_seek))
        .route("/debug/stop", get(debug_stop))
        .with_state(state);

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("DIAL server listening on 0.0.0.0:{port}");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        // Wait until shutdown is signalled
        loop {
            if *shutdown.borrow() {
                return;
            }
            if shutdown.changed().await.is_err() {
                return;
            }
        }
    })
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// GET /ssdp/device-desc.xml
// ---------------------------------------------------------------------------

async fn device_description(
    method: Method,
    uri: Uri,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<DialState>>,
) -> Response {
    tracing::debug!("DIAL {} {} from {}", method, uri, addr);

    let base_url = format!("http://{}:{}", state.local_ip, state.port);
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion>
    <major>1</major>
    <minor>0</minor>
  </specVersion>
  <URLBase>{base_url}</URLBase>
  <device>
    <deviceType>urn:dial-multiscreen-org:device:dial:1</deviceType>
    <friendlyName>{name}</friendlyName>
    <manufacturer>ytcast-lite</manufacturer>
    <modelName>ytcast-lite 0.1</modelName>
    <UDN>uuid:{uuid}</UDN>
    <iconList>
      <icon>
        <mimetype>image/png</mimetype>
        <width>48</width>
        <height>48</height>
        <depth>24</depth>
        <url>/icon.png</url>
      </icon>
    </iconList>
    <serviceList>
      <service>
        <serviceType>urn:dial-multiscreen-org:service:dial:1</serviceType>
        <serviceId>urn:dial-multiscreen-org:serviceId:dial</serviceId>
        <controlURL>/ssdp/notfound</controlURL>
        <eventSubURL>/ssdp/notfound</eventSubURL>
        <SCPDURL>/ssdp/notfound</SCPDURL>
      </service>
    </serviceList>
  </device>
</root>"#,
        base_url = base_url,
        name = state.device_name,
        uuid = state.uuid,
    );

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
    // The Application-URL header MUST be an absolute URL per DIAL spec Section 5.4.
    // No trailing slash -- matches peer-dial Node.js reference implementation.
    let app_url = format!("http://{}:{}/apps", state.local_ip, state.port);
    headers.insert(
        HeaderName::from_static("application-url"),
        app_url.parse().unwrap(),
    );

    (StatusCode::OK, headers, xml).into_response()
}

// ---------------------------------------------------------------------------
// GET /apps/YouTube -- app status
// ---------------------------------------------------------------------------

async fn app_status(
    method: Method,
    uri: Uri,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<DialState>>,
) -> Response {
    tracing::debug!("DIAL {} {} from {}", method, uri, addr);

    let running = state.running.load(Ordering::Relaxed);
    let state_str = if running { "running" } else { "stopped" };

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<service xmlns="urn:dial-multiscreen-org:schemas:dial" dialVer="1.7">
  <name>YouTube</name>
  <options allowStop="true"/>
  <state>{state_str}</state>
  <link rel="run" href="run"/>
  <additionalData>
    <screenId>{screen_id}</screenId>
  </additionalData>
</service>"#,
        screen_id = state.screen_id,
    );

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());

    (StatusCode::OK, headers, xml).into_response()
}

// ---------------------------------------------------------------------------
// POST /apps/YouTube -- launch
// ---------------------------------------------------------------------------

async fn app_launch(
    method: Method,
    uri: Uri,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<DialState>>,
    body: String,
) -> Response {
    tracing::debug!("DIAL {} {} from {} body={}", method, uri, addr, body);

    // Body is form-urlencoded: pairingCode=XXX&theme=m&v=VIDEO_ID&t=0
    let params: Vec<(String, String)> =
        serde_urlencoded::from_str(&body).unwrap_or_default();

    let theme = params.iter().find(|(k, _)| k == "theme").map(|(_, v)| v.as_str()).unwrap_or("cl");

    if let Some((_, code)) = params.iter().find(|(k, _)| k == "pairingCode") {
        if !code.is_empty() {
            let tx = if theme == "m" { &state.pairing_tx_m } else { &state.pairing_tx };
            tracing::info!("DIAL launch: theme={}, pairing code routing to {} session", theme, theme);
            if let Err(e) = tx.send(code.clone()).await {
                tracing::error!("failed to send pairing code: {e}");
            }
        }
    }

    state.running.store(true, Ordering::Relaxed);

    // DIAL spec: 201 Created with LOCATION pointing to the running instance.
    let mut headers = HeaderMap::new();
    headers.insert(header::LOCATION, "/apps/YouTube/run".parse().unwrap());

    (StatusCode::CREATED, headers).into_response()
}

// ---------------------------------------------------------------------------
// DELETE /apps/YouTube/run -- stop
// ---------------------------------------------------------------------------

async fn app_stop(
    method: Method,
    uri: Uri,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<DialState>>,
) -> StatusCode {
    tracing::debug!("DIAL {} {} from {}", method, uri, addr);
    state.running.store(false, Ordering::Relaxed);
    StatusCode::OK
}

// ---------------------------------------------------------------------------
// GET /debug/play/:video_id -- CLI test endpoint (simulates a cast)
// ---------------------------------------------------------------------------

async fn debug_play(
    axum::extract::Path(video_id): axum::extract::Path<String>,
    State(state): State<Arc<DialState>>,
) -> Response {
    use crate::lounge::PlayerCommand;

    let tx = match &state.player_cmd_tx {
        Some(tx) => tx,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no player channel").into_response(),
    };

    // Use stored ctt from last real cast (if any)
    let ctt = state.ctt.read().unwrap().clone();
    tracing::info!("[debug] play {} (ctt={})", video_id, ctt.is_some());

    // Simulate remote connect + play
    let _ = tx.send(PlayerCommand::RemoteConnected { theme: "cl".to_string() }).await;
    let _ = tx.send(PlayerCommand::SetPlaylist {
        video_id: video_id.clone(),
        video_ids: vec![video_id],
        index: 0,
        current_time: 0.0,
        list_id: String::new(),
        ctt,
        params: None,
    }).await;

    (StatusCode::OK, "playing").into_response()
}

async fn debug_pause(State(state): State<Arc<DialState>>) -> Response {
    let tx = match &state.player_cmd_tx {
        Some(tx) => tx,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no player channel").into_response(),
    };
    tracing::info!("[debug] pause");
    let _ = tx.send(crate::lounge::PlayerCommand::Pause).await;
    (StatusCode::OK, "paused").into_response()
}

async fn debug_resume(State(state): State<Arc<DialState>>) -> Response {
    let tx = match &state.player_cmd_tx {
        Some(tx) => tx,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no player channel").into_response(),
    };
    tracing::info!("[debug] resume");
    let _ = tx.send(crate::lounge::PlayerCommand::Play).await;
    (StatusCode::OK, "resumed").into_response()
}

async fn debug_seek(
    axum::extract::Path(seconds): axum::extract::Path<f64>,
    State(state): State<Arc<DialState>>,
) -> Response {
    let tx = match &state.player_cmd_tx {
        Some(tx) => tx,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no player channel").into_response(),
    };
    tracing::info!("[debug] seek to {}s", seconds);
    let _ = tx.send(crate::lounge::PlayerCommand::SeekTo { position: seconds }).await;
    (StatusCode::OK, format!("seeked to {}s", seconds)).into_response()
}

async fn debug_stop(State(state): State<Arc<DialState>>) -> Response {
    let tx = match &state.player_cmd_tx {
        Some(tx) => tx,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no player channel").into_response(),
    };
    tracing::info!("[debug] stop");
    let _ = tx.send(crate::lounge::PlayerCommand::Stop).await;
    (StatusCode::OK, "stopped").into_response()
}

// ---------------------------------------------------------------------------
// GET /stream/:video_id -- SABR audio stream
// ---------------------------------------------------------------------------

async fn stream_audio(
    axum::extract::Path(video_id): axum::extract::Path<String>,
    State(state): State<Arc<DialState>>,
) -> Response {
    // Validate video ID (11 chars, alphanumeric + - + _)
    if !video_id
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        || video_id.len() != 11
    {
        return (StatusCode::BAD_REQUEST, "invalid video id").into_response();
    }

    let ctt = state.ctt.read().unwrap().clone();
    let yt_cookies = state.yt_cookies.as_ref().map(|c| c.as_ref());
    let yt_session = state.yt_session.as_ref().map(|s| s.as_ref());
    let oauth = state.oauth_state.clone();
    match crate::sabr::stream::stream_audio(&state.http_client, &video_id, ctt.as_deref(), yt_cookies, yt_session, oauth.as_ref()).await {
        Ok((info, rx)) => {
            // Build streaming response from the mpsc receiver.
            // When YTCAST_DUMP_STREAM=1, also write every byte to /tmp/sabr-dump-{id}.webm
            // so we can inspect with ffprobe if MPD crashes.
            let dump_file = if std::env::var("YTCAST_DUMP_STREAM").is_ok() {
                let path = format!("/tmp/sabr-dump-{}.webm", video_id);
                tracing::info!("[stream] dumping raw stream to {}", path);
                std::fs::File::create(&path).ok()
            } else {
                None
            };
            let dump_file = std::sync::Arc::new(std::sync::Mutex::new(dump_file));

            let stream = ReceiverStream::new(rx).map(move |chunk| {
                if let Ok(mut guard) = dump_file.lock() {
                    if let Some(ref mut f) = *guard {
                        use std::io::Write;
                        let _ = f.write_all(&chunk);
                    }
                }
                Ok::<_, std::io::Error>(chunk)
            });
            let body = axum::body::Body::from_stream(stream);

            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                info.mime_type
                    .parse()
                    .unwrap_or_else(|_| "audio/webm".parse().unwrap()),
            );
            headers.insert(
                header::TRANSFER_ENCODING,
                "chunked".parse().unwrap(),
            );
            // Replace non-ASCII characters with '?' for a header-safe title
            let safe_title: String = info
                .title
                .chars()
                .map(|c| if c.is_ascii() { c } else { '?' })
                .collect();
            headers.insert(
                HeaderName::from_static("x-title"),
                safe_title
                    .parse()
                    .unwrap_or_else(|_| "unknown".parse().unwrap()),
            );
            headers.insert(
                HeaderName::from_static("x-duration"),
                info.duration_secs.to_string().parse().unwrap(),
            );

            (StatusCode::OK, headers, body).into_response()
        }
        Err(e) => {
            tracing::error!("[stream] {} failed: {}", video_id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("error: {}", e),
            )
                .into_response()
        }
    }
}
