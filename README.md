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

The audio stream URL is fetched directly from YouTube's CDN using mobile client identities that return pre-signed URLs — no signature decryption, no JavaScript engine, no Python, no yt-dlp. When direct URLs are unavailable, the included SABR resolver streams audio using YouTube's own native streaming protocol.

## Features

- Cast from YouTube Music or YouTube (phone, tablet, browser)
- Play, pause, seek, skip, previous, volume — all controlled from phone
- Auto-advance to next track when a song finishes (MPD idle events, not polling)
- Playlist support with queue updates mid-session
- Phone UI stays fully synced (seekbar, album art, controls)
- Graceful disconnect (stops playback when phone disconnects)

## Stream resolution

ytcast-open resolves video IDs to audio streams using YouTube's InnerTube `/player` API with a client fallback chain:

| Priority | Method | Status (Feb 2026) |
|----------|--------|--------------------|
| 1 | IOS (local) | 20/20 OK — direct URLs, no sig decryption |
| 2 | **SABR stream** (ytresolve) | Streams audio via YouTube's native SABR protocol — always works |
| 3 | ANDROID (local) | 20/20 OK — direct URLs, no sig decryption |
| 4 | ANDROID_VR (local) | ~30% OK — LOGIN_REQUIRED on most videos |
| 5 | TVHTML5_SIMPLY (local) | Untested — no PO token, no SABR, extra fallback |

Local clients return direct `url` fields (not `signatureCipher`), so no JavaScript engine or signature decryption is needed on the Pi. The SABR fallback uses YouTube's own streaming protocol via the [googlevideo](https://github.com/LuanRT/googlevideo) library — the same protocol the official YouTube app uses.

## Remote stream resolver (ytresolve)

A Node.js service that resolves YouTube audio two ways:

1. **`/resolve/:videoId`** — returns a direct audio URL (fast, same raw InnerTube approach as the Rust binary)
2. **`/stream/:videoId`** — streams audio via YouTube's SABR protocol (always works, future-proof)

The `/stream/` endpoint uses the [googlevideo](https://github.com/LuanRT/googlevideo) library to speak YouTube's native SABR (Server Adaptive Bit Rate) protocol — the same protocol the official YouTube app uses. This is the long-term solution: when YouTube eventually stops serving direct URLs for mobile clients, SABR will still work because it's YouTube's own streaming protocol, not a workaround.

**~55MB RSS.** No YouTube.js dependency. Just raw InnerTube calls + googlevideo for SABR.

### Deployment

ytresolve can run on the Pi itself (~55MB RAM) or on a VPS for ultra-low Pi RAM usage:

```bash
# Run locally on the Pi
cd ytresolve
npm install
YTRESOLVE_SECRET=your-secret node server.js

# Or deploy on a VPS with Docker
cd ytresolve
docker build -t ytresolve .
docker run -d --name ytresolve -p 3033:3033 \
  -e YTRESOLVE_SECRET=your-shared-secret \
  ytresolve
```

**Connect to ytcast-open:**
```bash
export YTRESOLVE_URL="http://localhost:3033"  # or https://your-vps:3033
export YTRESOLVE_SECRET="your-shared-secret"
```

### How SABR streaming works

When ytcast-open calls `/stream/:videoId`, ytresolve:
1. Makes a raw InnerTube `/player` call (IOS client, no SABR capability advertised)
2. Gets the SABR streaming URL + format metadata from the response
3. Opens a SABR session using googlevideo's `SabrStream`
4. Pipes opus audio chunks directly to the HTTP response
5. MPD plays the stream as a regular HTTP audio source

The SABR session handles server redirects, backoff, retries, and format negotiation automatically.

**Without a remote resolver:** ytcast-open works fine standalone — it just uses local InnerTube clients for direct URLs. The resolver is insurance for when YouTube blocks mobile client direct URLs.

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

# Optional: remote resolver (YouTube.js on your VPS)
export YTRESOLVE_URL="https://ytresolve.maelo.ca"
export YTRESOLVE_SECRET="your-shared-secret"

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
| `innertube.rs` | ~360 | Stream resolver — IOS → remote (YouTube.js) → ANDROID → VR → TV fallback chain |
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
| Sig decryption | Not needed locally; remote resolver handles it | YouTube.js (66MB import) | Built-in JS interpreter |

## Credits

- [yt-cast-receiver](https://github.com/patrickkfkan/yt-cast-receiver) by patrickkfkan — Node.js cast receiver framework (protocol reference)
- [plaincast](https://github.com/nickvdp/plaincast) — Go cast receiver (Lounge API reference)
- [ytm-mpd](https://github.com/dgalli1/ytm-mpd) by dgalli1 — original YouTube Cast + MPD bridge

## License

MIT
