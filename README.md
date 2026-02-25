# ytcast-open

A lightweight YouTube Music cast receiver written in Rust. Makes a Raspberry Pi (or any Linux device) appear as a cast target in YouTube Music — tap Cast, see your device, play music through MPD.

**3.8MB RSS idle. 2.4MB binary. Zero runtime dependencies.**

## How it works

```
YouTube Music (phone)
    | SSDP multicast discovery
    v
ytcast-open (this binary)
    | DIAL protocol (port 8008) — device appears in cast menu
    | YouTube Lounge API — long-poll session for play/pause/seek/skip/volume
    | InnerTube API — resolves video ID → direct audio stream URL
    v
MPD (Music Player Daemon)
    | ALSA output
    v
speakers
```

1. Phone discovers the device via SSDP/DIAL (same open protocol Chromecast uses for discovery)
2. Phone connects via YouTube's Lounge API (reverse-engineered cast control protocol)
3. Phone sends a video ID — ytcast-open resolves it to an audio stream URL via YouTube's InnerTube API
4. Audio plays through MPD. Position, duration, and state sync back to the phone in real-time

The audio stream URL is fetched directly from YouTube's CDN using mobile client identities that return pre-signed URLs — no signature decryption, no JavaScript engine, no Python, no yt-dlp.

## Features

- Cast from YouTube Music or YouTube (phone, tablet, browser)
- Play, pause, seek, skip, previous, volume — all controlled from phone
- Auto-advance to next track when a song finishes (MPD idle events, not polling)
- Playlist support with queue updates mid-session
- Phone UI stays fully synced (seekbar, album art, controls)
- Graceful disconnect (stops playback when phone disconnects)

## Stream resolution

ytcast-open resolves video IDs to audio streams using YouTube's InnerTube `/player` API with a client fallback chain:

| Priority | Client | Status (Feb 2026) |
|----------|--------|--------------------|
| 1 | IOS | 20/20 OK — direct URLs, no sig decryption |
| 2 | ANDROID | 20/20 OK — direct URLs, no sig decryption |
| 3 | ANDROID_VR | ~30% OK — LOGIN_REQUIRED on most videos |
| 4 | TVHTML5_SIMPLY | Untested — no PO token, no SABR, extra fallback |

All clients return direct `url` fields (not `signatureCipher`), so no JavaScript engine or signature decryption is needed.

**Future risk:** YouTube is rolling out PO token requirements and SABR (a new streaming protocol that removes direct URLs). Mobile clients buy time. When they get blocked, the recommended fallback is a **remote stream resolver** — see below.

## Remote stream resolver (recommended future-proofing)

If you have access to a server with more RAM (VPS, Hetzner, etc.), the most resilient setup is a tiny HTTP API on that server that resolves video IDs to stream URLs, and ytcast-open calls it as a fallback when all InnerTube clients fail.

**Best option: [YouTube.js](https://github.com/LuanRT/YouTube.js) + [googlevideo](https://github.com/LuanRT/googlevideo) SABR implementation.** This implements YouTube's own new streaming protocol (SABR) rather than trying to work around it. It's more future-proof than yt-dlp because it speaks the protocol YouTube is converging everything to. Needs Node.js/Deno and ~100-200MB RAM — too heavy for a Pi Zero but trivial on a VPS.

Alternative: yt-dlp on the server. Battle-tested, updated within hours of YouTube changes, but it works around YouTube's restrictions rather than implementing them. Needs Python + JS runtime + ~100MB.

Either way, ytcast-open on the Pi stays at 3.8MB — it just makes one HTTP call to your server instead of calling InnerTube directly.

## Requirements

- **MPD** (Music Player Daemon) running and accessible
- **Linux** (tested on Raspberry Pi Zero 2W with Debian trixie/aarch64)
- Network access to YouTube servers

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
| `innertube.rs` | ~310 | InnerTube stream resolver — IOS/ANDROID/ANDROID_VR/TVHTML5_SIMPLY fallback chain |
| `mpd.rs` | ~340 | Raw MPD TCP client (command + idle connections) + mock mode for dev |
| `ssdp.rs` | ~100 | SSDP multicast discovery responder |
| `dial.rs` | ~120 | DIAL HTTP server on port 8008 |
| `config.rs` | ~50 | JSON config (UUID, screen_id) load/save |

Single-threaded tokio runtime. Two MPD TCP connections (command + idle). One long-poll HTTP connection to YouTube Lounge API.

## Comparison

| | ytcast-open (Rust) | Node.js version | Typical yt-dlp setup |
|-|-------------------|----------------|---------------------|
| Idle RSS | **3.8MB** | ~70MB | 50-90MB per invocation |
| Binary/install | **2.4MB** single binary | 38MB node_modules | Python + yt-dlp + JS runtime |
| Startup | ~50ms | ~2s | 2-5s per video |
| Runtime deps | None | Node.js 18+ | Python 3, QuickJS/Deno/Node |
| Sig decryption | Not needed (mobile clients) | YouTube.js (66MB import) | Built-in JS interpreter |

## Credits

- [yt-cast-receiver](https://github.com/patrickkfkan/yt-cast-receiver) by patrickkfkan — Node.js cast receiver framework (protocol reference)
- [plaincast](https://github.com/nickvdp/plaincast) — Go cast receiver (Lounge API reference)
- [ytm-mpd](https://github.com/dgalli1/ytm-mpd) by dgalli1 — original YouTube Cast + MPD bridge

## License

MIT
