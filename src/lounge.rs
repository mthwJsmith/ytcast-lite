use std::collections::HashMap;
use std::error::Error as StdError;
use std::time::Duration;

use rand::Rng;
use tokio::sync::{mpsc, watch};
use tokio::time;

use crate::messages::{
    self, encode_outgoing, parse_chunks, zx, IncomingMessage, OutgoingMessage,
};

const BIND_URL: &str = "https://www.youtube.com/api/lounge/bc/bind";
const PAIRING_URL: &str = "https://www.youtube.com/api/lounge/pairing";

/// Base backoff delay for reconnection attempts.
const BACKOFF_BASE_MS: u64 = 500;
/// Maximum number of retries before giving up on a single connection cycle.
const MAX_RETRIES: u32 = 25;

pub type Result<T> = std::result::Result<T, LoungeError>;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum LoungeError {
    Http(reqwest::Error),
    Message(messages::MessageError),
    /// Server returned 400 with "Unknown SID" — needs reconnect
    UnknownSid,
    /// Server returned 410 — token expired, need full re-bind
    Gone,
    /// Protocol error (unexpected response, missing fields, etc.)
    Protocol(String),
    /// Shutdown signal received
    Shutdown,
}

impl std::fmt::Display for LoungeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoungeError::Http(e) => write!(f, "lounge http: {e}"),
            LoungeError::Message(e) => write!(f, "lounge message: {e}"),
            LoungeError::UnknownSid => write!(f, "lounge: unknown SID"),
            LoungeError::Gone => write!(f, "lounge: session gone (410)"),
            LoungeError::Protocol(msg) => write!(f, "lounge protocol: {msg}"),
            LoungeError::Shutdown => write!(f, "lounge: shutdown"),
        }
    }
}

impl std::error::Error for LoungeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LoungeError::Http(e) => Some(e),
            LoungeError::Message(e) => Some(e),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for LoungeError {
    fn from(e: reqwest::Error) -> Self {
        LoungeError::Http(e)
    }
}

impl From<messages::MessageError> for LoungeError {
    fn from(e: messages::MessageError) -> Self {
        LoungeError::Message(e)
    }
}

// ---------------------------------------------------------------------------
// Player commands (sent from lounge to the player task)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum PlayerCommand {
    SetPlaylist {
        video_id: String,
        video_ids: Vec<String>,
        index: usize,
        current_time: f64,
        list_id: String,
        ctt: Option<String>,
        params: Option<String>,
    },
    SetVideo {
        video_id: String,
        ctt: Option<String>,
        current_time: f64,
    },
    Play,
    Pause,
    SeekTo {
        position: f64,
    },
    SetVolume {
        volume: u32,
    },
    GetVolume,
    GetNowPlaying,
    GetPlaylist,
    Stop,
    Previous,
    Next,
    RemoteConnected { theme: String },
    RemoteDisconnected { theme: String },
    UpdatePlaylist {
        video_ids: Vec<String>,
        list_id: String,
        current_index: Option<usize>,
    },
}

// ---------------------------------------------------------------------------
// Lounge session state
// ---------------------------------------------------------------------------

struct LoungeSession {
    client: reqwest::Client,
    device_name: String,
    uuid: String,
    screen_id: String,
    lounge_token: String,
    theme: String,
    sid: String,
    gsessionid: String,
    aid: i32,
    rid: u32,
    ofs: u32,
}

impl LoungeSession {
    fn new(
        client: reqwest::Client,
        device_name: String,
        uuid: String,
        screen_id: String,
        theme: String,
    ) -> Self {
        let mut rng = rand::thread_rng();
        Self {
            client,
            device_name,
            uuid,
            screen_id,
            lounge_token: String::new(),
            theme,
            sid: String::new(),
            gsessionid: String::new(),
            aid: -1,
            rid: rng.gen_range(10_000..100_000),
            ofs: 0,
        }
    }

    fn next_rid(&mut self) -> u32 {
        let r = self.rid;
        self.rid += 1;
        r
    }

    /// Common query parameters used by all bind requests.
    /// Matches the Node.js yt-cast-receiver BindParams.commonParams exactly.
    fn bind_query_base(&self) -> Vec<(String, String)> {
        vec![
            ("device".into(), "LOUNGE_SCREEN".into()),
            ("id".into(), self.uuid.clone()),
            ("obfuscatedGaiaId".into(), String::new()),
            ("name".into(), self.device_name.clone()),
            ("app".into(), "ytcr".into()),
            ("theme".into(), self.theme.clone()),
            ("capabilities".into(), "dsp,mic,dpa,ntb".into()),
            ("cst".into(), "m".into()),
            ("mdxVersion".into(), "2".into()),
            ("loungeIdToken".into(), self.lounge_token.clone()),
            ("VER".into(), "8".into()),
            ("v".into(), "2".into()),
            ("zx".into(), zx()),
            ("t".into(), "1".into()),
        ]
    }

    /// JSON deviceInfo blob sent with initSession and sendMessage requests.
    fn device_info_json(&self) -> String {
        serde_json::json!({
            "brand": "ytcast-lite",
            "model": "ytcast-lite",
            "year": 0,
            "os": "Linux",
            "osVersion": "1.0",
            "chipset": "",
            "clientName": "TVHTML5",
            "dialAdditionalDataSupportLevel": "unsupported",
            "mdxDialServerType": "MDX_DIAL_SERVER_TYPE_UNKNOWN"
        })
        .to_string()
    }
}

// ---------------------------------------------------------------------------
// Pairing / token API calls
// ---------------------------------------------------------------------------

/// Generate a new screen ID. Called once; the result should be persisted.
pub async fn generate_screen_id(client: &reqwest::Client) -> Result<String> {
    let url = format!("{PAIRING_URL}/generate_screen_id");
    let resp = client.get(&url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(LoungeError::Protocol(format!(
            "generate_screen_id returned {status}"
        )));
    }
    let text = resp.text().await?;
    Ok(text.trim().to_owned())
}

/// Fetch a lounge token for the given screen ID.
pub async fn get_lounge_token(
    client: &reqwest::Client,
    screen_id: &str,
) -> Result<String> {
    let url = format!("{PAIRING_URL}/get_lounge_token_batch");
    let resp = client
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("screen_ids={screen_id}"))
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        return Err(LoungeError::Protocol(format!(
            "get_lounge_token_batch returned {status}"
        )));
    }

    let data: serde_json::Value = resp.json().await?;
    let token = data["screens"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|s| s["loungeToken"].as_str())
        .ok_or_else(|| {
            LoungeError::Protocol(format!(
                "missing loungeToken in response: {data}"
            ))
        })?
        .to_owned();

    Ok(token)
}

/// Register a pairing code so a phone can discover this screen.
pub async fn register_pairing_code(
    client: &reqwest::Client,
    screen_id: &str,
    device_name: &str,
    device_id: &str,
    code: &str,
) -> Result<()> {
    let url = format!("{PAIRING_URL}/register_pairing_code");
    let screen_name = format!("YouTube on {}", device_name);
    let body = format!(
        "access_type=permanent&app=ytcr&pairing_code={}&screen_id={}&screen_name={}&device_id={}",
        messages::percent_encode_public(code),
        messages::percent_encode_public(screen_id),
        messages::percent_encode_public(&screen_name),
        messages::percent_encode_public(device_id),
    );

    let resp = client
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;

    let status = resp.status();
    let resp_body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(LoungeError::Protocol(format!(
            "register_pairing_code returned {status}: {resp_body}"
        )));
    }

    tracing::info!("pairing code {code} registered (status={status}, body={resp_body})");
    Ok(())
}

// ---------------------------------------------------------------------------
// Bind operations
// ---------------------------------------------------------------------------

impl LoungeSession {
    /// Perform the initial bind (POST) to establish the session.
    ///
    /// Parses the response for `c` (SID) and `S` (gsessionid) commands.
    async fn initial_bind(&mut self) -> Result<Vec<IncomingMessage>> {
        let rid = self.next_rid();
        let mut query = self.bind_query_base();
        query.push(("deviceInfo".into(), self.device_info_json()));
        query.push(("RID".into(), rid.to_string()));
        query.push(("CVER".into(), "1".into()));

        let resp = self
            .client
            .post(BIND_URL)
            .query(&query)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("count=0")
            .send()
            .await?;

        let status = resp.status();
        if status == reqwest::StatusCode::GONE {
            return Err(LoungeError::Gone);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            if status == reqwest::StatusCode::BAD_REQUEST && body.contains("Unknown SID") {
                return Err(LoungeError::UnknownSid);
            }
            return Err(LoungeError::Protocol(format!(
                "initial_bind returned {status}: {body}"
            )));
        }

        let body = resp.bytes().await?;
        let messages = parse_chunks(&body)?;

        // Extract session identifiers
        for msg in &messages {
            self.handle_session_command(msg);
        }

        if self.sid.is_empty() {
            return Err(LoungeError::Protocol(
                "initial bind did not return a SID (\"c\" command)".into(),
            ));
        }

        tracing::info!("[lounge:{}] session bound: SID={}, gsession={}", self.theme, self.sid, self.gsessionid);
        Ok(messages)
    }

    /// Reconnect after an "Unknown SID" error by passing the old SID/AID as
    /// OSID/OAID parameters.
    async fn reconnect_bind(&mut self) -> Result<Vec<IncomingMessage>> {
        let old_sid = self.sid.clone();
        let old_aid = self.aid;

        let rid = self.next_rid();
        let mut query = self.bind_query_base();
        query.push(("deviceInfo".into(), self.device_info_json()));
        query.push(("RID".into(), rid.to_string()));
        query.push(("CVER".into(), "1".into()));
        query.push(("OSID".into(), old_sid));
        query.push(("OAID".into(), old_aid.to_string()));

        let resp = self
            .client
            .post(BIND_URL)
            .query(&query)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("count=0")
            .send()
            .await?;

        let status = resp.status();
        if status == reqwest::StatusCode::GONE {
            return Err(LoungeError::Gone);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LoungeError::Protocol(format!(
                "reconnect_bind returned {status}: {body}"
            )));
        }

        let body = resp.bytes().await?;
        let messages = parse_chunks(&body)?;

        // Reset SID/gsession from the new response
        self.sid.clear();
        self.gsessionid.clear();
        for msg in &messages {
            self.handle_session_command(msg);
        }

        if self.sid.is_empty() {
            return Err(LoungeError::Protocol(
                "reconnect bind did not return a new SID".into(),
            ));
        }

        tracing::info!("[lounge:{}] reconnected: SID={}, gsession={}", self.theme, self.sid, self.gsessionid);
        Ok(messages)
    }

    /// Start a streaming long-poll GET. Messages are sent through `msg_tx`
    /// as they arrive from the HTTP response stream. The returned future
    /// completes when the connection closes, times out idle, or errors.
    ///
    /// This does not borrow `self` — it clones everything it needs so the
    /// session can be mutated in other `select!` branches.
    fn start_streaming_poll(
        &self,
        msg_tx: mpsc::Sender<Vec<IncomingMessage>>,
    ) -> impl std::future::Future<Output = Result<()>> {
        let mut query = self.bind_query_base();
        query.push(("SID".into(), self.sid.clone()));
        query.push(("CI".into(), "0".into()));
        query.push(("AID".into(), self.aid.to_string()));
        query.push(("gsessionid".into(), self.gsessionid.clone()));
        query.push(("TYPE".into(), "xmlhttp".into()));
        query.push(("RID".into(), "rpc".into()));

        let client = self.client.clone();

        async move {
            tracing::debug!(
                "streaming poll starting (AID={})",
                query.iter().find(|(k, _)| k == "AID").map(|(_, v)| v.as_str()).unwrap_or("?")
            );

            let resp = client
                .get(BIND_URL)
                .query(&query)
                .send()
                .await?;

            let status = resp.status();
            if status == reqwest::StatusCode::GONE {
                return Err(LoungeError::Gone);
            }
            if status == reqwest::StatusCode::BAD_REQUEST {
                let body = resp.text().await.unwrap_or_default();
                if body.contains("Unknown SID") {
                    return Err(LoungeError::UnknownSid);
                }
                return Err(LoungeError::Protocol(format!(
                    "poll returned 400: {body}"
                )));
            }
            if !status.is_success() {
                return Err(LoungeError::Protocol(format!(
                    "poll returned {status}"
                )));
            }

            use futures_util::StreamExt;
            let mut stream = resp.bytes_stream();
            let mut parser = messages::ChunkParser::new();

            loop {
                match time::timeout(Duration::from_secs(45), stream.next()).await {
                    Err(_) => {
                        // No data for 45s — normal idle timeout
                        tracing::trace!("streaming poll idle timeout");
                        return Ok(());
                    }
                    Ok(None) => {
                        // Stream ended (server closed connection)
                        tracing::debug!("streaming poll connection closed");
                        return Ok(());
                    }
                    Ok(Some(Ok(bytes))) => {
                        let msgs = parser.feed(&bytes);
                        if !msgs.is_empty() {
                            tracing::debug!("streaming poll: {} messages received", msgs.len());
                            if msg_tx.send(msgs).await.is_err() {
                                return Ok(()); // receiver dropped
                            }
                        }
                    }
                    Ok(Some(Err(e))) => {
                        return Err(LoungeError::Http(e));
                    }
                }
            }
        }
    }

    /// Send outgoing messages to YouTube via POST.
    async fn send_messages(&mut self, messages: &[OutgoingMessage]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }

        let rid = self.next_rid();
        let mut query = self.bind_query_base();
        query.push(("deviceInfo".into(), self.device_info_json()));
        query.push(("SID".into(), self.sid.clone()));
        query.push(("RID".into(), rid.to_string()));
        query.push(("AID".into(), self.aid.to_string()));
        query.push(("gsessionid".into(), self.gsessionid.clone()));

        let body = encode_outgoing(messages, self.ofs);
        self.ofs += messages.len() as u32;

        tracing::debug!("[lounge] POST body: {}", &body[..body.len().min(500)]);

        let resp = self
            .client
            .post(BIND_URL)
            .query(&query)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await?;

        let status = resp.status();
        if status == reqwest::StatusCode::GONE {
            return Err(LoungeError::Gone);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            if status == reqwest::StatusCode::BAD_REQUEST && body.contains("Unknown SID") {
                return Err(LoungeError::UnknownSid);
            }
            return Err(LoungeError::Protocol(format!(
                "send_messages returned {status}: {body}"
            )));
        }

        Ok(())
    }

    /// Process session-management commands (`c`, `S`) from an incoming message.
    fn handle_session_command(&mut self, msg: &IncomingMessage) {
        match msg.command.as_str() {
            "c" => {
                if let Some(sid) = msg.args.get("") {
                    self.sid = sid.clone();
                }
            }
            "S" => {
                if let Some(gsid) = msg.args.get("") {
                    self.gsessionid = gsid.clone();
                }
            }
            _ => {}
        }

        // Track highest message index
        if msg.index > self.aid {
            self.aid = msg.index;
        }
    }
}

// ---------------------------------------------------------------------------
// Command dispatch — map incoming Lounge commands to PlayerCommands
// ---------------------------------------------------------------------------

/// Try to send a player command, logging if the channel is full or closed.
fn send_cmd(player_tx: &mpsc::Sender<PlayerCommand>, cmd: PlayerCommand) {
    if let Err(e) = player_tx.try_send(cmd) {
        tracing::warn!("player command dropped: {e}");
    }
}

/// Process a batch of incoming messages, dispatching player commands and
/// updating session state. Returns any outgoing messages that should be
/// sent back to YouTube in response.
fn dispatch_messages(
    session: &mut LoungeSession,
    messages: &[IncomingMessage],
    player_tx: &mpsc::Sender<PlayerCommand>,
) -> Vec<OutgoingMessage> {
    let mut responses: Vec<OutgoingMessage> = Vec::new();

    for msg in messages {
        session.handle_session_command(msg);

        match msg.command.as_str() {
            // -- Session control (handled above) --
            "c" | "S" => {}

            // -- Keepalive --
            "noop" => {}

            // -- Remote device lifecycle --
            "remoteConnected" => {
                let name = msg.args.get("name").map(String::as_str).unwrap_or("unknown");
                tracing::info!("[lounge:{}] remote connected: {name}", session.theme);
                send_cmd(player_tx, PlayerCommand::RemoteConnected { theme: session.theme.clone() });
                // Tell the phone we're ready — matches yt-cast-receiver handleSenderConnected:
                // 1. onHasPreviousNextChanged
                // 2. nowPlaying (empty = nothing playing)
                // 3. onStateChange (idle)
                // 4. onAutoplayModeChanged (UNSUPPORTED — we don't do autoplay)
                responses.push(
                    OutgoingMessage::new("onHasPreviousNextChanged")
                        .with_arg("hasPrevious", "false")
                        .with_arg("hasNext", "false"),
                );
                responses.push(
                    OutgoingMessage::new("nowPlaying"),
                );
                responses.push(
                    OutgoingMessage::new("onStateChange")
                        .with_arg("state", "-1"),
                );
                responses.push(
                    OutgoingMessage::new("onAutoplayModeChanged")
                        .with_arg("autoplayMode", "UNSUPPORTED"),
                );
            }
            "remoteDisconnected" => {
                let name = msg.args.get("name").map(String::as_str).unwrap_or("unknown");
                tracing::info!("[lounge:{}] remote disconnected: {name}", session.theme);
                send_cmd(player_tx, PlayerCommand::RemoteDisconnected { theme: session.theme.clone() });
            }

            // -- Lounge status --
            "loungeStatus" => {
                tracing::debug!("lounge status: {:?}", msg.args);
                // Check if any REMOTE_CONTROL senders are connected
                let has_senders = msg.args.get("devices")
                    .map(|d| d.contains("REMOTE_CONTROL"))
                    .unwrap_or(false);
                if has_senders {
                    tracing::info!("[lounge:{}] loungeStatus has remote senders, sending readiness", session.theme);
                    send_cmd(player_tx, PlayerCommand::RemoteConnected { theme: session.theme.clone() });
                    responses.push(
                        OutgoingMessage::new("onHasPreviousNextChanged")
                            .with_arg("hasPrevious", "false")
                            .with_arg("hasNext", "false"),
                    );
                    responses.push(
                        OutgoingMessage::new("nowPlaying"),
                    );
                    responses.push(
                        OutgoingMessage::new("onAutoplayModeChanged")
                            .with_arg("autoplayMode", "UNSUPPORTED"),
                    );
                }
            }

            // -- Playback commands --
            "setPlaylist" => {
                dispatch_set_playlist(&msg.args, player_tx);
            }
            "setVideo" => {
                dispatch_set_video(&msg.args, player_tx);
            }
            "play" => {
                send_cmd(player_tx, PlayerCommand::Play);
            }
            "pause" => {
                send_cmd(player_tx, PlayerCommand::Pause);
            }
            "seekTo" => {
                if let Some(pos) = msg.args.get("newTime").and_then(|s| s.parse::<f64>().ok()) {
                    send_cmd(player_tx, PlayerCommand::SeekTo { position: pos });
                }
            }
            "setVolume" => {
                if let Some(vol) = msg.args.get("volume").and_then(|s| s.parse::<u32>().ok()) {
                    send_cmd(player_tx, PlayerCommand::SetVolume { volume: vol });
                }
            }
            "getVolume" => {
                send_cmd(player_tx, PlayerCommand::GetVolume);
            }
            "getNowPlaying" => {
                send_cmd(player_tx, PlayerCommand::GetNowPlaying);
            }
            "getPlaylist" => {
                send_cmd(player_tx, PlayerCommand::GetPlaylist);
            }
            "updatePlaylist" => {
                dispatch_update_playlist(&msg.args, player_tx);
            }
            "stopVideo" => {
                send_cmd(player_tx, PlayerCommand::Stop);
            }
            "previous" => {
                send_cmd(player_tx, PlayerCommand::Previous);
            }
            "next" => {
                send_cmd(player_tx, PlayerCommand::Next);
            }

            "forceDisconnect" => {
                let reason = msg.args.get("reason").map(String::as_str).unwrap_or("unknown");
                tracing::warn!("forceDisconnect: {reason}");
            }

            other => {
                tracing::debug!("unhandled lounge command: {other} args={:?}", msg.args);
            }
        }
    }

    responses
}

fn dispatch_set_playlist(args: &HashMap<String, String>, player_tx: &mpsc::Sender<PlayerCommand>) {
    let video_ids: Vec<String> = args
        .get("videoIds")
        .or_else(|| args.get("videoId"))
        .map(|s| s.split(',').map(String::from).collect())
        .unwrap_or_default();

    if video_ids.is_empty() {
        tracing::warn!("setPlaylist with no video IDs: {args:?}");
        return;
    }

    let index: usize = args
        .get("currentIndex")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
        .min(video_ids.len().saturating_sub(1));

    let video_id = args
        .get("videoId")
        .cloned()
        .unwrap_or_else(|| video_ids.get(index).cloned().unwrap_or_default());

    let current_time: f64 = args
        .get("currentTime")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);

    let list_id = args.get("listId").cloned().unwrap_or_default();
    let ctt = args.get("ctt").cloned();
    let params = args.get("params").cloned();

    send_cmd(player_tx, PlayerCommand::SetPlaylist {
        video_id,
        video_ids,
        index,
        current_time,
        list_id,
        ctt,
        params,
    });
}

fn dispatch_set_video(args: &HashMap<String, String>, player_tx: &mpsc::Sender<PlayerCommand>) {
    let video_id = match args.get("videoId") {
        Some(id) if !id.is_empty() => id.clone(),
        _ => {
            tracing::warn!("setVideo with no videoId: {args:?}");
            return;
        }
    };

    let ctt = args.get("ctt").cloned();

    let current_time: f64 = args
        .get("currentTime")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);

    send_cmd(player_tx, PlayerCommand::SetVideo {
        video_id,
        ctt,
        current_time,
    });
}

fn dispatch_update_playlist(args: &HashMap<String, String>, player_tx: &mpsc::Sender<PlayerCommand>) {
    let video_ids: Vec<String> = args
        .get("videoIds")
        .map(|s| s.split(',').map(String::from).collect())
        .unwrap_or_default();

    if video_ids.is_empty() {
        tracing::warn!("updatePlaylist with no video IDs: {args:?}");
        return;
    }

    let list_id = args.get("listId").cloned().unwrap_or_default();
    let current_index: Option<usize> = args
        .get("currentIndex")
        .and_then(|s| s.parse().ok());

    tracing::info!(
        "[lounge] updatePlaylist: {} videos, currentIndex={:?}",
        video_ids.len(),
        current_index
    );

    send_cmd(player_tx, PlayerCommand::UpdatePlaylist {
        video_ids,
        list_id,
        current_index,
    });
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Run the Lounge protocol loop.
///
/// This is the primary entry point for the YouTube Lounge integration. It:
/// 1. Obtains a lounge token
/// 2. Performs the initial bind to establish a session
/// 3. Runs a receive loop (long-polling) and a send loop concurrently
/// 4. Dispatches incoming commands to `player_cmd_tx`
/// 5. Forwards state updates from `state_rx` to YouTube
/// 6. Handles pairing codes from `pairing_rx`
/// 7. Reconnects automatically on transient errors with exponential backoff
///
/// Returns `Ok(())` only when `shutdown` is signaled.
pub async fn run_lounge(
    client: reqwest::Client,
    device_name: String,
    uuid: String,
    screen_id: String,
    theme: String,
    player_cmd_tx: mpsc::Sender<PlayerCommand>,
    state_rx: mpsc::Receiver<OutgoingMessage>,
    pairing_rx: mpsc::Receiver<String>,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut session = LoungeSession::new(client.clone(), device_name, uuid, screen_id, theme);
    let theme_tag = format!("[lounge:{}]", session.theme);
    let mut state_rx = state_rx;
    let mut shutdown = shutdown;

    // Spawn a dedicated task for pairing code registration so it never
    // interrupts the long-poll. This runs independently from the session loop.
    let pair_client = client;
    let pair_screen = session.screen_id.clone();
    let pair_name = session.device_name.clone();
    let pair_uuid = session.uuid.clone();
    tokio::spawn(async move {
        let mut pairing_rx = pairing_rx;
        while let Some(code) = pairing_rx.recv().await {
            if let Err(e) = register_pairing_code(
                &pair_client,
                &pair_screen,
                &pair_name,
                &pair_uuid,
                &code,
            ).await {
                tracing::error!("failed to register pairing code: {e}");
            }
        }
    });

    // Outer loop: handles full reconnection (token refresh + re-bind)
    loop {
        if *shutdown.borrow() {
            return Err(LoungeError::Shutdown);
        }

        // -- Get lounge token --
        tracing::info!("{} obtaining lounge token for screen {}", theme_tag, session.screen_id);
        match get_lounge_token(&session.client, &session.screen_id).await {
            Ok(token) => {
                session.lounge_token = token;
                tracing::info!("{} lounge token acquired", theme_tag);
            }
            Err(e) => {
                tracing::error!("failed to get lounge token: {e}");
                backoff_sleep(1, &mut shutdown).await?;
                continue;
            }
        }

        // -- Initial bind --
        session.aid = -1;
        session.ofs = 0;
        session.sid.clear();
        session.gsessionid.clear();
        session.rid = rand::thread_rng().gen_range(10_000..100_000);

        let initial_messages = match session.initial_bind().await {
            Ok(msgs) => msgs,
            Err(LoungeError::Gone) => {
                tracing::warn!("session gone during initial bind, refreshing token");
                continue;
            }
            Err(e) => {
                tracing::error!("initial bind failed: {e}");
                backoff_sleep(1, &mut shutdown).await?;
                continue;
            }
        };

        // Dispatch any commands from the initial bind response
        let responses = dispatch_messages(&mut session, &initial_messages, &player_cmd_tx);
        if !responses.is_empty() {
            if let Err(e) = session.send_messages(&responses).await {
                tracing::error!("failed to send initial responses: {e}");
            }
        }

        // -- Session loop: streaming long-poll + state send --
        //
        // `start_streaming_poll()` pushes messages through a channel as they
        // arrive from the HTTP stream. The future only completes when the
        // connection closes or errors. A fresh channel is created for each
        // poll connection to avoid stale messages after reconnects.
        let mut retry_count: u32 = 0;

        'session: loop {
            if *shutdown.borrow() {
                return Err(LoungeError::Shutdown);
            }

            // Fresh channel per poll connection
            let (poll_msg_tx, mut poll_msg_rx) = mpsc::channel::<Vec<IncomingMessage>>(16);

            let poll_fut = session.start_streaming_poll(poll_msg_tx);
            tokio::pin!(poll_fut);

            // Inner loop: process messages while the streaming connection is alive
            let poll_result = 'poll: loop {
                tokio::select! {
                    biased;

                    // Shutdown signal (highest priority)
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            return Err(LoungeError::Shutdown);
                        }
                    }

                    // Incoming messages from the streaming poll
                    Some(messages) = poll_msg_rx.recv() => {
                        retry_count = 0;
                        let resps = dispatch_messages(&mut session, &messages, &player_cmd_tx);
                        if !resps.is_empty() {
                            if let Err(e) = session.send_messages(&resps).await {
                                tracing::error!("failed to send responses: {e}");
                            }
                        }
                    }

                    // Outgoing state update from player
                    Some(msg) = state_rx.recv() => {
                        let mut batch = vec![msg];
                        while let Ok(extra) = state_rx.try_recv() {
                            batch.push(extra);
                        }

                        tracing::info!(
                            "[lounge:{}] sending {} state messages: [{}]",
                            session.theme,
                            batch.len(),
                            batch.iter().map(|m| m.command.as_str()).collect::<Vec<_>>().join(", ")
                        );

                        match session.send_messages(&batch).await {
                            Ok(()) => {
                                retry_count = 0;
                                tracing::info!("[lounge:{}] state messages sent OK", session.theme);
                            }
                            Err(LoungeError::UnknownSid) => {
                                tracing::warn!("unknown SID on send, reconnecting");
                                match session.reconnect_bind().await {
                                    Ok(msgs) => {
                                        let resps = dispatch_messages(&mut session, &msgs, &player_cmd_tx);
                                        if !resps.is_empty() {
                                            let _ = session.send_messages(&resps).await;
                                        }
                                        retry_count = 0;
                                        // Restart poll with new SID
                                        break 'poll Ok(());
                                    }
                                    Err(LoungeError::Gone) => break 'session,
                                    Err(e) => {
                                        tracing::error!("reconnect failed: {e}");
                                        break 'session;
                                    }
                                }
                            }
                            Err(LoungeError::Gone) => break 'session,
                            Err(e) => {
                                tracing::error!("send error: {e}");
                                retry_count += 1;
                                if retry_count >= MAX_RETRIES {
                                    tracing::error!("max retries exceeded, full reconnect");
                                    break 'session;
                                }
                                backoff_sleep(retry_count, &mut shutdown).await?;
                            }
                        }
                    }

                    // Streaming poll completed (connection closed or error)
                    result = &mut poll_fut => {
                        break 'poll result;
                    }
                }
            };

            // Handle why the poll connection ended
            match poll_result {
                Ok(()) => {
                    // Normal close or idle timeout — restart immediately
                }
                Err(LoungeError::UnknownSid) => {
                    tracing::warn!("unknown SID on poll, reconnecting");
                    match session.reconnect_bind().await {
                        Ok(msgs) => {
                            let resps = dispatch_messages(&mut session, &msgs, &player_cmd_tx);
                            if !resps.is_empty() {
                                let _ = session.send_messages(&resps).await;
                            }
                            retry_count = 0;
                        }
                        Err(LoungeError::Gone) => break 'session,
                        Err(e) => {
                            tracing::error!("reconnect failed: {e}");
                            break 'session;
                        }
                    }
                }
                Err(LoungeError::Gone) => {
                    tracing::warn!("session gone (410), refreshing token");
                    break 'session;
                }
                Err(LoungeError::Http(ref e)) if e.is_timeout() => {
                    tracing::trace!("poll timeout, restarting");
                }
                Err(LoungeError::Http(ref e)) if e.is_connect() || is_io_error(e) => {
                    retry_count += 1;
                    tracing::warn!("poll network error (attempt {retry_count}): {e}");
                    if retry_count >= MAX_RETRIES {
                        tracing::error!("max retries exceeded, full reconnect");
                        break 'session;
                    }
                    backoff_sleep(retry_count, &mut shutdown).await?;
                }
                Err(e) => {
                    retry_count += 1;
                    tracing::error!("poll error (attempt {retry_count}): {e}");
                    if retry_count >= MAX_RETRIES {
                        tracing::error!("max retries exceeded, full reconnect");
                        break 'session;
                    }
                    backoff_sleep(retry_count, &mut shutdown).await?;
                }
            }
        }

        // If we break out of the session loop, wait briefly and restart
        tracing::info!("session ended, will reconnect");
        backoff_sleep(1, &mut shutdown).await?;
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check if a reqwest error is caused by an underlying I/O error (connection
/// reset, broken pipe, etc.).
fn is_io_error(e: &reqwest::Error) -> bool {
    let mut source = e.source();
    while let Some(err) = source {
        if err.downcast_ref::<std::io::Error>().is_some() {
            return true;
        }
        source = err.source();
    }
    false
}

/// Exponential backoff sleep: `BACKOFF_BASE_MS * 2^(attempt-1)`, clamped.
/// Returns `Err(Shutdown)` if the shutdown signal fires during the wait.
async fn backoff_sleep(attempt: u32, shutdown: &mut watch::Receiver<bool>) -> Result<()> {
    // 500ms, 1s, 2s, 4s, 8s, 16s, ... capped at ~30s
    let delay_ms = BACKOFF_BASE_MS.saturating_mul(1u64 << attempt.min(6));
    let delay = Duration::from_millis(delay_ms);
    tracing::debug!("backoff sleep: {delay_ms}ms (attempt {attempt})");

    tokio::select! {
        _ = time::sleep(delay) => Ok(()),
        _ = shutdown.changed() => {
            if *shutdown.borrow() {
                Err(LoungeError::Shutdown)
            } else {
                Ok(())
            }
        }
    }
}

