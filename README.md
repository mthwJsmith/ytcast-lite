# ytcast-open

A lightweight YouTube Music cast receiver written in Rust. Makes a Raspberry Pi (or any Linux device) appear as a cast target in YouTube Music — tap Cast, see your device, play music through MPD.

**3.8MB RSS idle. 2.4MB binary.**

## How it works

```
YouTube Music (phone)
    | SSDP multicast discovery
    v
ytcast-open (Rust binary)
    | DIAL protocol (port 8008) — device appears in cast menu
    | YouTube Lounge API — long-poll session for play/pause/seek/skip/volume
    v
ytresolve (Node.js, local or VPS)
    | SABR protocol — YouTube's native streaming protocol
    | Raw InnerTube /player call → SABR session → opus audio stream
    v
MPD (Music Player Daemon)
    | ALSA output
    v
speakers
```

1. Phone discovers the device via SSDP/DIAL (same open protocol Chromecast uses for discovery)
2. Phone connects via YouTube's Lounge API (reverse-engineered cast control protocol)
3. Phone sends a video ID — ytcast-open passes it to ytresolve
4. ytresolve streams audio via YouTube's SABR protocol and serves it as HTTP
5. MPD plays the stream. Position, duration, and state sync back to the phone in real-time

## Stream resolution — SABR

All audio is streamed using YouTube's **SABR (Server Adaptive Bit Rate) protocol** — the same protocol the official YouTube app uses. This is handled by ytresolve using the [googlevideo](https://github.com/LuanRT/googlevideo) library.

Unlike direct URL approaches that break when YouTube changes client policies, SABR is YouTube's own native streaming protocol. It handles format negotiation, server redirects, retries, and backoff automatically.

**How it works under the hood:**
1. ytresolve makes a raw InnerTube `/player` call (IOS client)
2. Gets the SABR streaming URL + format metadata from the response
3. Opens a SABR session using googlevideo's `SabrStream`
4. Pipes opus audio chunks directly to the HTTP response
5. MPD plays it as a regular HTTP audio source

## Features

- Cast from YouTube Music or YouTube (phone, tablet, browser)
- Play, pause, seek, skip, previous, volume — all controlled from phone
- Auto-advance to next track when a song finishes (MPD idle events, not polling)
- Playlist support with queue updates mid-session
- Phone UI stays fully synced (seekbar, album art, controls)
- Graceful disconnect (stops playback when phone disconnects)

## ytresolve — SABR audio proxy

A Node.js service that streams YouTube audio via SABR. Runs on the Pi itself (~55MB RAM) or on a VPS.

**Endpoints:**
- `GET /stream/:videoId` — streams opus audio via SABR protocol
- `GET /resolve/:videoId` — returns a direct audio URL (if available)
- `GET /health` — health check + active stream count

**~55MB RSS.** Only dependency is [googlevideo](https://github.com/LuanRT/googlevideo) (YouTube's SABR protocol implementation).

### Setup

```bash
# On the Pi (or any Linux box)
cd ytresolve
npm install
node server.js

# Or with Docker (for VPS deployment)
cd ytresolve
docker build -t ytresolve .
docker run -d --name ytresolve -p 3033:3033 ytresolve
```

### Deploying on a VPS instead

For ultra-low RAM on the Pi, run ytresolve on a VPS and point ytcast-open at it:

```bash
export YTRESOLVE_URL="https://your-vps:3033"
export YTRESOLVE_SECRET="your-shared-secret"
```

## Requirements

- **Node.js 18+** (for ytresolve)
- **MPD** (Music Player Daemon) running and accessible
- **Linux** (tested on Raspberry Pi Zero 2W with Debian trixie/aarch64)
- Network access to YouTube servers

## Usage

```bash
# Start ytresolve (SABR audio proxy)
cd ytresolve && node server.js &

# Set device name (default: "Living Room Pi")
export DEVICE_NAME="Living Room Pi"

# MPD connection (defaults: localhost:6600)
export MPD_HOST=localhost
export MPD_PORT=6600

# ytresolve (defaults to localhost:3033)
export YTRESOLVE_URL="http://localhost:3033"

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
  "apt-get update -qq && apt-get install -y -qq gcc-aarch64-linux-gnu && \
   rustup target add aarch64-unknown-linux-gnu && \
   CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
   cargo build --release --target aarch64-unknown-linux-gnu"
```

## Architecture

| File | Lines | Purpose |
|------|-------|---------|
| `main.rs` | ~750 | Entry point, player command loop, MPD idle events, auto-advance |
| `lounge.rs` | ~1020 | YouTube Lounge API — session management, streaming long-poll, command parsing |
| `messages.rs` | ~510 | Lounge message parsing + `ChunkParser` for HTTP streaming responses |
| `innertube.rs` | ~50 | Stream resolver — builds ytresolve `/stream/` URL |
| `mpd.rs` | ~340 | Raw MPD TCP client (command + idle connections) + mock mode for dev |
| `ssdp.rs` | ~100 | SSDP multicast discovery responder |
| `dial.rs` | ~120 | DIAL HTTP server on port 8008 |
| `config.rs` | ~50 | JSON config (UUID, screen_id) load/save |

Single-threaded tokio runtime. Two MPD TCP connections (command + idle). One long-poll HTTP connection to YouTube Lounge API.

## Comparison

| | ytcast-open + ytresolve | Node.js version | Typical yt-dlp setup |
|-|------------------------|----------------|---------------------|
| Idle RSS | **~60MB** (3.8MB Rust + 55MB Node) | ~70MB | 50-90MB per invocation |
| Binary/install | 2.4MB binary + 2MB node_modules | 38MB node_modules | Python + yt-dlp + JS runtime |
| Startup | ~50ms | ~2s | 2-5s per video |
| Stream method | **SABR** (YouTube's native protocol) | YouTube.js | yt-dlp sig decryption |
| Future-proof | Yes — uses YouTube's own protocol | Depends on YouTube.js updates | Breaks frequently |

## Credits

- [googlevideo](https://github.com/LuanRT/googlevideo) by LuanRT — SABR protocol implementation
- [yt-cast-receiver](https://github.com/patrickkfkan/yt-cast-receiver) by patrickkfkan — Node.js cast receiver framework (protocol reference)
- [plaincast](https://github.com/nickvdp/plaincast) — Go cast receiver (Lounge API reference)
- [ytm-mpd](https://github.com/dgalli1/ytm-mpd) by dgalli1 — original YouTube Cast + MPD bridge

## License

MIT
