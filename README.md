# ytcast-open

> **Known issue (Feb 2026):** YouTube Music has a server-side casting bug affecting certain songs across all cast targets — including official Chromecast, Nest speakers, and Android TV. Some tracks show a loading spinner or "Sorry, something went wrong" when cast, while playing fine locally on the phone. This is not a ytcast-open bug. See: [Reddit thread 1](https://www.reddit.com/r/YoutubeMusic/comments/1rdirhx/sorry_something_went_wrong_when_youre_ready_give/), [Reddit thread 2](https://www.reddit.com/r/YoutubeMusic/comments/1r9zu7v/casting_to_honedevices_stopped_working/). Confirmed by testing with a Pixel 9a casting to both this receiver and a Sony Android TV — same failures on both.

A lightweight YouTube Music cast receiver written in Rust. Makes a Raspberry Pi (or any Linux device) appear as a cast target in YouTube Music. Tap Cast, see your device, play music through MPD.

**~13MB RAM idle. Single binary. One optional sidecar for PoToken.**

Audio streaming powered by [sabr-rs](https://github.com/mthwJsmith/sabr-rs), my Rust implementation of YouTube's SABR protocol.

## How it works

```
YouTube Music (phone)
    | SSDP multicast discovery
    v
ytcast-open (single Rust binary)
    | DIAL protocol (port 8008) - device appears in cast menu
    | YouTube Lounge API - long-poll session for play/pause/seek/skip/volume
    | SABR protocol - YouTube's native streaming, built-in via sabr-rs
    v
MPD (Music Player Daemon)
    | ALSA output
    v
speakers
```

1. Phone discovers the device via SSDP/DIAL (same open protocol Chromecast uses for discovery)
2. Phone connects via YouTube's Lounge API (reverse-engineered cast control protocol)
3. Phone sends a video ID
4. Built-in SABR implementation streams audio directly from YouTube's CDN
5. MPD plays the stream. Position, duration, and state sync back to the phone in real-time

No Node.js. No Python. No yt-dlp. Just one binary.

## Stream resolution - SABR

All audio is streamed using YouTube's **SABR (Server Adaptive Bit Rate) protocol**, the same protocol the official YouTube app uses. The SABR implementation is built directly into the binary using [sabr-rs](https://github.com/mthwJsmith/sabr-rs), our Rust port of the protocol.

Unlike direct URL approaches that break when YouTube changes client policies, SABR is YouTube's own native streaming protocol. It handles format negotiation, server redirects, retries, and backoff automatically.

**Under the hood:**
1. InnerTube `/player` call using WEB_REMIX client + credential transfer token (ctt) from the cast session for authenticated access. When casting from YouTube Music, the phone sends a per-video ctt automatically — no setup needed. Falls back to IOS/ANDROID clients without ctt (some tracks may be restricted without authentication)
2. Picks the highest-bitrate Opus audio format
3. Opens a SABR session with YouTube's CDN using protobuf requests
4. Parses UMP (Universal Media Protocol) binary response frames
5. Extracts and streams audio segments as chunked HTTP to MPD
6. Reports buffered ranges so the server sends the next segments

The SABR code adds roughly 1800 lines of Rust across 3 files. Proto definitions (18 `.proto` files) are compiled at build time via `prost-build`. This replaced an earlier Node.js SABR proxy (ytresolve) that used ~84MB RSS on its own.

## Features

- Cast from YouTube Music or YouTube (phone, tablet, browser)
- Play, pause, seek, skip, previous, volume - all controlled from phone
- Auto-advance to next track when a song finishes (MPD idle events, not polling)
- Playlist support with queue updates mid-session
- Phone UI stays fully synced (seekbar, album art, controls)
- Graceful disconnect (stops playback when phone disconnects)
- Best-quality Opus audio (itag 251, up to ~160kbps, 48kHz stereo)
- Authenticated streaming via cast session credential transfer tokens (WEB_REMIX + ctt)
- Full YouTube Music session context (fetches live ytcfg from music.youtube.com for WEB_REMIX /player)
- n-parameter (nsig) challenge decoding via embedded yt-dlp/ejs solver in QuickJS
- Dual Lounge sessions (YouTube + YouTube Music themes) for broad compatibility
- Debug HTTP endpoints for testing without a phone (`/debug/play/VIDEO_ID`, `/debug/pause`, etc.)

## Requirements

- **MPD** (Music Player Daemon) running and accessible
- **Linux** (tested on Raspberry Pi Zero 2W with Debian trixie/aarch64)
- Network access to YouTube servers
- **Optional: [bgutil-ytdlp-pot-provider-rs](https://github.com/jim60105/bgutil-ytdlp-pot-provider-rs)** for PoToken generation (prevents CDN throttle after 60s). Uses [rustypipe-botguard](https://github.com/tobiashenkel/rustypipe-botguard) internally. Runs as HTTP server on port 4416. ~66MB RAM due to embedded V8 JS engine for BotGuard challenges — can be hosted on a separate machine/VPS to save RAM on the Pi.

No Node.js, no Python, no yt-dlp.

## Usage

```bash
# Set device name (default: "Living Room Pi")
export DEVICE_NAME="Living Room Pi"

# MPD connection (defaults: localhost:6600)
export MPD_HOST=localhost
export MPD_PORT=6600

# Run
./ytcast-open
```

Open YouTube Music on your phone, tap Cast, and select your device.

## Building

```bash
# Native build (with mock MPD for development on Windows/Mac)
cargo build --features mock-mpd

# Release build
cargo build --release

# Cross-compile for Raspberry Pi (aarch64)
docker run --rm -v "$PWD:/src" -w /src rust:1.89-bookworm bash -c \
  "apt-get update -qq && apt-get install -y -qq gcc-aarch64-linux-gnu protobuf-compiler && \
   rustup target add aarch64-unknown-linux-gnu && \
   CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
   cargo build --release --target aarch64-unknown-linux-gnu"
```

## Architecture

| File | Lines | Purpose |
|------|-------|---------|
| `main.rs` | ~1500 | Entry point, player command loop, MPD idle events, auto-advance, debug endpoints |
| `lounge.rs` | ~1090 | YouTube Lounge API - session management, streaming long-poll, command parsing |
| `messages.rs` | ~540 | Lounge message parsing + `ChunkParser` for HTTP streaming responses |
| `cookies.rs` | ~410 | Cookie loading, YouTube Music session extraction (ytcfg), triple SAPISIDHASH auth |
| `innertube.rs` | ~250 | InnerTube /player for metadata (WEB_REMIX+ctt with full context, or IOS fallback) |
| `nsig.rs` | ~250 | n-parameter challenge decoder using yt-dlp/ejs solver in QuickJS |
| `mpd.rs` | ~420 | Raw MPD TCP client (command + idle connections) + mock mode for dev |
| `ssdp.rs` | ~280 | SSDP multicast discovery responder |
| `dial.rs` | ~420 | DIAL HTTP server on port 8008 + `/stream/:videoId` endpoint |
| `config.rs` | ~110 | JSON config (UUID, screen_id, screen_id_m) load/save |
| `sabr/stream.rs` | ~1450 | SABR streaming - InnerTube /player, PoToken fetch, nsig decode, format selection, SABR request loop, UMP parsing |
| `sabr/ump.rs` | ~320 | UMP binary codec (YouTube's custom varint) + streaming parser + tests |
| `sabr/mod.rs` | ~12 | Module glue + protobuf includes |

Single-threaded tokio runtime. Two MPD TCP connections (command + idle). One long-poll HTTP connection to YouTube Lounge API. SABR requests run per-track in a spawned task.

## RAM usage

RSS = Resident Set Size (actual physical RAM used by the process).

| Component | RAM (RSS) |
|-----------|-----------|
| ytcast-open idle | ~12.8MB |
| ytcast-open during SABR streaming | ~41MB (55MB peak during nsig decode, settles back) |
| bgutil-pot (PoToken sidecar, optional) | ~66MB |
| MPD | ~10MB |
| **Total system (without bgutil-pot)** | **~55-65MB** |
| **Total system (with bgutil-pot)** | **~120-130MB** |

The previous architecture used a Node.js SABR proxy (ytresolve, ~84MB) alongside the Rust binary. That's gone now. All SABR streaming is built into the single binary via [sabr-rs](https://github.com/mthwJsmith/sabr-rs).

The bgutil-pot sidecar generates PoTokens (BotGuard attestation) needed to prevent YouTube's CDN from throttling streams after 60 seconds. It uses ~66MB because it embeds a V8/Deno JavaScript VM to execute Google's BotGuard challenges — this is unfortunately unavoidable as BotGuard requires real JS execution. To save RAM on constrained devices, bgutil-pot can be hosted on a separate machine or VPS and accessed via a single HTTP call.

## Comparison

| | ytcast-open (current) | Previous (Rust + Node.js ytresolve) | Typical yt-dlp setup |
|-|----------------------|-------------------------------------|---------------------|
| Idle RAM | **~12.8MB** | ~60MB (3.8MB + 55MB Node) | 50-90MB per invocation |
| Binary/install | **2.4MB single binary** | 2.4MB + 2MB node_modules | Python + yt-dlp + JS runtime |
| Runtime deps | **None** | Node.js 18+ | Python + JS runtime |
| Startup | ~50ms | ~2s (Node.js) | 2-5s per video |
| Stream method | SABR (built-in Rust) | SABR (Node.js googlevideo) | yt-dlp sig decryption |
| Future-proof | Yes - YouTube's own protocol | Yes | Breaks frequently |

## Credits

- [sabr-rs](https://github.com/mthwJsmith/sabr-rs) - our Rust SABR implementation (extracted as a standalone crate)
- [googlevideo](https://github.com/LuanRT/googlevideo) by LuanRT - TypeScript SABR reference implementation + proto definitions
- [yt-dlp/ejs](https://github.com/nichobi/ejs) - JavaScript-based n-parameter (nsig) challenge solver, embedded in QuickJS
- [bgutil-ytdlp-pot-provider-rs](https://github.com/jim60105/bgutil-ytdlp-pot-provider-rs) by jim60105 - Rust PoToken provider (BotGuard attestation)
- [rustypipe-botguard](https://github.com/tobiashenkel/rustypipe-botguard) - Rust BotGuard challenge solver used by bgutil-pot
- [yt-cast-receiver](https://github.com/patrickkfkan/yt-cast-receiver) by patrickkfkan - Node.js cast receiver framework (protocol reference)
- [plaincast](https://github.com/nickvdp/plaincast) - Go cast receiver (Lounge API reference)
- [ytm-mpd](https://github.com/dgalli1/ytm-mpd) by dgalli1 - original YouTube Cast + MPD bridge

## License

MIT
