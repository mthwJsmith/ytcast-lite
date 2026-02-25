mod config;
mod dial;
mod innertube;
mod lounge;
mod messages;
mod mpd;
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
    /// Current track title (from InnerTube).
    current_title: String,
    /// Current track duration in seconds.
    current_duration: u64,
}

impl Default for PlaylistState {
    fn default() -> Self {
        Self {
            video_ids: Vec::new(),
            current_index: 0,
            list_id: String::new(),
            ctt: None,
            current_title: String::new(),
            current_duration: 0,
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

    // 4. Create reqwest client (shared across innertube + lounge)
    let http_client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0")
        .build()?;

    // 5. Generate screen_id on first run
    if config.screen_id.is_empty() {
        config.screen_id = lounge::generate_screen_id(&http_client).await?;
        config.save()?;
        tracing::info!("[init] Generated screen_id: {}", config.screen_id);
    }

    // 6. Connect to MPD (command connection)
    let mut mpd_client = mpd::MpdClient::connect(&mpd_host, mpd_port).await?;
    mpd_client.consume("0").await?;
    mpd_client.clear().await?;
    tracing::info!("[MPD] Connected to {}:{}", mpd_host, mpd_port);

    // 7. Connect MPD idle listener (dedicated connection for real-time events)
    //
    // MPD's protocol is single-command-at-a-time per connection — you can't
    // send `idle` and `status` on the same socket. Two connections is the
    // standard pattern used by every serious MPD client (mpd2, ncmpcpp, etc.).
    let mut mpd_idle = mpd::MpdIdleClient::connect(&mpd_host, mpd_port).await?;
    tracing::info!("[MPD] Idle listener connected");

    // 8. Create channels
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (player_cmd_tx, mut player_cmd_rx) = tokio::sync::mpsc::channel::<lounge::PlayerCommand>(32);
    let (state_tx, state_rx) = tokio::sync::mpsc::channel::<messages::OutgoingMessage>(32);
    let (pairing_tx, pairing_rx) = tokio::sync::mpsc::channel::<String>(8);

    // 9. Get local IP
    let local_ip = ssdp::get_local_ip()?;
    tracing::info!("[net] Local IP: {}", local_ip);

    // 10. Create shared DIAL state
    let dial_state = Arc::new(dial::DialState {
        running: std::sync::atomic::AtomicBool::new(false),
        pairing_tx,
        device_name: device_name.clone(),
        uuid: config.uuid.clone(),
        local_ip,
        port: dial_port,
        http_client: http_client.clone(),
    });

    // 11. Spawn background tasks

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
    tokio::spawn(async move {
        if let Err(e) = dial::run_dial_server(dial_port, dial_state, dial_shutdown).await {
            tracing::error!("[DIAL] Error: {}", e);
        }
    });

    // Lounge API (YouTube cast protocol)
    let lounge_shutdown = shutdown_rx.clone();
    let lounge_client = http_client.clone();
    let lounge_name = device_name.clone();
    let lounge_uuid = config.uuid.clone();
    let lounge_screen_id = config.screen_id.clone();
    // Launch lounge session with theme "cl" (classic YouTube).
    // The phone DIAL POST sends theme=cl even for YouTube Music (with topic=music).
    tokio::spawn(async move {
        if let Err(e) = lounge::run_lounge(
            lounge_client,
            lounge_name,
            lounge_uuid,
            lounge_screen_id,
            "cl".to_string(),
            player_cmd_tx,
            state_rx,
            pairing_rx,
            lounge_shutdown,
        )
        .await
        {
            tracing::error!("[Lounge] Error: {}", e);
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

    // 12. MPD idle listener — real-time player state change detection
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

    // 13. Player command loop (runs on main thread)
    //
    // - Idle events detect song endings instantly for auto-advance
    // - Position timer reports playback progress to YouTube every 3s (seekbar sync)
    let innertube_client = http_client;
    let mut playlist = PlaylistState::default();
    let mut play_started = false;
    let mut position_timer = tokio::time::interval(std::time::Duration::from_secs(3));

    loop {
        tokio::select! {
            // Player commands from the Lounge protocol
            cmd = player_cmd_rx.recv() => {
                let Some(cmd) = cmd else {
                    tracing::info!("[player] Command channel closed, shutting down");
                    break;
                };

                if let Err(e) = handle_command(
                    cmd,
                    &mut mpd_client,
                    &innertube_client,
                    &state_tx,
                    &mut playlist,
                    &mut play_started,
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
                            state_tx.send(
                                messages::OutgoingMessage::new("onStateChange")
                                    .with_arg("state", "0")
                                    .with_arg("currentTime", format!("{duration:.3}"))
                                    .with_arg("duration", format!("{duration:.3}")),
                            ).await.ok();
                            state_tx.send(
                                messages::OutgoingMessage::new("nowPlaying")
                                    .with_arg("videoId", &ended_vid)
                                    .with_arg("currentTime", format!("{duration:.3}"))
                                    .with_arg("duration", format!("{duration:.3}"))
                                    .with_arg("state", "0")
                                    .with_arg("listId", &playlist.list_id)
                                    .with_arg("currentIndex", playlist.current_index.to_string())
                                    .with_arg("cpn", &messages::zx()),
                            ).await.ok();

                            if playlist.current_index + 1 < playlist.video_ids.len() {
                                playlist.current_index += 1;
                                let video_id = playlist.video_ids[playlist.current_index].clone();
                                tracing::info!(
                                    "[player] Advancing to index {} ({})",
                                    playlist.current_index, video_id
                                );

                                match play_video(
                                    &mut mpd_client, &innertube_client, &state_tx,
                                    &mut playlist, &video_id, 0.0,
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
                if let Ok(status) = mpd_client.status().await {
                    if play_started && !playlist.video_ids.is_empty() && status.state == "play" {
                        let elapsed = status.elapsed.unwrap_or(0.0);
                        let duration = playlist.current_duration as f64;
                        let vid = current_video_id(&playlist);
                        state_tx.send(
                            messages::OutgoingMessage::new("onStateChange")
                                .with_arg("state", "1")
                                .with_arg("currentTime", format!("{elapsed:.3}"))
                                .with_arg("duration", format!("{duration:.3}")),
                        ).await.ok();
                        state_tx.send(
                            messages::OutgoingMessage::new("nowPlaying")
                                .with_arg("videoId", &vid)
                                .with_arg("currentTime", format!("{elapsed:.3}"))
                                .with_arg("duration", format!("{duration:.3}"))
                                .with_arg("state", "1")
                                .with_arg("listId", &playlist.list_id)
                                .with_arg("cpn", &messages::zx()),
                        ).await.ok();
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
) -> CmdResult {
    use lounge::PlayerCommand;

    match cmd {
        PlayerCommand::SetPlaylist {
            video_id,
            video_ids,
            list_id,
            index,
            ctt,
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
            playlist.ctt = ctt;

            *play_started = false;
            *play_started = play_video(mpd, http, state_tx, playlist, &video_id, current_time).await?;
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
            playlist.ctt = ctt;

            *play_started = false;
            *play_started = play_video(mpd, http, state_tx, playlist, &video_id, current_time).await?;
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
            mpd.seekcur(position).await?;
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
            let status = mpd.status().await?;
            let state = mpd_state_to_player_state(&status.state);
            let elapsed = status.elapsed.unwrap_or(0.0);
            let duration = playlist.current_duration as f64;

            let vid = current_video_id(playlist);

            state_tx
                .send(
                    messages::OutgoingMessage::new("nowPlaying")
                        .with_arg("videoId", &vid)
                        .with_arg("currentTime", format!("{elapsed:.3}"))
                        .with_arg("duration", format!("{duration:.3}"))
                        .with_arg("state", state.as_str())
                        .with_arg("listId", &playlist.list_id)
                        .with_arg("cpn", &messages::zx()),
                )
                .await
                .ok();
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
                *play_started = false;
                *play_started = play_video(mpd, http, state_tx, playlist, &video_id, 0.0).await?;
            } else {
                tracing::info!("[player] Previous: already at start");
            }
        }

        PlayerCommand::Next => {
            if playlist.current_index + 1 < playlist.video_ids.len() {
                playlist.current_index += 1;
                let video_id = playlist.video_ids[playlist.current_index].clone();
                tracing::info!("[player] Next -> index {} ({})", playlist.current_index, video_id);
                *play_started = false;
                *play_started = play_video(mpd, http, state_tx, playlist, &video_id, 0.0).await?;
            } else {
                tracing::info!("[player] Next: end of playlist");
                *play_started = false;
                mpd.stop().await?;
                mpd.clear().await?;
                send_state(mpd, state_tx, playlist, messages::PlayerState::Stopped).await?;
            }
        }

        PlayerCommand::RemoteDisconnected => {
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
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let stream = innertube::resolve_stream(
        http,
        video_id,
        playlist.ctt.as_deref(),
        if playlist.list_id.is_empty() {
            None
        } else {
            Some(playlist.list_id.as_str())
        },
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
                mpd.seekcur(start_position).await?;
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
        _ => messages::PlayerState::Stopped,
    }
}

/// Send onStateChange + nowPlaying + onHasPreviousNextChanged to the Lounge session.
///
/// This matches the Node.js yt-cast-receiver behavior: every state change
/// reports the full playback state AND updates navigation button availability.
async fn send_state(
    mpd: &mut mpd::MpdClient,
    state_tx: &tokio::sync::mpsc::Sender<messages::OutgoingMessage>,
    playlist: &PlaylistState,
    state: messages::PlayerState,
) -> CmdResult {
    let status = mpd.status().await?;
    let elapsed = status.elapsed.unwrap_or(0.0);
    let duration = playlist.current_duration as f64;
    let vid = current_video_id(playlist);

    state_tx
        .send(
            messages::OutgoingMessage::new("onStateChange")
                .with_arg("state", state.as_str())
                .with_arg("currentTime", format!("{elapsed:.3}"))
                .with_arg("duration", format!("{duration:.3}")),
        )
        .await
        .ok();

    state_tx
        .send(
            messages::OutgoingMessage::new("nowPlaying")
                .with_arg("videoId", &vid)
                .with_arg("currentTime", format!("{elapsed:.3}"))
                .with_arg("duration", format!("{duration:.3}"))
                .with_arg("state", state.as_str())
                .with_arg("listId", &playlist.list_id)
                .with_arg("currentIndex", playlist.current_index.to_string())
                .with_arg("cpn", &messages::zx()),
        )
        .await
        .ok();

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
