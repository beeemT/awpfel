use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct AwpfelConfig {
    pub ai: AiConfig,
    pub context: ContextConfig,
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub debounce_ms: u64,
    pub timeout_ms: u64,
    pub max_tokens: u32,
    pub features: AiFeaturesConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AiFeaturesConfig {
    pub error_recovery: bool,
    pub completion: bool,
    pub nl_generation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    pub session_history_size: usize,
    pub stderr_max_chars: usize,
    pub repo_detect: bool,
    pub git_context: bool,
    pub capture_stderr: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub mode: UiMode,
    pub ghost_text_color: u8,
    pub risk_detection: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiMode {
    #[default]
    Minimal,
    Full,
}

impl std::fmt::Display for UiMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UiMode::Minimal => write!(f, "minimal"),
            UiMode::Full => write!(f, "full"),
        }
    }
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            // On-device Apfel on the dedicated loopback endpoint: no key needed.
            enabled: true,
            base_url: "http://127.0.0.1:11435/v1".into(),
            model: "apple-foundationmodel".into(),
            api_key: String::new(),
            debounce_ms: 300,
            timeout_ms: 30000,
            max_tokens: 256,
            features: AiFeaturesConfig::default(),
        }
    }
}

impl Default for AiFeaturesConfig {
    fn default() -> Self {
        Self {
            error_recovery: false,
            completion: true,
            nl_generation: true,
        }
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            session_history_size: 20,
            stderr_max_chars: 500,
            repo_detect: true,
            git_context: true,
            capture_stderr: false,
        }
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            mode: UiMode::default(),
            ghost_text_color: 242,
            risk_detection: true,
        }
    }
}

pub fn config_dir() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("~"))
                .join(".config")
        })
        .join("awpfel")
}

pub fn data_dir() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("~"))
                .join(".local/share")
        })
        .join("awpfel")
}

pub fn runtime_dir() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
}

pub fn socket_path() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    runtime_dir().join(format!("awpfel-{uid}.sock"))
}

pub fn pid_path() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    runtime_dir().join(format!("awpfel-{uid}.pid"))
}

pub fn log_path() -> PathBuf {
    data_dir().join("awpfel.log")
}

pub fn history_db_path() -> PathBuf {
    data_dir().join("history.db")
}

pub fn default_zsh_histfile() -> PathBuf {
    if let Ok(histfile) = std::env::var("HISTFILE") {
        return PathBuf::from(histfile);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".zsh_history")
}

pub fn load_config() -> AwpfelConfig {
    let path = config_dir().join("config.toml");
    if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(content) => match toml::from_str(&content) {
                Ok(config) => return config,
                Err(e) => {
                    tracing::warn!("failed to parse config: {e}, using defaults");
                }
            },
            Err(e) => {
                tracing::warn!("failed to read config: {e}, using defaults");
            }
        }
    } else {
        let dir = config_dir();
        std::fs::create_dir_all(&dir).ok();
        let default = AwpfelConfig::default();
        if let Ok(content) = toml::to_string_pretty(&default)
            && std::fs::write(&path, &content).is_ok()
        {
            tracing::info!("created default config at {}", path.display());
        }
        return default;
    }
    AwpfelConfig::default()
}

pub fn load_config_from_str(s: &str) -> Result<AwpfelConfig, toml::de::Error> {
    toml::from_str(s)
}
