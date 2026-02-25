//! Core SABR streaming implementation.
//!
//! Handles the complete SABR audio streaming flow:
//!   InnerTube /player call -> format selection -> SABR request loop ->
//!   UMP response parsing -> audio byte extraction.
//!
//! Called from an axum HTTP handler. Takes a video ID and returns a stream
//! of audio bytes. The caller pipes these bytes as an HTTP response to MPD.

use std::collections::HashMap;

use bytes::Bytes;
use futures_util::StreamExt;
use prost::Message;
use tokio::sync::mpsc;

use super::proto::misc;
use super::proto::vs;
use super::ump::{self, UmpParser};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Metadata about the audio stream being served.
pub struct SabrStreamInfo {
    pub title: String,
    pub duration_secs: u64,
    pub mime_type: String,
}

/// Error type for SABR streaming.
pub type SabrError = Box<dyn std::error::Error + Send + Sync>;

// ---------------------------------------------------------------------------
// InnerTube client configuration
// ---------------------------------------------------------------------------

struct InnerTubeClient {
    client_name: &'static str,
    client_version: &'static str,
    device_make: &'static str,
    device_model: &'static str,
    os_name: &'static str,
    os_version: &'static str,
    user_agent: &'static str,
    client_name_id: i32,
    android_sdk_version: Option<i32>,
}

const IOS_CLIENT: InnerTubeClient = InnerTubeClient {
    client_name: "IOS",
    client_version: "19.45.4",
    device_make: "Apple",
    device_model: "iPhone16,2",
    os_name: "iPhone",
    os_version: "18.1.0.22B83",
    user_agent: "com.google.ios.youtube/19.45.4 (iPhone16,2; U; CPU iOS 18_1_0 like Mac OS X;)",
    client_name_id: 5,
    android_sdk_version: None,
};

const ANDROID_CLIENT: InnerTubeClient = InnerTubeClient {
    client_name: "ANDROID",
    client_version: "19.44.38",
    device_make: "Google",
    device_model: "Pixel 8",
    os_name: "Android",
    os_version: "14",
    user_agent: "com.google.android.youtube/19.44.38 (Linux; U; Android 14; en_US; Pixel 8) gzip",
    client_name_id: 3,
    android_sdk_version: Some(34),
};

// ---------------------------------------------------------------------------
// InnerTube /player response data
// ---------------------------------------------------------------------------

struct PlayerData {
    server_abr_streaming_url: String,
    ustreamer_config: Vec<u8>,
    formats: Vec<AdaptiveFormat>,
    title: String,
    duration_secs: u64,
}

#[derive(Debug, Clone)]
struct AdaptiveFormat {
    itag: i32,
    last_modified: u64,
    xtags: Option<String>,
    mime_type: String,
    bitrate: u64,
}

// ---------------------------------------------------------------------------
// InnerTube /player call
// ---------------------------------------------------------------------------

const INNERTUBE_PLAYER_URL: &str =
    "https://www.youtube.com/youtubei/v1/player?prettyPrint=false";

async fn innertube_player(
    http: &reqwest::Client,
    video_id: &str,
    itclient: &InnerTubeClient,
) -> Result<PlayerData, SabrError> {
    let mut client_obj = serde_json::json!({
        "clientName": itclient.client_name,
        "clientVersion": itclient.client_version,
        "deviceMake": itclient.device_make,
        "deviceModel": itclient.device_model,
        "osName": itclient.os_name,
        "osVersion": itclient.os_version,
        "hl": "en",
        "gl": "US",
    });

    if let Some(sdk) = itclient.android_sdk_version {
        client_obj["androidSdkVersion"] = serde_json::json!(sdk);
    }

    let body = serde_json::json!({
        "videoId": video_id,
        "context": { "client": client_obj },
        "contentCheckOk": true,
        "racyCheckOk": true,
    });

    tracing::info!(
        "[sabr] innertube /player for {} using {}",
        video_id,
        itclient.client_name
    );

    let resp = http
        .post(INNERTUBE_PLAYER_URL)
        .header("User-Agent", itclient.user_agent)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(format!(
            "innertube /player returned HTTP {}",
            resp.status()
        )
        .into());
    }

    let json: serde_json::Value = resp.json().await?;

    let status = json["playabilityStatus"]["status"]
        .as_str()
        .unwrap_or("UNKNOWN");
    if status != "OK" {
        let reason = json["playabilityStatus"]["reason"]
            .as_str()
            .unwrap_or("unknown reason");
        return Err(format!(
            "video {} not playable: {} ({})",
            video_id, status, reason
        )
        .into());
    }

    let server_abr_url = json["streamingData"]["serverAbrStreamingUrl"]
        .as_str()
        .ok_or("missing serverAbrStreamingUrl")?
        .to_owned();

    let ustreamer_b64 = json["playerConfig"]["mediaCommonConfig"]
        ["mediaUstreamerRequestConfig"]["videoPlaybackUstreamerConfig"]
        .as_str()
        .unwrap_or("");

    let ustreamer_config = if ustreamer_b64.is_empty() {
        Vec::new()
    } else {
        // YouTube uses URL-safe base64 (- and _ instead of + and /)
        // with optional padding. Use lenient decoding to handle both.
        use base64::Engine;
        use base64::engine::{GeneralPurpose, GeneralPurposeConfig, DecodePaddingMode};
        const URL_SAFE_LENIENT: GeneralPurpose = GeneralPurpose::new(
            &base64::alphabet::URL_SAFE,
            GeneralPurposeConfig::new()
                .with_decode_padding_mode(DecodePaddingMode::Indifferent),
        );
        URL_SAFE_LENIENT
            .decode(ustreamer_b64)
            .unwrap_or_default()
    };

    let formats_json = json["streamingData"]["adaptiveFormats"]
        .as_array()
        .ok_or("missing adaptiveFormats")?;

    let mut formats = Vec::with_capacity(formats_json.len());
    for f in formats_json {
        let itag = f["itag"].as_i64().unwrap_or(0) as i32;
        let last_modified = f["lastModified"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let xtags = f["xtags"].as_str().map(|s| s.to_owned());
        let mime_type = f["mimeType"].as_str().unwrap_or("").to_owned();
        let bitrate = f["bitrate"].as_u64().unwrap_or(0);

        formats.push(AdaptiveFormat {
            itag,
            last_modified,
            xtags,
            mime_type,
            bitrate,
        });
    }

    let title = json["videoDetails"]["title"]
        .as_str()
        .unwrap_or("Unknown")
        .to_owned();
    let duration_secs = json["videoDetails"]["lengthSeconds"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    Ok(PlayerData {
        server_abr_streaming_url: server_abr_url,
        ustreamer_config,
        formats,
        title,
        duration_secs,
    })
}

/// Call InnerTube /player, trying IOS first then falling back to ANDROID.
async fn get_player_data(
    http: &reqwest::Client,
    video_id: &str,
) -> Result<PlayerData, SabrError> {
    match innertube_player(http, video_id, &IOS_CLIENT).await {
        Ok(data) => Ok(data),
        Err(e) => {
            tracing::warn!("[sabr] IOS client failed: {}, trying ANDROID", e);
            innertube_player(http, video_id, &ANDROID_CLIENT).await
        }
    }
}

// ---------------------------------------------------------------------------
// Audio format selection
// ---------------------------------------------------------------------------

/// Choose the best audio format, preferring Opus/WebM at the highest bitrate.
fn choose_audio_format(formats: &[AdaptiveFormat]) -> Option<&AdaptiveFormat> {
    formats
        .iter()
        .filter(|f| f.mime_type.starts_with("audio/"))
        .max_by_key(|f| {
            let codec_priority: u64 = if f.mime_type.contains("opus") {
                1_000_000_000
            } else if f.mime_type.contains("mp4a") {
                500_000_000
            } else {
                0
            };
            codec_priority + f.bitrate
        })
}

/// Choose a video format for the SABR discard trick.
///
/// The SABR protocol requires both audio AND video formats in the request,
/// even for audio-only streaming. We select the lowest-quality video and
/// send a "fully buffered" BufferedRange (MAX values) so the server knows
/// not to send any video data.
fn choose_video_format(formats: &[AdaptiveFormat]) -> Option<&AdaptiveFormat> {
    formats
        .iter()
        .filter(|f| f.mime_type.starts_with("video/"))
        .min_by_key(|f| f.bitrate)
}

// ---------------------------------------------------------------------------
// SABR streaming state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct HeaderInfo {
    itag: i32,
    is_init_seg: bool,
    sequence_number: i32,
    start_ms: i64,
    duration_ms: i64,
    is_audio: bool,
}

/// Max i32 — used for the video discard trick.
const MAX_I32: i32 = i32::MAX;
const MAX_I64: i64 = MAX_I32 as i64;

struct SabrState {
    server_url: String,
    request_number: u32,
    playback_cookie: Option<Vec<u8>>,
    backoff_time_ms: u32,
    header_map: HashMap<u32, HeaderInfo>,
    // Audio tracking
    audio_itag: i32,
    audio_format_id: misc::FormatId,
    audio_max_segment: i32,
    audio_downloaded_ms: i64,
    audio_init_sent: bool,
    audio_initialized: bool,
    audio_total_end_ms: i64,
    audio_end_segment: i64,
    // Video format (for discard trick — we never play video)
    video_format_id: misc::FormatId,
    // SABR context
    sabr_contexts: HashMap<i32, Vec<u8>>,
    sabr_contexts_send: std::collections::HashSet<i32>,
    sabr_contexts_discard: std::collections::HashSet<i32>,
    ustreamer_config: Vec<u8>,
    client_config: &'static InnerTubeClient,
    done: bool,
}

impl SabrState {
    fn new(
        server_url: String,
        audio_format: &AdaptiveFormat,
        video_format: &AdaptiveFormat,
        ustreamer_config: Vec<u8>,
        client_config: &'static InnerTubeClient,
    ) -> Self {
        let audio_format_id = misc::FormatId {
            itag: Some(audio_format.itag),
            last_modified: Some(audio_format.last_modified),
            xtags: audio_format.xtags.clone(),
        };
        let video_format_id = misc::FormatId {
            itag: Some(video_format.itag),
            last_modified: Some(video_format.last_modified),
            xtags: video_format.xtags.clone(),
        };

        Self {
            server_url,
            request_number: 0,
            playback_cookie: None,
            backoff_time_ms: 0,
            header_map: HashMap::new(),
            audio_itag: audio_format.itag,
            audio_format_id,
            audio_max_segment: -1,
            audio_downloaded_ms: 0,
            audio_init_sent: false,
            audio_initialized: false,
            audio_total_end_ms: 0,
            audio_end_segment: 0,
            video_format_id,
            sabr_contexts: HashMap::new(),
            sabr_contexts_send: std::collections::HashSet::new(),
            sabr_contexts_discard: std::collections::HashSet::new(),
            ustreamer_config,
            client_config,
            done: false,
        }
    }

    /// Build a SABR VideoPlaybackAbrRequest protobuf.
    ///
    /// Matches the TypeScript googlevideo reference behavior:
    /// - Always includes a video format with MAX discard BufferedRange
    /// - Only includes audio in selectedFormatIds after FORMAT_INIT received
    /// - Always includes both in preferredAudio/VideoFormatIds
    fn build_request(&self) -> vs::VideoPlaybackAbrRequest {
        let client_info = vs::streamer_context::ClientInfo {
            device_make: Some(self.client_config.device_make.to_owned()),
            device_model: Some(self.client_config.device_model.to_owned()),
            client_name: Some(self.client_config.client_name_id),
            client_version: Some(self.client_config.client_version.to_owned()),
            os_name: Some(self.client_config.os_name.to_owned()),
            os_version: Some(self.client_config.os_version.to_owned()),
            android_sdk_version: self.client_config.android_sdk_version,
            ..Default::default()
        };

        let sabr_ctxs: Vec<vs::streamer_context::SabrContext> = self
            .sabr_contexts
            .iter()
            .filter(|(ct, _)| {
                !self.sabr_contexts_discard.contains(ct)
                    && self.sabr_contexts_send.contains(ct)
            })
            .map(|(ct, v)| vs::streamer_context::SabrContext {
                r#type: Some(*ct),
                value: Some(v.clone()),
            })
            .collect();

        let streamer_context = vs::StreamerContext {
            client_info: Some(client_info),
            playback_cookie: self.playback_cookie.clone(),
            sabr_contexts: sabr_ctxs,
            ..Default::default()
        };

        // -- BufferedRanges --
        let mut buffered_ranges = Vec::new();

        // Audio: only report after we've received at least one segment.
        if self.audio_max_segment >= 0 {
            buffered_ranges.push(vs::BufferedRange {
                format_id: self.audio_format_id.clone(),
                start_time_ms: 0,
                duration_ms: self.audio_downloaded_ms,
                start_segment_index: 0,
                end_segment_index: self.audio_max_segment,
                time_range: Some(vs::TimeRange {
                    start_ticks: Some(0),
                    duration_ticks: Some(self.audio_downloaded_ms),
                    timescale: Some(1000),
                }),
                ..Default::default()
            });
        }

        // Video: always "fully buffered" (discard trick).
        // This tells the server we have ALL video data, so it won't send any.
        buffered_ranges.push(vs::BufferedRange {
            format_id: self.video_format_id.clone(),
            start_time_ms: 0,
            duration_ms: MAX_I64,
            start_segment_index: MAX_I32,
            end_segment_index: MAX_I32,
            time_range: Some(vs::TimeRange {
                start_ticks: Some(0),
                duration_ticks: Some(MAX_I64),
                timescale: Some(1000),
            }),
            ..Default::default()
        });

        // -- selectedFormatIds --
        // Per TS reference: video (discarded) is always in selectedFormatIds.
        // Audio is only added after FORMAT_INITIALIZATION_METADATA is received.
        let mut selected_format_ids = vec![self.video_format_id.clone()];
        if self.audio_initialized {
            selected_format_ids.push(self.audio_format_id.clone());
        }

        let client_abr_state = vs::ClientAbrState {
            enabled_track_types_bitfield: Some(1), // 1 = audio only
            playback_rate: Some(1.0),
            bandwidth_estimate: Some(5_000_000),
            player_time_ms: Some(0),
            visibility: Some(1),
            player_state: Some(1),
            ..Default::default()
        };

        vs::VideoPlaybackAbrRequest {
            client_abr_state: Some(client_abr_state),
            selected_format_ids,
            buffered_ranges,
            video_playback_ustreamer_config: if self.ustreamer_config.is_empty() {
                None
            } else {
                Some(self.ustreamer_config.clone())
            },
            preferred_audio_format_ids: vec![self.audio_format_id.clone()],
            preferred_video_format_ids: vec![self.video_format_id.clone()],
            streamer_context: Some(streamer_context),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// UMP part handlers
// ---------------------------------------------------------------------------

fn handle_format_init_metadata(state: &mut SabrState, data: &[u8]) -> Result<(), SabrError> {
    let meta = vs::FormatInitializationMetadata::decode(data)?;
    let itag = meta.format_id.as_ref().and_then(|f| f.itag).unwrap_or(0);
    let mime_type = meta.mime_type.clone().unwrap_or_default();
    let is_audio = itag == state.audio_itag;

    tracing::info!(
        "[sabr] format init: itag={}, mime={}, audio={}, end_seg={:?}, end_ms={:?}",
        itag, mime_type, is_audio, meta.end_segment_number, meta.end_time_ms,
    );

    if is_audio {
        state.audio_initialized = true;
        state.audio_total_end_ms = meta.end_time_ms.unwrap_or(0);
        state.audio_end_segment = meta.end_segment_number.unwrap_or(0);
    }

    Ok(())
}

fn handle_media_header(state: &mut SabrState, data: &[u8]) -> Result<(), SabrError> {
    let hdr = vs::MediaHeader::decode(data)?;
    let header_id = hdr.header_id.unwrap_or(0);
    let itag = hdr.itag.unwrap_or(0);
    let is_audio = itag == state.audio_itag;

    // Compute duration_ms: prefer the direct field, fall back to timeRange.
    // The TS reference does this same fallback.
    let duration_ms = hdr.duration_ms.unwrap_or_else(|| {
        if let Some(ref tr) = hdr.time_range {
            let ticks = tr.duration_ticks.unwrap_or(0);
            let timescale = tr.timescale.unwrap_or(1000);
            if timescale > 0 {
                (ticks * 1000) / timescale as i64
            } else {
                0
            }
        } else {
            0
        }
    });
    let start_ms = hdr.start_ms.unwrap_or_else(|| {
        if let Some(ref tr) = hdr.time_range {
            let ticks = tr.start_ticks.unwrap_or(0);
            let timescale = tr.timescale.unwrap_or(1000);
            if timescale > 0 {
                (ticks * 1000) / timescale as i64
            } else {
                0
            }
        } else {
            0
        }
    });

    tracing::debug!(
        "[sabr] media header: id={}, itag={}, init={:?}, seq={:?}, start={}ms, dur={}ms, tr={:?}, audio={}",
        header_id, itag, hdr.is_init_seg, hdr.sequence_number,
        start_ms, duration_ms, hdr.time_range, is_audio,
    );

    state.header_map.insert(header_id, HeaderInfo {
        itag,
        is_init_seg: hdr.is_init_seg.unwrap_or(false),
        sequence_number: hdr.sequence_number.unwrap_or(0),
        start_ms,
        duration_ms,
        is_audio,
    });

    Ok(())
}

/// Handle MEDIA part. First byte is header_id, rest is media payload.
fn handle_media(state: &mut SabrState, data: &[u8]) -> Option<Bytes> {
    if data.is_empty() {
        return None;
    }

    let header_id = data[0] as u32;
    let payload = &data[1..];

    let info = match state.header_map.get(&header_id) {
        Some(i) => i.clone(),
        None => {
            tracing::warn!("[sabr] media: unknown header_id={}", header_id);
            return None;
        }
    };

    if !info.is_audio || payload.is_empty() {
        return None;
    }

    if info.is_init_seg {
        state.audio_init_sent = true;
    }

    Some(Bytes::copy_from_slice(payload))
}

/// Handle MEDIA_END. First byte is header_id. Update buffered-range tracking.
fn handle_media_end(state: &mut SabrState, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    let header_id = data[0] as u32;
    let info = match state.header_map.get(&header_id) {
        Some(i) => i.clone(),
        None => return,
    };
    if !info.is_audio || info.is_init_seg {
        return;
    }
    if info.sequence_number > state.audio_max_segment {
        state.audio_max_segment = info.sequence_number;
    }
    let seg_end = info.start_ms + info.duration_ms;
    if seg_end > state.audio_downloaded_ms {
        state.audio_downloaded_ms = seg_end;
    }

    tracing::debug!(
        "[sabr] audio seg {} done ({}ms-{}ms), progress {}ms/{}ms",
        info.sequence_number, info.start_ms, info.start_ms + info.duration_ms,
        state.audio_downloaded_ms, state.audio_total_end_ms,
    );
}

fn handle_next_request_policy(state: &mut SabrState, data: &[u8]) -> Result<(), SabrError> {
    let policy = vs::NextRequestPolicy::decode(data)?;
    state.backoff_time_ms = policy.backoff_time_ms.unwrap_or(0) as u32;
    if let Some(ref cookie) = policy.playback_cookie {
        state.playback_cookie = Some(cookie.encode_to_vec());
    }
    tracing::debug!(
        "[sabr] next request: backoff={}ms, cookie={}",
        state.backoff_time_ms, policy.playback_cookie.is_some(),
    );
    Ok(())
}

fn handle_sabr_redirect(state: &mut SabrState, data: &[u8]) -> Result<(), SabrError> {
    let redirect = vs::SabrRedirect::decode(data)?;
    if let Some(url) = redirect.url {
        tracing::info!("[sabr] redirect to new streaming URL");
        state.server_url = url;
    }
    Ok(())
}

fn handle_sabr_error(data: &[u8]) -> Result<(), SabrError> {
    let err = vs::SabrError::decode(data)?;
    let t = err.r#type.as_deref().unwrap_or("unknown");
    let c = err.code.unwrap_or(0);
    Err(format!("SABR error: type={}, code={}", t, c).into())
}

fn handle_context_update(state: &mut SabrState, data: &[u8]) -> Result<(), SabrError> {
    let update = vs::SabrContextUpdate::decode(data)?;
    let ctx_type = update.r#type.unwrap_or(0);
    let send = update.send_by_default.unwrap_or(false);
    if let Some(value) = update.value {
        state.sabr_contexts.insert(ctx_type, value);
    }
    if send {
        state.sabr_contexts_send.insert(ctx_type);
    }
    tracing::debug!("[sabr] context update: type={}, send={}", ctx_type, send);
    Ok(())
}

fn handle_context_sending_policy(state: &mut SabrState, data: &[u8]) -> Result<(), SabrError> {
    let policy = vs::SabrContextSendingPolicy::decode(data)?;
    for t in &policy.start_policy {
        state.sabr_contexts_send.insert(*t);
    }
    for t in &policy.stop_policy {
        state.sabr_contexts_send.remove(t);
    }
    for t in &policy.discard_policy {
        state.sabr_contexts_discard.insert(*t);
        state.sabr_contexts.remove(t);
    }
    tracing::debug!(
        "[sabr] sending policy: start={:?}, stop={:?}, discard={:?}",
        policy.start_policy, policy.stop_policy, policy.discard_policy,
    );
    Ok(())
}

fn handle_stream_protection(data: &[u8]) -> Result<(), SabrError> {
    let sps = vs::StreamProtectionStatus::decode(data)?;
    if sps.status.unwrap_or(0) != 0 {
        tracing::warn!(
            "[sabr] stream protection status={} (attestation may be required)",
            sps.status.unwrap_or(0),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Single SABR request/response cycle
// ---------------------------------------------------------------------------

async fn sabr_request_cycle(
    http: &reqwest::Client,
    state: &mut SabrState,
    tx: &mpsc::Sender<Bytes>,
) -> Result<bool, SabrError> {
    let req_proto = state.build_request();
    let encoded = req_proto.encode_to_vec();

    let url = if state.server_url.contains('?') {
        format!("{}&rn={}", state.server_url, state.request_number)
    } else {
        format!("{}?rn={}", state.server_url, state.request_number)
    };

    tracing::info!(
        "[sabr] request #{} ({} bytes), progress {}ms/{}ms",
        state.request_number, encoded.len(),
        state.audio_downloaded_ms, state.audio_total_end_ms,
    );

    let resp = http
        .post(&url)
        .header("User-Agent", state.client_config.user_agent)
        .header("Content-Type", "application/x-protobuf")
        .header("Accept", "application/vnd.yt-ump")
        .body(encoded)
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(format!("SABR request returned HTTP {}", resp.status()).into());
    }

    let mut parser = UmpParser::new();
    let mut byte_stream = resp.bytes_stream();

    while let Some(chunk_result) = byte_stream.next().await {
        let chunk = chunk_result?;
        parser.push(&chunk);

        while let Some(part) = parser.next_part() {
            match part.part_type {
                ump::FORMAT_INITIALIZATION_METADATA => {
                    handle_format_init_metadata(state, &part.data)?;
                }
                ump::MEDIA_HEADER => {
                    handle_media_header(state, &part.data)?;
                }
                ump::MEDIA => {
                    if let Some(audio_bytes) = handle_media(state, &part.data) {
                        if tx.send(audio_bytes).await.is_err() {
                            tracing::info!("[sabr] receiver dropped, stopping");
                            return Ok(false);
                        }
                    }
                }
                ump::MEDIA_END => {
                    handle_media_end(state, &part.data);
                }
                ump::NEXT_REQUEST_POLICY => {
                    handle_next_request_policy(state, &part.data)?;
                }
                ump::SABR_REDIRECT => {
                    handle_sabr_redirect(state, &part.data)?;
                }
                ump::SABR_ERROR => {
                    handle_sabr_error(&part.data)?;
                }
                ump::SABR_CONTEXT_UPDATE => {
                    handle_context_update(state, &part.data)?;
                }
                ump::STREAM_PROTECTION_STATUS => {
                    handle_stream_protection(&part.data)?;
                }
                ump::SABR_CONTEXT_SENDING_POLICY => {
                    handle_context_sending_policy(state, &part.data)?;
                }
                other => {
                    tracing::debug!(
                        "[sabr] unhandled UMP type={}, {} bytes",
                        other, part.data.len(),
                    );
                }
            }
        }
    }

    state.request_number += 1;

    if state.audio_end_segment > 0
        && state.audio_max_segment >= (state.audio_end_segment as i32)
    {
        tracing::info!(
            "[sabr] all audio segments downloaded ({}/{})",
            state.audio_max_segment, state.audio_end_segment,
        );
        state.done = true;
        return Ok(false);
    }

    if state.audio_total_end_ms > 0
        && state.audio_downloaded_ms >= state.audio_total_end_ms
    {
        tracing::info!(
            "[sabr] all audio downloaded by time ({}ms/{}ms)",
            state.audio_downloaded_ms, state.audio_total_end_ms,
        );
        state.done = true;
        return Ok(false);
    }

    Ok(true)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Stream audio for a YouTube video via the SABR protocol.
///
/// Returns metadata about the stream and a channel receiver that yields
/// audio byte chunks. The init segment (WebM/Opus container headers) is sent
/// first, followed by media segments in order.
///
/// A background tokio task runs the SABR request loop. When all segments
/// have been downloaded, or an error occurs, the task ends and the channel
/// closes naturally.
pub async fn stream_audio(
    client: &reqwest::Client,
    video_id: &str,
) -> Result<(SabrStreamInfo, mpsc::Receiver<Bytes>), SabrError> {
    // 1. Call InnerTube /player to get streaming metadata.
    let player = get_player_data(client, video_id).await?;

    // 2. Pick the best audio format + a video format for the discard trick.
    let audio_fmt = choose_audio_format(&player.formats)
        .ok_or("no suitable audio format found")?
        .clone();
    let video_fmt = choose_video_format(&player.formats)
        .ok_or("no video format found (needed for SABR discard trick)")?
        .clone();

    tracing::info!(
        "[sabr] selected audio: itag={}, mime={}, bitrate={}",
        audio_fmt.itag, audio_fmt.mime_type, audio_fmt.bitrate,
    );
    tracing::info!(
        "[sabr] selected video (discard): itag={}, mime={}",
        video_fmt.itag, video_fmt.mime_type,
    );

    let info = SabrStreamInfo {
        title: player.title.clone(),
        duration_secs: player.duration_secs,
        mime_type: audio_fmt.mime_type.clone(),
    };

    // We always use the IOS client config for SABR StreamerContext.
    let client_config: &'static InnerTubeClient = &IOS_CLIENT;

    // 3. Channel for streaming audio bytes to the caller.
    let (tx, rx) = mpsc::channel::<Bytes>(64);

    // 4. Spawn the SABR streaming loop.
    let http = client.clone();
    let server_url = player.server_abr_streaming_url;
    let ustreamer_config = player.ustreamer_config;

    let video_fmt_clone = video_fmt.clone();
    tokio::spawn(async move {
        let result = sabr_loop(
            &http, server_url, &audio_fmt, &video_fmt_clone,
            ustreamer_config, client_config, &tx,
        ).await;

        match result {
            Ok(()) => tracing::info!("[sabr] streaming complete"),
            Err(e) => tracing::error!("[sabr] streaming error: {}", e),
        }
        // tx is dropped here, closing the channel.
    });

    Ok((info, rx))
}

/// Inner SABR request loop. Runs until all audio segments are downloaded,
/// the downstream receiver disconnects, or an unrecoverable error occurs.
///
/// Transient network errors are retried with exponential backoff (up to 5
/// retries). This matches the TypeScript reference behavior.
async fn sabr_loop(
    http: &reqwest::Client,
    server_url: String,
    audio_format: &AdaptiveFormat,
    video_format: &AdaptiveFormat,
    ustreamer_config: Vec<u8>,
    client_config: &'static InnerTubeClient,
    tx: &mpsc::Sender<Bytes>,
) -> Result<(), SabrError> {
    let mut state = SabrState::new(
        server_url, audio_format, video_format, ustreamer_config, client_config,
    );

    let mut consecutive_errors = 0u32;
    const MAX_RETRIES: u32 = 5;

    loop {
        match sabr_request_cycle(http, &mut state, tx).await {
            Ok(should_continue) => {
                consecutive_errors = 0;
                if !should_continue || state.done {
                    break;
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors > MAX_RETRIES {
                    return Err(format!(
                        "SABR failed after {} retries: {}", MAX_RETRIES, e
                    ).into());
                }
                let delay_ms = 500 * 2u64.pow(consecutive_errors - 1);
                tracing::warn!(
                    "[sabr] request error (attempt {}/{}): {}, retrying in {}ms",
                    consecutive_errors, MAX_RETRIES, e, delay_ms,
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                continue;
            }
        }

        // Respect the server-requested backoff before the next request.
        if state.backoff_time_ms > 0 {
            tracing::debug!("[sabr] backing off {}ms", state.backoff_time_ms);
            tokio::time::sleep(std::time::Duration::from_millis(
                state.backoff_time_ms as u64,
            )).await;
        }
    }

    Ok(())
}
