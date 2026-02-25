fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = &[
        "proto/misc/common.proto",
        "proto/video_streaming/video_playback_abr_request.proto",
        "proto/video_streaming/client_abr_state.proto",
        "proto/video_streaming/streamer_context.proto",
        "proto/video_streaming/buffered_range.proto",
        "proto/video_streaming/format_initialization_metadata.proto",
        "proto/video_streaming/media_header.proto",
        "proto/video_streaming/next_request_policy.proto",
        "proto/video_streaming/playback_cookie.proto",
        "proto/video_streaming/sabr_redirect.proto",
        "proto/video_streaming/sabr_error.proto",
        "proto/video_streaming/sabr_context_update.proto",
        "proto/video_streaming/sabr_context_sending_policy.proto",
        "proto/video_streaming/stream_protection_status.proto",
        "proto/video_streaming/time_range.proto",
        "proto/video_streaming/media_capabilities.proto",
        "proto/video_streaming/reload_player_response.proto",
        "proto/video_streaming/snackbar_message.proto",
    ];

    prost_build::Config::new()
        .compile_protos(protos, &["proto/"])?;

    Ok(())
}
