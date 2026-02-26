use std::fmt;

pub type Result<T> = std::result::Result<T, MpdError>;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug)]
#[allow(dead_code)]
pub enum MpdError {
    Io(std::io::Error),
    /// MPD returned `ACK [error@pos] {command} message`
    Ack(String),
    /// Unexpected protocol response
    Protocol(String),
}

impl fmt::Display for MpdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MpdError::Io(e) => write!(f, "mpd io: {e}"),
            MpdError::Ack(msg) => write!(f, "mpd ack: {msg}"),
            MpdError::Protocol(msg) => write!(f, "mpd protocol: {msg}"),
        }
    }
}

impl std::error::Error for MpdError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MpdError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for MpdError {
    fn from(e: std::io::Error) -> Self {
        MpdError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct MpdStatus {
    pub state: String,
    pub elapsed: Option<f64>,
    #[allow(dead_code)]
    pub duration: Option<f64>,
    pub volume: u32,
}

// ---------------------------------------------------------------------------
// Real MPD client (production)
// ---------------------------------------------------------------------------

#[cfg(not(feature = "mock-mpd"))]
mod real {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
    use tokio::net::TcpStream;

    pub struct MpdClient {
        reader: BufReader<tokio::io::ReadHalf<TcpStream>>,
        writer: BufWriter<tokio::io::WriteHalf<TcpStream>>,
        host: String,
        port: u16,
    }

    impl MpdClient {
        /// Connect to MPD and consume the greeting line (`OK MPD x.x.x`).
        pub async fn connect(host: &str, port: u16) -> Result<Self> {
            let (reader, writer) = Self::connect_inner(host, port).await?;
            Ok(Self {
                reader,
                writer,
                host: host.to_owned(),
                port,
            })
        }

        async fn connect_inner(
            host: &str,
            port: u16,
        ) -> Result<(
            BufReader<tokio::io::ReadHalf<TcpStream>>,
            BufWriter<tokio::io::WriteHalf<TcpStream>>,
        )> {
            let stream = TcpStream::connect((host, port)).await?;
            let (rd, wr) = tokio::io::split(stream);
            let mut reader = BufReader::new(rd);
            let writer = BufWriter::new(wr);

            // Read greeting
            let mut greeting = String::new();
            let n = reader.read_line(&mut greeting).await?;
            if n == 0 {
                return Err(MpdError::Protocol("connection closed before greeting".into()));
            }
            if !greeting.starts_with("OK MPD") {
                return Err(MpdError::Protocol(format!(
                    "expected OK MPD greeting, got: {greeting}"
                )));
            }

            tracing::info!("connected to MPD at {host}:{port} — {}", greeting.trim());
            Ok((reader, writer))
        }

        async fn reconnect(&mut self) -> Result<()> {
            tracing::info!("[MPD] reconnecting command connection...");
            let (reader, writer) = Self::connect_inner(&self.host, self.port).await?;
            self.reader = reader;
            self.writer = writer;
            Ok(())
        }

        // -- queue / playback ------------------------------------------------

        pub async fn clear(&mut self) -> Result<()> {
            self.command("clear").await?;
            Ok(())
        }

        pub async fn add(&mut self, uri: &str) -> Result<()> {
            if uri.contains('"') || uri.contains('\n') || uri.contains('\r') {
                return Err(MpdError::Protocol("URI contains forbidden characters".into()));
            }
            self.command(&format!("add \"{uri}\"")).await?;
            Ok(())
        }

        pub async fn play(&mut self) -> Result<()> {
            self.command("play").await?;
            Ok(())
        }

        pub async fn pause(&mut self) -> Result<()> {
            self.command("pause 1").await?;
            Ok(())
        }

        pub async fn resume(&mut self) -> Result<()> {
            self.command("pause 0").await?;
            Ok(())
        }

        pub async fn stop(&mut self) -> Result<()> {
            self.command("stop").await?;
            Ok(())
        }

        pub async fn seekcur(&mut self, seconds: f64) -> Result<()> {
            if !seconds.is_finite() || seconds < 0.0 {
                return Err(MpdError::Protocol("invalid seek position".into()));
            }
            self.command(&format!("seekcur {seconds}")).await?;
            Ok(())
        }

        pub async fn setvol(&mut self, vol: u32) -> Result<()> {
            self.command(&format!("setvol {vol}")).await?;
            Ok(())
        }

        pub async fn single(&mut self, mode: &str) -> Result<()> {
            self.command(&format!("single {mode}")).await?;
            Ok(())
        }

        pub async fn consume(&mut self, mode: &str) -> Result<()> {
            self.command(&format!("consume {mode}")).await?;
            Ok(())
        }

        // -- status ----------------------------------------------------------

        pub async fn status(&mut self) -> Result<MpdStatus> {
            let lines = self.command("status").await?;
            let mut state = String::from("stop");
            let mut elapsed: Option<f64> = None;
            let mut duration: Option<f64> = None;
            let mut volume: u32 = 0;

            for line in &lines {
                if let Some((key, val)) = line.split_once(": ") {
                    match key {
                        "state" => state = val.to_owned(),
                        "elapsed" => elapsed = val.parse().ok(),
                        "duration" => duration = val.parse().ok(),
                        "volume" => volume = val.parse().unwrap_or(0),
                        _ => {}
                    }
                }
            }

            Ok(MpdStatus {
                state,
                elapsed,
                duration,
                volume,
            })
        }

        // -- protocol layer --------------------------------------------------

        /// Send a command, auto-reconnecting once on connection errors
        /// (Io errors, "connection closed"). ACK errors from MPD are NOT retried.
        async fn command(&mut self, cmd: &str) -> Result<Vec<String>> {
            match self.command_raw(cmd).await {
                Ok(lines) => Ok(lines),
                Err(e @ MpdError::Ack(_)) => Err(e),
                Err(_) => {
                    // Io or Protocol error = connection broken. Reconnect + retry.
                    if let Err(e) = self.reconnect().await {
                        tracing::error!("[MPD] reconnect failed: {e}");
                        return Err(e);
                    }
                    self.command_raw(cmd).await
                }
            }
        }

        /// Send a raw command (no reconnect logic).
        async fn command_raw(&mut self, cmd: &str) -> Result<Vec<String>> {
            // Write command
            self.writer.write_all(cmd.as_bytes()).await?;
            self.writer.write_all(b"\n").await?;
            self.writer.flush().await?;

            // Read response lines until OK or ACK
            const MAX_LINE_LEN: usize = 64 * 1024;
            const MAX_LINES: usize = 10_000;
            let mut lines = Vec::new();
            loop {
                let mut line = String::new();
                let n = self.reader.read_line(&mut line).await?;
                if n == 0 {
                    return Err(MpdError::Protocol("connection closed".into()));
                }
                if line.len() > MAX_LINE_LEN {
                    return Err(MpdError::Protocol("response line too long".into()));
                }

                let trimmed = line.trim_end();

                if trimmed == "OK" {
                    return Ok(lines);
                }
                if trimmed.starts_with("ACK ") {
                    return Err(MpdError::Ack(trimmed.to_owned()));
                }

                if lines.len() >= MAX_LINES {
                    return Err(MpdError::Protocol("too many response lines".into()));
                }
                lines.push(trimmed.to_owned());
            }
        }
    }

    // -----------------------------------------------------------------------
    // Idle client — dedicated connection for real-time player state events
    // -----------------------------------------------------------------------

    pub struct MpdIdleClient {
        reader: BufReader<tokio::io::ReadHalf<TcpStream>>,
        writer: BufWriter<tokio::io::WriteHalf<TcpStream>>,
    }

    impl MpdIdleClient {
        /// Connect to MPD and consume the greeting line.
        ///
        /// This connection is dedicated to `idle` commands for real-time event
        /// detection — the same approach the Node.js mpd2 library uses.
        pub async fn connect(host: &str, port: u16) -> Result<Self> {
            let stream = TcpStream::connect((host, port)).await?;
            let (rd, wr) = tokio::io::split(stream);
            let mut reader = BufReader::new(rd);
            let writer = BufWriter::new(wr);

            let mut greeting = String::new();
            let n = reader.read_line(&mut greeting).await?;
            if n == 0 {
                return Err(MpdError::Protocol("connection closed before greeting".into()));
            }
            if !greeting.starts_with("OK MPD") {
                return Err(MpdError::Protocol(format!(
                    "expected OK MPD greeting, got: {greeting}"
                )));
            }

            Ok(Self { reader, writer })
        }

        /// Block until MPD's player subsystem changes (song ends, play/pause/stop).
        ///
        /// Sends MPD's `idle player` command which blocks server-side until a
        /// player event occurs. Returns immediately when the state changes.
        /// This gives instant song-end detection — no polling delay.
        pub async fn wait_player_change(&mut self) -> Result<()> {
            self.writer.write_all(b"idle player\n").await?;
            self.writer.flush().await?;

            loop {
                let mut line = String::new();
                let n = self.reader.read_line(&mut line).await?;
                if n == 0 {
                    return Err(MpdError::Protocol("idle connection closed".into()));
                }
                let trimmed = line.trim_end();
                if trimmed == "OK" {
                    return Ok(());
                }
                if trimmed.starts_with("ACK ") {
                    return Err(MpdError::Ack(trimmed.to_owned()));
                }
                // "changed: player" — consume and wait for the terminating OK
            }
        }
    }
}

#[cfg(not(feature = "mock-mpd"))]
pub use real::{MpdClient, MpdIdleClient};

// ---------------------------------------------------------------------------
// Mock MPD client (for Windows development without a real MPD)
// ---------------------------------------------------------------------------

#[cfg(feature = "mock-mpd")]
mod mock {
    use super::*;

    pub struct MpdClient;

    impl MpdClient {
        pub async fn connect(host: &str, port: u16) -> Result<Self> {
            tracing::info!("[mock-mpd] skipping connection to {host}:{port}");
            Ok(Self)
        }

        pub async fn clear(&mut self) -> Result<()> {
            Ok(())
        }

        pub async fn add(&mut self, uri: &str) -> Result<()> {
            tracing::info!("[mock-mpd] add: {}", &uri[..uri.len().min(80)]);
            Ok(())
        }

        pub async fn play(&mut self) -> Result<()> {
            tracing::info!("[mock-mpd] play");
            Ok(())
        }

        pub async fn pause(&mut self) -> Result<()> {
            tracing::info!("[mock-mpd] pause");
            Ok(())
        }

        pub async fn resume(&mut self) -> Result<()> {
            tracing::info!("[mock-mpd] resume");
            Ok(())
        }

        pub async fn stop(&mut self) -> Result<()> {
            tracing::info!("[mock-mpd] stop");
            Ok(())
        }

        pub async fn seekcur(&mut self, seconds: f64) -> Result<()> {
            tracing::info!("[mock-mpd] seekcur {seconds}");
            Ok(())
        }

        pub async fn setvol(&mut self, vol: u32) -> Result<()> {
            tracing::info!("[mock-mpd] setvol {vol}");
            Ok(())
        }

        pub async fn single(&mut self, _mode: &str) -> Result<()> {
            Ok(())
        }

        pub async fn consume(&mut self, _mode: &str) -> Result<()> {
            Ok(())
        }

        pub async fn status(&mut self) -> Result<MpdStatus> {
            Ok(MpdStatus {
                state: "stop".into(),
                elapsed: Some(0.0),
                duration: Some(0.0),
                volume: 50,
            })
        }
    }

    pub struct MpdIdleClient;

    impl MpdIdleClient {
        pub async fn connect(_host: &str, _port: u16) -> Result<Self> {
            tracing::info!("[mock-mpd] idle client (no-op)");
            Ok(Self)
        }

        pub async fn wait_player_change(&mut self) -> Result<()> {
            // Mock: never fires — no real MPD to generate events
            std::future::pending().await
        }
    }
}

#[cfg(feature = "mock-mpd")]
pub use mock::{MpdClient, MpdIdleClient};
