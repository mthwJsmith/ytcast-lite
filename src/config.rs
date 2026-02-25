use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

pub type Result<T> = std::result::Result<T, ConfigError>;

#[derive(Debug)]
pub enum ConfigError {
    Io(io::Error),
    Json(serde_json::Error),
    NoHomeDir,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "config io: {e}"),
            ConfigError::Json(e) => write!(f, "config json: {e}"),
            ConfigError::NoHomeDir => write!(f, "could not determine home directory"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(e) => Some(e),
            ConfigError::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for ConfigError {
    fn from(e: io::Error) -> Self {
        ConfigError::Io(e)
    }
}

impl From<serde_json::Error> for ConfigError {
    fn from(e: serde_json::Error) -> Self {
        ConfigError::Json(e)
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Config {
    pub uuid: String,
    pub screen_id: String,
}

impl Config {
    /// Load config from disk, or create a new one with defaults if the file
    /// does not exist.
    pub fn load() -> Result<Self> {
        let path = Self::path()?;

        match std::fs::read_to_string(&path) {
            Ok(data) => {
                let config: Config = serde_json::from_str(&data)?;
                tracing::info!("loaded config from {}", path.display());
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let config = Config {
                    uuid: uuid::Uuid::new_v4().to_string(),
                    screen_id: String::new(),
                };
                config.save()?;
                tracing::info!("created new config at {}", path.display());
                Ok(config)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Save config to disk, creating parent directories if needed.
    /// Save config atomically: write to temp file, then rename.
    /// Prevents corruption on power loss (important for a Pi running 24/7).
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Get the config file path: `~/.config/ytcast-lite.json`
    fn path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir().ok_or(ConfigError::NoHomeDir)?;
        Ok(config_dir.join("ytcast-lite.json"))
    }
}
