mod config;
mod cookies;
mod dial;
mod innertube;
mod lounge;
mod messages;
mod mpd;
mod nsig;
mod oauth;
mod sabr;
mod ssdp;

use std::sync::Arc;

// ---------------------------------------------------------------------------
// Playlist tracking state (local to main)
// ---------------------------------------------------------------------------

struct PlaylistState {
    video_ids: Vec<String>,
    current_index: usize,
    list_id: String,
    /// Credential transfer token from the cast session.
    ctt: Option<String>,
    /// Opaque params token from the cast session (passed through to nowPlaying).
    params: Option<String>,
    /// Current track title (from InnerTube).
    current_title: String,
    /// Current track duration in seconds.
    current_duration: u64,
    /// Stable content playback nonce — generated once, reused for all state reports.
    cpn: String,
    /// Last reported player state (for reference-matching NowPlaying logic).
    last_state: messages::PlayerState,
}

impl Default for PlaylistState {
    fn default() -> Self {
        Self {
            video_ids: Vec::new(),
            current_index: 0,
            list_id: String::new(),
            ctt: None,
            params: None,
            current_title: String::new(),
            current_duration: 0,
            cpn: messages::generate_cpn(),
            last_state: messages::PlayerState::Idle,
        }
    }
}

// ---------------------------------------------------------------------------
// Memory logging (Linux only)
// ---------------------------------------------------------------------------

fn log_memory() {
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if line.starts_with("VmRSS:") {
                    tracing::info!("[mem] {}", line.trim());
                    return;
                }
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        // Memory introspection not available on this platform.
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 1. Init logging
    tracing_subscriber::fmt::init();

    // 2. Load/create config
    let mut config = config::Config::load()?;
    if config.uuid.is_empty() {
        config.uuid = uuid::Uuid::new_v4().to_string();
        config.save()?;
    }
    tracing::info!("[init] UUID: {}", config.uuid);

    // 3. Read env vars
    let device_name = std::env::var("DEVICE_NAME").unwrap_or_else(|_| "Living Room Pi".into());
    let mpd_host = std::env::var("MPD_HOST").unwrap_or_else(|_| "localhost".into());
    let mpd_port: u16 = std::env::var("MPD_PORT")
        .unwrap_or_else(|_| "6600".into())
        .parse()
        .unwrap_or(6600);
    let dial_port: u16 = 8008;

    // 4. Load YouTube cookies (for Premium auth — bypasses PoToken requirement)
    let cookie_path = std::env::var("YTM_COOKIES").unwrap_or_else(|_| {
        dirs::home_dir()
            .map(|h| h.join("ytm-cookies.txt").to_string_lossy().to_string())
            .unwrap_or_else(|| "ytm-cookies.txt".into())
    });

    let yt_cookies = if std::path::Path::new(&cookie_path).exists() {
        match cookies::load_cookie_file(&cookie_path) {
            Ok(Some(c)) => {
                tracing::info!("[init] Loaded cookies from {}", cookie_path);
                Some(Arc::new(c))
            }
            Ok(None) => {
                tracing::warn!("[init] Cookie file {} has no SAPISID", cookie_path);
                None
            }
            Err(e) => {
                tracing::warn!("[init] Failed to load cookies from {}: {}", cookie_path, e);
                None
            }
        }
    } else {
        tracing::info!("[init] No cookie file at {}, running without Premium auth", cookie_path);
        None
    };

    // 5. Create reqwest client (shared across innertube + lounge)
    // NOTE: cookies are NOT loaded into the client — they're sent manually
    // only on WEB_REMIX /player calls to avoid poisoning IOS/other requests.
    let http_client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0")
        .build()?;

    // 5b. Fetch live YouTube Music session (INNERTUBE_CONTEXT from page)
    // This is critical: without the full 22+ field context from ytcfg,
    // WEB_REMIX /player returns UNPLAYABLE. yt-dlp does the same thing.
    let yt_session = if let Some(ref cookies) = yt_cookies {
        match cookies::fetch_music_session(&http_client, cookies).await {
            Some(session) => {
                tracing::info!("[init] YouTube Music session extracted (logged_in={})", session.logged_in);
                Some(Arc::new(session))
            }
            None => {
                tracing::warn!("[init] Failed to extract YouTube Music session — WEB_REMIX may fail");
                None
            }
        }
    } else {
        None
    };

    // 5c. Load OAuth credentials (YouTube TV device flow)
    // If logged in with a Premium account, Bearer token bypasses PoToken
    // requirement entirely — no bgutil-pot sidecar needed.
    let oauth_creds = oauth::load_credentials();
    let oauth_state = oauth::OAuthState::new(http_client.clone(), oauth_creds.clone());

    // Run login flow if --login flag is passed
    if std::env::args().any(|a| a == "--login") {
        oauth_state.device_login().await?;
        let creds = oauth_state.credentials.read().await;
        oauth::save_credentials(&creds)?;
        tracing::info!("[init] OAuth login complete, restart without --login to use");
        return Ok(());
    }

    if oauth_creds.is_logged_in() {
        tracing::info!("[init] OAuth: logged in (token expires {})", oauth_creds.expiry_date);
    } else {
        tracing::info!("[init] OAuth: not logged in (run with --login to authenticate)");
    }

    // --test <video_id>: Test OAuth /player calls without starting the full server.
    // Tests multiple clients with Bearer token and prints what comes back.
    if let Some(test_vid) = std::env::args()
        .skip_while(|a| a != "--test")
        .nth(1)
    {
        return test_oauth_player(&oauth_state, &http_client, &test_vid).await;
    }

    // 6. Generate screen_ids on first run (one per theme)
    let mut config_changed = false;
    if config.screen_id.is_empty() {
        config.screen_id = lounge::generate_screen_id(&http_client).await?;
        config_changed = true;
        tracing::info!("[init] Generated screen_id (cl): {}", config.screen_id);
    }
    if config.screen_id_m.is_empty() {
        config.screen_id_m = lounge::generate_screen_id(&http_client).await?;
        config_changed = true;
        tracing::info!("[init] Generated screen_id (m): {}", config.screen_id_m);
    }
    if config_changed {
        config.save()?;
    }

    // 7. Connect to MPD (command connection)
    let mut mpd_client = mpd::MpdClient::connect(&mpd_host, mpd_port).await?;
    mpd_client.consume("0").await?;
    mpd_client.clear().await?;
    tracing::info!("[MPD] Connected to {}:{}", mpd_host, mpd_port);

    // 8. Connect MPD idle listener (dedicated connection for real-time events)
    //
    // MPD's protocol is single-command-at-a-time per connection — you can't
    // send `idle` and `status` on the same socket. Two connections is the
    // standard pattern used by every serious MPD client (mpd2, ncmpcpp, etc.).
    let mut mpd_idle = mpd::MpdIdleClient::connect(&mpd_host, mpd_port).await?;
    tracing::info!("[MPD] Idle listener connected");

    // 9. Create channels
    //
    // Dual Lounge sessions: "cl" (YouTube) and "m" (YouTube Music).
    // Both run concurrently, each with their own long-poll + pairing channel.
    // State updates are routed to whichever session is currently active
    // (determined by which session's remoteConnected fired last).
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (player_cmd_tx, mut player_cmd_rx) = tokio::sync::mpsc::channel::<lounge::PlayerCommand>(32);

    // Player → router → active session
    let (state_tx, mut state_router_rx) = tokio::sync::mpsc::channel::<messages::OutgoingMessage>(32);
    let (state_cl_tx, state_cl_rx) = tokio::sync::mpsc::channel::<messages::OutgoingMessage>(32);
    let (state_m_tx, state_m_rx) = tokio::sync::mpsc::channel::<messages::OutgoingMessage>(32);

    // Pairing channels (one per session)
    let (pairing_cl_tx, pairing_cl_rx) = tokio::sync::mpsc::channel::<String>(8);
    let (pairing_m_tx, pairing_m_rx) = tokio::sync::mpsc::channel::<String>(8);

    // Active session tracking — router checks this to decide where state goes
    let active_theme: std::sync::Arc<std::sync::RwLock<String>> =
        std::sync::Arc::new(std::sync::RwLock::new("cl".into()));

    // State router: forwards state messages to the active session's channel
    let router_theme = active_theme.clone();
    tokio::spawn(async move {
        while let Some(msg) = state_router_rx.recv().await {
            let theme = router_theme.read().unwrap().clone();
            let tx = if theme == "m" { &state_m_tx } else { &state_cl_tx };
            if tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    // 10. Get local IP
    let local_ip = ssdp::get_local_ip()?;
    tracing::info!("[net] Local IP: {}", local_ip);

    // 11. Create shared DIAL state
    let dial_state = Arc::new(dial::DialState {
        running: std::sync::atomic::AtomicBool::new(false),
        pairing_tx: pairing_cl_tx,
        pairing_tx_m: pairing_m_tx,
        player_cmd_tx: Some(player_cmd_tx.clone()),
        device_name: device_name.clone(),
        uuid: config.uuid.clone(),
        local_ip,
        port: dial_port,
        http_client: http_client.clone(),
        ctt: std::sync::RwLock::new(None),
        yt_cookies: yt_cookies.clone(),
        yt_session: yt_session.clone(),
        oauth_state: if oauth_creds.is_logged_in() { Some(oauth_state.clone()) } else { None },
        screen_id: config.screen_id.clone(),
    });

    // 12. Spawn background tasks

    // SSDP discovery responder
    let ssdp_shutdown = shutdown_rx.clone();
    let ssdp_uuid = config.uuid.clone();
    tokio::spawn(async move {
        if let Err(e) = ssdp::run_ssdp(local_ip, dial_port, &ssdp_uuid, ssdp_shutdown).await {
            tracing::error!("[SSDP] Error: {}", e);
        }
    });

    // DIAL HTTP server
    let dial_shutdown = shutdown_rx.clone();
    let dial_state_server = dial_state.clone();
    tokio::spawn(async move {
        if let Err(e) = dial::run_dial_server(dial_port, dial_state_server, dial_shutdown).await {
            tracing::error!("[DIAL] Error: {}", e);
        }
    });

    // Lounge API — "cl" session (YouTube theme)
    let lounge_cl_shutdown = shutdown_rx.clone();
    let lounge_cl_client = http_client.clone();
    let lounge_cl_name = device_name.clone();
    let lounge_cl_uuid = config.uuid.clone();
    let lounge_cl_screen = config.screen_id.clone();
    let player_cmd_tx_cl = player_cmd_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = lounge::run_lounge(
            lounge_cl_client,
            lounge_cl_name,
            lounge_cl_uuid,
            lounge_cl_screen,
            "cl".to_string(),
            player_cmd_tx_cl,
            state_cl_rx,
            pairing_cl_rx,
            lounge_cl_shutdown,
        )
        .await
        {
            tracing::error!("[Lounge:cl] Error: {}", e);
        }
    });

    // Lounge API — "m" session (YouTube Music theme)
    let lounge_m_shutdown = shutdown_rx.clone();
    let lounge_m_client = http_client.clone();
    let lounge_m_name = device_name.clone();
    let lounge_m_uuid = uuid::Uuid::new_v4().to_string(); // separate device id per session
    let lounge_m_screen = config.screen_id_m.clone();
    tokio::spawn(async move {
        if let Err(e) = lounge::run_lounge(
            lounge_m_client,
            lounge_m_name,
            lounge_m_uuid,
            lounge_m_screen,
            "m".to_string(),
            player_cmd_tx,
            state_m_rx,
            pairing_m_rx,
            lounge_m_shutdown,
        )
        .await
        {
            tracing::error!("[Lounge:m] Error: {}", e);
        }
    });

    // Periodic memory logging (every 60s)
    let mem_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            if *mem_shutdown.borrow() {
                break;
            }
            log_memory();
        }
    });

    // 13. MPD idle listener — real-time player state change detection
    //
    // Uses MPD's `idle player` command for instant notification when a song
    // ends, just like the Node.js mpd2 library's event system. No polling.
    let (idle_tx, mut idle_rx) = tokio::sync::mpsc::channel::<()>(8);
    let idle_mpd_host = mpd_host.clone();
    let idle_mpd_port = mpd_port;
    let idle_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        loop {
            if *idle_shutdown.borrow() {
                break;
            }
            match mpd_idle.wait_player_change().await {
                Ok(()) => {
                    if idle_tx.send(()).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!("[MPD idle] {e}, reconnecting...");
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        if *idle_shutdown.borrow() {
                            return;
                        }
                        match mpd::MpdIdleClient::connect(&idle_mpd_host, idle_mpd_port).await {
                            Ok(new_idle) => {
                                mpd_idle = new_idle;
                                tracing::info!("[MPD idle] Reconnected");
                                break;
                            }
                            Err(re) => {
                                tracing::error!("[MPD idle] Reconnect failed: {re}");
                            }
                        }
                    }
                }
            }
        }
    });

    tracing::info!("[yt-mpd] \"{}\" is now discoverable", device_name);
    log_memory();

    // 14. Player command loop (runs on main thread)
    //
    // - Idle events detect song endings instantly for auto-advance
    // - Position timer reports playback progress to YouTube every 3s (seekbar sync)
    let innertube_client = http_client;
    let innertube_cookies = yt_cookies;
    let innertube_session = yt_session;
    let innertube_oauth = if oauth_creds.is_logged_in() { Some(oauth_state.clone()) } else { None };
    let mut playlist = PlaylistState::default();
    let mut play_started = false;
    let mut has_remote = false;
    let mut position_timer = tokio::time::interval(std::time::Duration::from_secs(3));

    loop {
        tokio::select! {
            // Player commands from the Lounge protocol
            cmd = player_cmd_rx.recv() => {
                let Some(cmd) = cmd else {
                    tracing::info!("[player] Command channel closed, shutting down");
                    break;
                };

                // Track remote connection state + active session
                match &cmd {
                    lounge::PlayerCommand::RemoteConnected { theme } => {
                        tracing::info!("[player] Remote connected on theme={}, ready for commands", theme);
                        has_remote = true;
                        *active_theme.write().unwrap() = theme.clone();
                        continue;
                    }
                    lounge::PlayerCommand::RemoteDisconnected { theme } => {
                        let current = active_theme.read().unwrap().clone();
                        if *theme != current {
                            tracing::debug!("[player] Ignoring disconnect from non-active theme={}", theme);
                            continue;
                        }
                        has_remote = false;
                        // Fall through to handle_command for stop/cleanup
                    }
                    _ => {}
                }

                // Ignore stale playback commands replayed before any remote connects.
                // These come from the initial bind and would pollute YouTube's
                // cached state with duration=0 (innertube can't resolve without ctt).
                if !has_remote && matches!(&cmd,
                    lounge::PlayerCommand::SetPlaylist { .. } |
                    lounge::PlayerCommand::SetVideo { .. }
                ) {
                    tracing::info!("[player] Ignoring stale command (no remote connected yet)");
                    continue;
                }

                if let Err(e) = handle_command(
                    cmd,
                    &mut mpd_client,
                    &innertube_client,
                    &state_tx,
                    &mut playlist,
                    &mut play_started,
                    &dial_state,
                    innertube_cookies.as_ref().map(|c| c.as_ref()),
                    innertube_session.as_ref().map(|s| s.as_ref()),
                    innertube_oauth.as_ref(),
                ).await {
                    tracing::error!("[player] Error handling command: {}", e);
                }
            }

            // MPD player state changed — instant song-end detection
            Some(()) = idle_rx.recv() => {
                // Drain any extra queued events (e.g. from clear+add+play sequence)
                while idle_rx.try_recv().is_ok() {}

                if play_started {
                    if let Ok(status) = mpd_client.status().await {
                        if status.state == "stop" {
                            play_started = false;

                            // Tell YouTube the current track has ended BEFORE
                            // attempting auto-advance. If InnerTube fails
                            // (LOGIN_REQUIRED), YouTube already has the ended
                            // signal and will send a recovery setPlaylist fast.
                            let duration = playlist.current_duration as f64;
                            let ended_vid = current_video_id(&playlist);
                            tracing::info!(
                                "[player] Song ended: {} (index {})",
                                ended_vid, playlist.current_index
                            );
                            // Song ended: send OnStateChange only (PLAYING→STOPPED).
                            // Reference does NOT send NowPlaying when prev was PLAYING.
                            let dur_str = messages::fmt_time(duration);
                            playlist.last_state = messages::PlayerState::Stopped;
                            state_tx.send(
                                messages::OutgoingMessage::new("onStateChange")
                                    .with_arg("state", "0")
                                    .with_arg("currentTime", &dur_str)
                                    .with_arg("duration", &dur_str)
                                    .with_arg("loadedTime", &dur_str)
                                    .with_arg("seekableStartTime", "0")
                                    .with_arg("seekableEndTime", &dur_str)
                                    .with_arg("cpn", &playlist.cpn),
                            ).await.ok();

                            if playlist.current_index + 1 < playlist.video_ids.len() {
                                playlist.current_index += 1;
                                let video_id = playlist.video_ids[playlist.current_index].clone();
                                tracing::info!(
                                    "[player] Advancing to index {} ({})",
                                    playlist.current_index, video_id
                                );

                                // Track change — ensure NowPlaying is sent
                                playlist.last_state = messages::PlayerState::Idle;

                                match play_video(
                                    &mut mpd_client, &innertube_client, &state_tx,
                                    &mut playlist, &video_id, 0.0, innertube_cookies.as_ref().map(|c| c.as_ref()),
                                    innertube_session.as_ref().map(|s| s.as_ref()),
                                    innertube_oauth.as_ref(),
                                ).await {
                                    Ok(true) => play_started = true,
                                    Ok(false) => {
                                        tracing::warn!(
                                            "[player] Auto-advance: no stream for {}, waiting for YouTube recovery",
                                            video_id
                                        );
                                    }
                                    Err(e) => {
                                        tracing::error!("[player] Auto-advance error: {}", e);
                                    }
                                }
                            } else {
                                tracing::info!("[player] Playlist finished");
                            }
                        }
                    }
                }
            }

            // Position reporting + keepalive (every 3s)
            //
            // Always queries MPD status to keep the command connection alive
            // (MPD closes idle connections after 60s). Only reports to YouTube
            // when actively playing.
            _ = position_timer.tick() => {
                match mpd_client.status().await {
                    Ok(status) => {
                        if play_started && !playlist.video_ids.is_empty() {
                            if status.state == "play" {
                                let elapsed = status.elapsed.unwrap_or(0.0);
                                let duration = playlist.current_duration as f64;
                                tracing::info!(
                                    "[tick] pos={} dur={} vid={}",
                                    messages::fmt_time(elapsed),
                                    messages::fmt_time(duration),
                                    current_video_id(&playlist),
                                );
                                // Position tick: only send onStateChange, NOT nowPlaying.
                                state_tx.send(
                                    messages::OutgoingMessage::new("onStateChange")
                                        .with_arg("state", "1")
                                        .with_arg("currentTime", messages::fmt_time(elapsed))
                                        .with_arg("duration", messages::fmt_time(duration))
                                        .with_arg("loadedTime", messages::fmt_time(duration))
                                        .with_arg("seekableStartTime", "0")
                                        .with_arg("seekableEndTime", messages::fmt_time(duration))
                                        .with_arg("cpn", &playlist.cpn),
                                ).await.ok();
                            } else {
                                tracing::info!(
                                    "[tick] mpd_state={} (not play), skipping tick",
                                    status.state
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("[tick] mpd.status() FAILED: {}", e);
                    }
                }
            }

            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Received SIGINT, shutting down...");
                break;
            }
        }
    }

    let _ = shutdown_tx.send(true);
    tracing::info!("Shutdown complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Command dispatch
// ---------------------------------------------------------------------------

type CmdResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

async fn handle_command(
    cmd: lounge::PlayerCommand,
    mpd: &mut mpd::MpdClient,
    http: &reqwest::Client,
    state_tx: &tokio::sync::mpsc::Sender<messages::OutgoingMessage>,
    playlist: &mut PlaylistState,
    play_started: &mut bool,
    dial_state: &Arc<dial::DialState>,
    yt_cookies: Option<&cookies::YtCookies>,
    yt_session: Option<&cookies::YtMusicSession>,
    oauth: Option<&Arc<oauth::OAuthState>>,
) -> CmdResult {
    use lounge::PlayerCommand;

    match cmd {
        PlayerCommand::SetPlaylist {
            video_id,
            video_ids,
            list_id,
            index,
            ctt,
            params,
            current_time,
        } => {
            tracing::info!(
                "[player] SetPlaylist: {} videos, starting at index {} ({})",
                video_ids.len(),
                index,
                video_id
            );

            playlist.video_ids = video_ids;
            playlist.current_index = index;
            playlist.list_id = list_id;
            playlist.ctt = ctt.clone();
            playlist.params = params;
            playlist.current_duration = 0; // Reset — will be set by play_video after InnerTube resolve
            playlist.current_title.clear();
            // New track — reset last_state so NowPlaying is sent (nowPlayingChanged).
            // Without this, Playing→Loading would suppress NowPlaying since prev==Playing.
            playlist.last_state = messages::PlayerState::Idle;

            // Store ctt in shared DIAL state so the SABR handler can use it.
            if let Some(ref token) = ctt {
                tracing::info!("[player] ctt received: {}", token);
            }
            *dial_state.ctt.write().unwrap() = ctt;

            *play_started = false;
            *play_started = play_video(mpd, http, state_tx, playlist, &video_id, current_time, yt_cookies, yt_session, oauth).await?;
        }

        PlayerCommand::SetVideo {
            video_id,
            ctt,
            current_time,
        } => {
            tracing::info!("[player] SetVideo: {}", video_id);

            playlist.video_ids = vec![video_id.clone()];
            playlist.current_index = 0;
            playlist.list_id.clear();
            playlist.ctt = ctt.clone();
            playlist.current_duration = 0;
            playlist.current_title.clear();
            playlist.last_state = messages::PlayerState::Idle; // New track — ensure NowPlaying is sent

            if ctt.is_some() {
                tracing::info!("[player] ctt received from cast session");
            }
            *dial_state.ctt.write().unwrap() = ctt;

            *play_started = false;
            *play_started = play_video(mpd, http, state_tx, playlist, &video_id, current_time, yt_cookies, yt_session, oauth).await?;
        }

        PlayerCommand::Play => {
            tracing::info!("[player] Play (resume)");
            mpd.resume().await?;
            *play_started = true;
            send_state(mpd, state_tx, playlist, messages::PlayerState::Playing).await?;
        }

        PlayerCommand::Pause => {
            tracing::info!("[player] Pause");
            mpd.pause().await?;
            // Don't clear play_started — we still want auto-advance after resume
            send_state(mpd, state_tx, playlist, messages::PlayerState::Paused).await?;
        }

        PlayerCommand::SeekTo { position } => {
            tracing::info!("[player] SeekTo: {}s", position);
            match mpd.seekcur(position).await {
                Ok(()) => {}
                Err(e) => {
                    // SABR streams aren't seekable until fully buffered.
                    // Don't crash — report current position so phone doesn't hang.
                    tracing::warn!("[player] seek failed (non-fatal): {e}");
                }
            }
            // Always respond with current state so the phone doesn't spin
            send_state(mpd, state_tx, playlist, messages::PlayerState::Playing).await?;
        }

        PlayerCommand::SetVolume { volume } => {
            tracing::info!("[player] SetVolume: {}", volume);
            mpd.setvol(volume).await?;
            send_volume(state_tx, volume).await?;
        }

        PlayerCommand::GetVolume => {
            let status = mpd.status().await?;
            tracing::info!("[player] GetVolume: {}", status.volume);
            send_volume(state_tx, status.volume).await?;
        }

        PlayerCommand::GetNowPlaying => {
            tracing::info!("[player] GetNowPlaying");
            // Query response — always include NowPlaying regardless of state.
            // Temporarily reset last_state so send_state includes NowPlaying.
            playlist.last_state = messages::PlayerState::Idle;
            let status = mpd.status().await?;
            let state = mpd_state_to_player_state(&status.state);
            send_state(mpd, state_tx, playlist, state).await?;
        }

        PlayerCommand::GetPlaylist => {
            tracing::info!("[player] GetPlaylist");
            state_tx
                .send(
                    messages::OutgoingMessage::new("nowPlayingPlaylist")
                        .with_arg("videoIds", playlist.video_ids.join(","))
                        .with_arg("currentIndex", playlist.current_index.to_string())
                        .with_arg("listId", &playlist.list_id),
                )
                .await
                .ok();
        }

        PlayerCommand::Stop => {
            tracing::info!("[player] Stop");
            *play_started = false;
            mpd.stop().await?;
            mpd.clear().await?;
            playlist.video_ids.clear();
            playlist.current_index = 0;
            playlist.list_id.clear();
            playlist.current_title.clear();
            playlist.current_duration = 0;
            send_state(mpd, state_tx, playlist, messages::PlayerState::Stopped).await?;
        }

        PlayerCommand::UpdatePlaylist {
            video_ids,
            list_id,
            current_index,
        } => {
            tracing::info!(
                "[player] UpdatePlaylist: {} videos (was {})",
                video_ids.len(),
                playlist.video_ids.len()
            );

            // Find current video in the new list to maintain position
            let current_vid = current_video_id(playlist);
            let new_index = if let Some(idx) = current_index {
                idx.min(video_ids.len().saturating_sub(1))
            } else {
                // Try to find current video in new list
                video_ids.iter().position(|id| id == &current_vid).unwrap_or(0)
            };

            playlist.video_ids = video_ids;
            playlist.current_index = new_index;
            if !list_id.is_empty() {
                playlist.list_id = list_id;
            }

            // Update nav buttons for the new playlist
            let has_prev = playlist.current_index > 0;
            let has_next = playlist.current_index + 1 < playlist.video_ids.len();
            state_tx
                .send(
                    messages::OutgoingMessage::new("onHasPreviousNextChanged")
                        .with_arg("hasPrevious", if has_prev { "true" } else { "false" })
                        .with_arg("hasNext", if has_next { "true" } else { "false" }),
                )
                .await
                .ok();

            // Send updated playlist info
            state_tx
                .send(
                    messages::OutgoingMessage::new("nowPlayingPlaylist")
                        .with_arg("videoIds", playlist.video_ids.join(","))
                        .with_arg("currentIndex", playlist.current_index.to_string())
                        .with_arg("listId", &playlist.list_id),
                )
                .await
                .ok();
        }

        PlayerCommand::Previous => {
            if playlist.current_index > 0 {
                playlist.current_index -= 1;
                let video_id = playlist.video_ids[playlist.current_index].clone();
                tracing::info!("[player] Previous -> index {} ({})", playlist.current_index, video_id);
                playlist.last_state = messages::PlayerState::Idle; // Track change
                *play_started = false;
                *play_started = play_video(mpd, http, state_tx, playlist, &video_id, 0.0, yt_cookies, yt_session, oauth).await?;
            } else {
                tracing::info!("[player] Previous: already at start");
            }
        }

        PlayerCommand::Next => {
            if playlist.current_index + 1 < playlist.video_ids.len() {
                playlist.current_index += 1;
                let video_id = playlist.video_ids[playlist.current_index].clone();
                tracing::info!("[player] Next -> index {} ({})", playlist.current_index, video_id);
                playlist.last_state = messages::PlayerState::Idle; // Track change
                *play_started = false;
                *play_started = play_video(mpd, http, state_tx, playlist, &video_id, 0.0, yt_cookies, yt_session, oauth).await?;
            } else {
                tracing::info!("[player] Next: end of playlist");
                *play_started = false;
                mpd.stop().await?;
                mpd.clear().await?;
                send_state(mpd, state_tx, playlist, messages::PlayerState::Stopped).await?;
            }
        }

        PlayerCommand::RemoteConnected { .. } => {
            // Handled in the main select! loop before handle_command
        }

        PlayerCommand::RemoteDisconnected { .. } => {
            tracing::info!("[player] Remote disconnected, stopping playback");
            *play_started = false;
            mpd.stop().await?;
            mpd.clear().await?;
            playlist.video_ids.clear();
            playlist.current_index = 0;
            playlist.list_id.clear();
            playlist.current_title.clear();
            playlist.current_duration = 0;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn current_video_id(playlist: &PlaylistState) -> String {
    playlist
        .video_ids
        .get(playlist.current_index)
        .cloned()
        .unwrap_or_default()
}

/// Resolve an audio stream via InnerTube and play it through MPD.
///
/// Returns `true` if playback started, `false` if no stream was available.
/// If InnerTube can't resolve the track, reports error state to YouTube
/// and returns `false`. Does NOT auto-skip — let YouTube handle navigation.
async fn play_video(
    mpd: &mut mpd::MpdClient,
    http: &reqwest::Client,
    state_tx: &tokio::sync::mpsc::Sender<messages::OutgoingMessage>,
    playlist: &mut PlaylistState,
    video_id: &str,
    start_position: f64,
    yt_cookies: Option<&cookies::YtCookies>,
    yt_session: Option<&cookies::YtMusicSession>,
    oauth: Option<&Arc<oauth::OAuthState>>,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    // Tell YouTube we're loading — app shows spinner immediately
    send_state(mpd, state_tx, playlist, messages::PlayerState::Loading).await?;

    // Get OAuth Bearer token if available (auto-refreshes if expired)
    let oauth_token = if let Some(os) = oauth {
        os.get_access_token().await
    } else {
        None
    };

    let stream = innertube::resolve_stream(
        http,
        video_id,
        playlist.ctt.as_deref(),
        if playlist.list_id.is_empty() {
            None
        } else {
            Some(playlist.list_id.as_str())
        },
        yt_cookies,
        yt_session,
        oauth_token.as_deref(),
    )
    .await?;

    match stream {
        Some(info) => {
            tracing::info!(
                "[player] Playing: {} [{}s] ({})",
                info.title,
                info.duration,
                video_id
            );

            playlist.current_title = info.title;
            playlist.current_duration = info.duration;

            mpd.clear().await?;
            mpd.add(&info.stream_url).await?;
            mpd.play().await?;

            if start_position > 0.0 {
                // Non-fatal: MPD may not have buffered enough data to seek yet
                if let Err(e) = mpd.seekcur(start_position).await {
                    tracing::warn!("[player] seek to {start_position}s failed (non-fatal): {e}");
                }
            }

            send_state(mpd, state_tx, playlist, messages::PlayerState::Playing).await?;
            Ok(true)
        }
        None => {
            tracing::error!("[player] No stream for {video_id}, reporting error to YouTube");
            // Tell YouTube we couldn't play it — phone will handle skip/retry
            send_state(mpd, state_tx, playlist, messages::PlayerState::Stopped).await?;
            Ok(false)
        }
    }
}

fn mpd_state_to_player_state(state: &str) -> messages::PlayerState {
    match state {
        "play" => messages::PlayerState::Playing,
        "pause" => messages::PlayerState::Paused,
        "stop" => messages::PlayerState::Stopped,
        _ => messages::PlayerState::Idle,
    }
}

/// Send state messages to the Lounge session, matching yt-cast-receiver's
/// conditional logic for when NowPlaying is included.
///
/// Reference rules (YouTubeApp.ts #handlePlayerStateEvent):
/// - OnStateChange: ALWAYS sent on status or position change
/// - NowPlaying + OnHasPreviousNextChanged: ONLY sent when:
///   (a) video/queue changed (nowPlayingChanged), OR
///   (b) status changed AND previous status was NOT Playing
///
/// This means Pause (Playing→Paused) and Stop (Playing→Stopped) only send
/// OnStateChange — NOT NowPlaying. Sending extra NowPlaying confuses the app.
async fn send_state(
    mpd: &mut mpd::MpdClient,
    state_tx: &tokio::sync::mpsc::Sender<messages::OutgoingMessage>,
    playlist: &mut PlaylistState,
    state: messages::PlayerState,
) -> CmdResult {
    let status = mpd.status().await?;
    let elapsed = status.elapsed.unwrap_or(0.0);
    let duration = playlist.current_duration as f64;
    let vid = current_video_id(playlist);

    // loadedTime: report fully buffered when playing/paused/loading, 0 otherwise
    let loaded_time = match state {
        messages::PlayerState::Playing
        | messages::PlayerState::Paused
        | messages::PlayerState::Loading => duration,
        _ => 0.0,
    };

    let prev = playlist.last_state;
    let status_changed = state != prev;

    // Reference condition: send NowPlaying when status changed AND
    // previous was NOT Playing. This avoids extra NowPlaying on
    // Pause (Playing→Paused) and Stop (Playing→Stopped).
    let send_now_playing = status_changed && prev != messages::PlayerState::Playing;

    tracing::info!(
        "[state] -> state={} currentTime={:.1} duration={:.1} vid={} cpn={} nowPlaying={}",
        state.as_str(), elapsed, duration, &vid, &playlist.cpn, send_now_playing
    );

    // Update last_state for next comparison
    playlist.last_state = state;

    // onStateChange — ALWAYS sent
    state_tx
        .send(
            messages::OutgoingMessage::new("onStateChange")
                .with_arg("state", state.as_str())
                .with_arg("currentTime", messages::fmt_time(elapsed))
                .with_arg("duration", messages::fmt_time(duration))
                .with_arg("loadedTime", messages::fmt_time(loaded_time))
                .with_arg("seekableStartTime", "0")
                .with_arg("seekableEndTime", messages::fmt_time(duration))
                .with_arg("cpn", &playlist.cpn),
        )
        .await
        .ok();

    // nowPlaying + nav — only when condition matches reference
    if send_now_playing {
        let mut np = messages::OutgoingMessage::new("nowPlaying")
            .with_arg("currentTime", messages::fmt_time(elapsed))
            .with_arg("duration", messages::fmt_time(duration))
            .with_arg("cpn", &playlist.cpn)
            .with_arg("loadedTime", messages::fmt_time(loaded_time))
            .with_arg("videoId", &vid)
            .with_arg("state", state.as_str())
            .with_arg("seekableStartTime", "0")
            .with_arg("seekableEndTime", messages::fmt_time(duration));

        if !playlist.list_id.is_empty() {
            np = np.with_arg("listId", &playlist.list_id);
        }
        np = np.with_arg("currentIndex", playlist.current_index.to_string());
        if let Some(ref ctt) = playlist.ctt {
            np = np.with_arg("ctt", ctt);
        }
        if let Some(ref params) = playlist.params {
            np = np.with_arg("params", params);
        }

        state_tx.send(np).await.ok();

        // Tell phone whether next/previous buttons should be enabled
        let has_prev = playlist.current_index > 0;
        let has_next = playlist.current_index + 1 < playlist.video_ids.len();
        state_tx
            .send(
                messages::OutgoingMessage::new("onHasPreviousNextChanged")
                    .with_arg("hasPrevious", if has_prev { "true" } else { "false" })
                    .with_arg("hasNext", if has_next { "true" } else { "false" }),
            )
            .await
            .ok();
    }

    Ok(())
}

async fn send_volume(
    state_tx: &tokio::sync::mpsc::Sender<messages::OutgoingMessage>,
    volume: u32,
) -> CmdResult {
    state_tx
        .send(
            messages::OutgoingMessage::new("onVolumeChanged")
                .with_arg("volume", volume.to_string())
                .with_arg("muted", if volume == 0 { "true" } else { "false" }),
        )
        .await
        .ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// --test mode: probe /player API with OAuth Bearer token
// ---------------------------------------------------------------------------

async fn test_oauth_player(
    oauth: &std::sync::Arc<oauth::OAuthState>,
    http: &reqwest::Client,
    video_id: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("\n=== OAuth /player Test ===");
    println!("Video ID: {}", video_id);

    // 1. Get access token (login if needed)
    let token = match oauth.get_access_token().await {
        Some(t) => {
            println!("Bearer token: {}...{}", &t[..8.min(t.len())], &t[t.len().saturating_sub(4)..]);
            t
        }
        None => {
            println!("Not logged in. Run with --login first.");
            return Ok(());
        }
    };

    let visitor_data = oauth::generate_visitor_data();
    println!("Visitor data: {}", &visitor_data[..20.min(visitor_data.len())]);

    // 2. Test each client
    let clients: &[(&str, &str, &str, i32, &str)] = &[
        // (name, client_version, user_agent, client_name_id, player_url)
        (
            "WEB_REMIX",
            "1.20260114.03.00",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
            67,
            "https://music.youtube.com/youtubei/v1/player?prettyPrint=false",
        ),
        (
            "TVHTML5",
            "7.20250219.16.00",
            "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/Version",
            7,
            "https://www.youtube.com/youtubei/v1/player?prettyPrint=false",
        ),
        (
            "IOS",
            "19.45.4",
            "com.google.ios.youtube/19.45.4 (iPhone16,2; U; CPU iOS 18_1_0 like Mac OS X;)",
            5,
            "https://www.youtube.com/youtubei/v1/player?prettyPrint=false",
        ),
    ];

    for (name, version, ua, name_id, url) in clients {
        println!("\n--- {} (id={}) ---", name, name_id);

        let mut body = serde_json::json!({
            "videoId": video_id,
            "context": {
                "client": {
                    "clientName": name,
                    "clientVersion": version,
                    "hl": "en",
                    "gl": "US",
                    "visitorData": visitor_data,
                }
            },
            "contentCheckOk": true,
            "racyCheckOk": true,
        });

        // Add device fields for mobile clients
        if *name == "IOS" {
            body["context"]["client"]["deviceMake"] = "Apple".into();
            body["context"]["client"]["deviceModel"] = "iPhone16,2".into();
            body["context"]["client"]["osName"] = "iPhone".into();
            body["context"]["client"]["osVersion"] = "18.1.0.22B83".into();
        }

        let mut req = http
            .post(*url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", token))
            .header("X-Goog-Visitor-Id", &visitor_data)
            .header("User-Agent", *ua);

        if *name == "WEB_REMIX" {
            req = req
                .header("Origin", "https://music.youtube.com")
                .header("Referer", "https://music.youtube.com/");
        }

        let resp = req.json(&body).send().await?;
        let status_code = resp.status();
        println!("HTTP: {}", status_code);

        if !status_code.is_success() {
            let text = resp.text().await.unwrap_or_default();
            println!("Error body: {}", &text[..text.len().min(200)]);
            continue;
        }

        let json: serde_json::Value = resp.json().await?;

        // Playability
        let play_status = json["playabilityStatus"]["status"]
            .as_str()
            .unwrap_or("MISSING");
        let play_reason = json["playabilityStatus"]["reason"]
            .as_str()
            .unwrap_or("");
        println!("Playability: {} {}", play_status, play_reason);

        if play_status != "OK" {
            continue;
        }

        // Title + duration
        let title = json["videoDetails"]["title"].as_str().unwrap_or("?");
        let duration = json["videoDetails"]["lengthSeconds"].as_str().unwrap_or("?");
        println!("Title: {} [{}s]", title, duration);

        // Streaming data
        let sd = &json["streamingData"];
        if sd.is_null() {
            println!("streamingData: MISSING");
            continue;
        }

        // SABR URL
        if let Some(sabr_url) = sd["serverAbrStreamingUrl"].as_str() {
            println!("SABR URL: {}...{}", &sabr_url[..60.min(sabr_url.len())], &sabr_url[sabr_url.len().saturating_sub(30)..]);
        } else {
            println!("SABR URL: none");
        }

        // Adaptive formats (direct URLs — Path A)
        if let Some(formats) = sd["adaptiveFormats"].as_array() {
            let audio_formats: Vec<_> = formats
                .iter()
                .filter(|f| {
                    f["mimeType"]
                        .as_str()
                        .map(|m| m.starts_with("audio/"))
                        .unwrap_or(false)
                })
                .collect();

            println!("Adaptive formats: {} total, {} audio", formats.len(), audio_formats.len());

            for f in &audio_formats {
                let mime = f["mimeType"].as_str().unwrap_or("?");
                let bitrate = f["bitrate"].as_u64().unwrap_or(0);
                let has_url = f["url"].as_str().is_some();
                let has_cipher = f["signatureCipher"].as_str().is_some();
                let drm = f["drmFamilies"].as_array().map(|a| a.len()).unwrap_or(0);
                let itag = f["itag"].as_u64().unwrap_or(0);

                println!(
                    "  itag={} {} {}kbps url={} cipher={} drm={}",
                    itag,
                    mime,
                    bitrate / 1000,
                    if has_url { "YES" } else { "no" },
                    if has_cipher { "YES" } else { "no" },
                    drm,
                );
            }
        } else {
            println!("Adaptive formats: none (SABR-only?)");
        }

        // Check for po_token hints in the response
        if let Some(sid) = json["serviceIntegrityDimensions"].as_object() {
            println!("serviceIntegrityDimensions: {:?}", sid.keys().collect::<Vec<_>>());
        }

        // attestation info
        if let Some(att) = json["attestation"].as_object() {
            if let Some(challenge) = att.get("playerAttestationRenderer") {
                let has_challenge = challenge.get("challenge").is_some();
                println!("Attestation challenge present: {}", has_challenge);
            }
        }
    }

    // 3. SABR probe: test with multiple /player clients and SABR auth combos.
    //    IOS without auth = baseline (works on Pi for 60s).
    //    Then try adding Bearer to SABR to see if Premium changes status.
    let sabr_clients: &[(&str, &str, &str, i32, bool)] = &[
        // (name, version, user_agent, name_id, use_bearer_for_player)
        ("IOS", "19.45.4", "com.google.ios.youtube/19.45.4 (iPhone16,2; U; CPU iOS 18_1_0 like Mac OS X;)", 5, false),
        ("TVHTML5", "7.20250219.16.00", "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/Version", 7, true),
    ];

    for (cname, cversion, cua, cname_id, player_bearer) in sabr_clients {
        println!("\n--- SABR Probe ({} {}) ---", cname, if *player_bearer { "+ Bearer" } else { "no auth" });
        let mut body = serde_json::json!({
            "videoId": video_id,
            "context": {
                "client": {
                    "clientName": cname,
                    "clientVersion": cversion,
                    "hl": "en",
                    "gl": "US",
                    "visitorData": visitor_data,
                }
            },
            "contentCheckOk": true,
            "racyCheckOk": true,
        });
        if *cname == "IOS" {
            body["context"]["client"]["deviceMake"] = "Apple".into();
            body["context"]["client"]["deviceModel"] = "iPhone16,2".into();
            body["context"]["client"]["osName"] = "iPhone".into();
            body["context"]["client"]["osVersion"] = "18.1.0.22B83".into();
        }

        let mut player_req = http
            .post("https://www.youtube.com/youtubei/v1/player?prettyPrint=false")
            .header("Content-Type", "application/json")
            .header("X-Goog-Visitor-Id", &visitor_data)
            .header("User-Agent", *cua);
        if *player_bearer {
            player_req = player_req.header("Authorization", format!("Bearer {}", token));
        }
        let resp = player_req.json(&body).send().await?;

        if !resp.status().is_success() {
            println!("SABR probe: /player failed ({})", resp.status());
        } else {
            let json: serde_json::Value = resp.json().await?;
            let sabr_url = json["streamingData"]["serverAbrStreamingUrl"]
                .as_str()
                .unwrap_or("");

            if sabr_url.is_empty() {
                println!("SABR probe: no serverAbrStreamingUrl");
            } else {
                // Get format info for the SABR request
                let formats = json["streamingData"]["adaptiveFormats"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();

                // Pick an audio format (prefer opus)
                let audio_fmt = formats.iter()
                    .filter(|f| f["mimeType"].as_str().map(|m| m.starts_with("audio/")).unwrap_or(false))
                    .max_by_key(|f| f["bitrate"].as_u64().unwrap_or(0));
                // Pick a video format (lowest bitrate for discard trick)
                let video_fmt = formats.iter()
                    .filter(|f| f["mimeType"].as_str().map(|m| m.starts_with("video/")).unwrap_or(false))
                    .min_by_key(|f| f["bitrate"].as_u64().unwrap_or(u64::MAX));

                if let (Some(af), Some(vf)) = (audio_fmt, video_fmt) {
                    println!("Audio: itag={} lastMod={} xtags={}", af["itag"], af["lastModified"], af["xtags"]);
                    println!("Video: itag={} lastMod={} xtags={}", vf["itag"], vf["lastModified"], vf["xtags"]);

                    // Build minimal SABR protobuf request
                    use prost::Message;

                    let audio_format_id = sabr::proto::misc::FormatId {
                        itag: af["itag"].as_i64().map(|v| v as i32),
                        last_modified: af["lastModified"].as_str().and_then(|s| s.parse().ok()),
                        xtags: af["xtags"].as_str().map(|s| s.to_owned()),
                    };
                    let video_format_id = sabr::proto::misc::FormatId {
                        itag: vf["itag"].as_i64().map(|v| v as i32),
                        last_modified: vf["lastModified"].as_str().and_then(|s| s.parse().ok()),
                        xtags: vf["xtags"].as_str().map(|s| s.to_owned()),
                    };

                    let client_info = sabr::proto::vs::streamer_context::ClientInfo {
                        client_name: Some(*cname_id),
                        client_version: Some(cversion.to_string()),
                        ..Default::default()
                    };

                    let streamer_context = sabr::proto::vs::StreamerContext {
                        client_info: Some(client_info),
                        po_token: None, // No PoToken — testing if Premium bypasses it
                        ..Default::default()
                    };

                    // Video: fully buffered (discard trick)
                    let video_range = sabr::proto::vs::BufferedRange {
                        format_id: video_format_id.clone(),
                        start_time_ms: 0,
                        duration_ms: i32::MAX as i64,
                        start_segment_index: i32::MAX,
                        end_segment_index: i32::MAX,
                        time_range: Some(sabr::proto::vs::TimeRange {
                            start_ticks: Some(0),
                            duration_ticks: Some(i32::MAX as i64),
                            timescale: Some(1000),
                        }),
                        ..Default::default()
                    };

                    let client_abr_state = sabr::proto::vs::ClientAbrState {
                        enabled_track_types_bitfield: Some(1),
                        playback_rate: Some(1.0),
                        bandwidth_estimate: Some(5_000_000),
                        player_time_ms: Some(0),
                        visibility: Some(1),
                        player_state: Some(1),
                        ..Default::default()
                    };

                    // Get ustreamer config
                    let ustreamer_b64 = json["playerConfig"]["mediaCommonConfig"]
                        ["mediaUstreamerRequestConfig"]["videoPlaybackUstreamerConfig"]
                        .as_str()
                        .unwrap_or("");
                    let ustreamer_config = if ustreamer_b64.is_empty() {
                        Vec::new()
                    } else {
                        use base64::Engine;
                        use base64::engine::{GeneralPurpose, GeneralPurposeConfig, DecodePaddingMode};
                        const URL_SAFE_LENIENT: GeneralPurpose = GeneralPurpose::new(
                            &base64::alphabet::URL_SAFE,
                            GeneralPurposeConfig::new()
                                .with_decode_padding_mode(DecodePaddingMode::Indifferent),
                        );
                        URL_SAFE_LENIENT.decode(ustreamer_b64).unwrap_or_default()
                    };

                    // First SABR request: only video in selected_format_ids.
                    // Audio goes in preferred_audio_format_ids only.
                    // The CDN sends FORMAT_INITIALIZATION_METADATA first,
                    // and audio is added to selected_format_ids in subsequent requests.
                    let sabr_req = sabr::proto::vs::VideoPlaybackAbrRequest {
                        client_abr_state: Some(client_abr_state),
                        selected_format_ids: vec![video_format_id.clone()],
                        buffered_ranges: vec![video_range],
                        video_playback_ustreamer_config: if ustreamer_config.is_empty() {
                            None
                        } else {
                            Some(ustreamer_config)
                        },
                        streamer_context: Some(streamer_context),
                        preferred_audio_format_ids: vec![audio_format_id],
                        preferred_video_format_ids: vec![video_format_id],
                        ..Default::default()
                    };

                    let mut proto_buf = Vec::new();
                    sabr_req.encode(&mut proto_buf)?;

                    let sabr_full_url = format!("{}&rn=0", sabr_url);
                    println!("SABR POST to: {}...{}", &sabr_full_url[..60.min(sabr_full_url.len())], &sabr_full_url[sabr_full_url.len().saturating_sub(20)..]);

                    // Try SABR: no auth first (baseline), then with Bearer (Premium test)
                    let sabr_attempts: &[(&str, bool)] = &[
                        ("No auth (baseline)", false),
                        ("+ Bearer token (Premium?)", true),
                    ];

                    for (label, use_bearer) in sabr_attempts {
                        println!("\n  SABR attempt: {}", label);
                        let mut sabr_req_builder = http
                            .post(&sabr_full_url)
                            .header("Content-Type", "application/x-protobuf")
                            .header("Accept", "application/vnd.yt-ump")
                            .header("Accept-Encoding", "identity")
                            .header("User-Agent", "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/Version");

                        if *use_bearer {
                            sabr_req_builder = sabr_req_builder
                                .header("Authorization", format!("Bearer {}", token));
                        }

                        let sabr_resp = sabr_req_builder
                            .body(proto_buf.clone())
                            .send()
                            .await?;

                    println!("  HTTP: {}", sabr_resp.status());

                    if sabr_resp.status().is_success() {
                        let sabr_bytes = sabr_resp.bytes().await?;
                        println!("  Response: {} bytes", sabr_bytes.len());

                        // Parse UMP response using the real YouTube varint format
                        let mut parser = sabr::ump::UmpParser::new();
                        parser.push(&sabr_bytes);
                        while let Some(part) = parser.next_part() {
                            let part_type = part.part_type;
                            let part_data = &part.data;

                            let type_name = match part_type {
                                20 => "MEDIA_HEADER",
                                21 => "MEDIA",
                                22 => "MEDIA_END",
                                35 => "NEXT_REQUEST_POLICY",
                                42 => "FORMAT_INIT_METADATA",
                                43 => "SABR_REDIRECT",
                                44 => "SABR_ERROR",
                                51 => "USTREAMER_VIDEO_FORMAT_DATA",
                                57 => "SABR_CONTEXT_UPDATE",
                                58 => "STREAM_PROTECTION_STATUS",
                                59 => "SABR_CONTEXT_SENDING_POLICY",
                                _ => "OTHER",
                            };
                            println!("  UMP[{} {}] {} bytes", part_type, type_name, part_data.len());

                            // Decode SABR_ERROR to see what went wrong
                            if part_type == 44 {
                                // Print hex for debugging
                                let hex: String = part_data.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ");
                                println!("    SABR_ERROR hex: {}", hex);
                                // Try protobuf decode: SabrError has fields like error_code, etc.
                                if let Ok(err) = <sabr::proto::vs::SabrError as prost::Message>::decode(part_data.as_slice()) {
                                    println!("    SABR_ERROR decoded: {:?}", err);
                                }
                            }

                            if part_type == sabr::ump::STREAM_PROTECTION_STATUS {
                                if let Ok(sps) = <sabr::proto::vs::StreamProtectionStatus as prost::Message>::decode(part_data.as_slice()) {
                                    let status = sps.status.unwrap_or(-1);
                                    match status {
                                        1 => println!("  >>> stream_protection_status = 1 (OK!) — PREMIUM BYPASS WORKS! <<<"),
                                        2 => println!("  >>> stream_protection_status = 2 (GRACE PERIOD ~60s) — no Premium bypass <<<"),
                                        3 => println!("  >>> stream_protection_status = 3 (HARD BLOCK) <<<"),
                                        other => println!("  >>> stream_protection_status = {} (unknown) <<<", other),
                                    }
                                }
                            }
                        }
                    } else {
                        let err_text = sabr_resp.text().await.unwrap_or_default();
                        println!("  Error: {}", &err_text[..err_text.len().min(200)]);
                    }
                    } // end sabr_attempts loop
                } else {
                    println!("SABR probe: could not find audio+video formats");
                }
            }
        }
    } // end sabr_clients loop

    // Save credentials (token may have been refreshed)
    let creds = oauth.credentials.read().await;
    oauth::save_credentials(&creds)?;

    println!("\n=== Test Complete ===");
    Ok(())
}

