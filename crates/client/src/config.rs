//! Persistent client configuration (`chattercrab/config.toml` in the platform config
//! directory). Chat history is never stored.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Default screen-share frame rate.
pub const DEFAULT_SCREEN_FPS: u32 = 15;
/// Allowed screen-share frame rates.
pub const SCREEN_FPS_RANGE: std::ops::RangeInclusive<u32> = 5..=30;

/// Light or dark UI theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeChoice {
    /// Light theme.
    Light,
    /// Dark theme.
    #[default]
    Dark,
}

/// User settings persisted between runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Signaling server base URL, e.g. `https://chat.example.com:8443`.
    pub server_url: String,
    /// Display name used when joining rooms.
    pub display_name: String,
    /// Preferred microphone; `None` uses the system default.
    pub input_device: Option<String>,
    /// Preferred speakers; `None` uses the system default.
    pub output_device: Option<String>,
    /// UI theme.
    pub theme: ThemeChoice,
    /// Screen-share frame rate (5 to 30).
    pub screen_fps: u32,
    /// Microphone noise suppression (RNNoise) enabled.
    pub noise_suppression: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_url: "https://localhost:8443".to_owned(),
            display_name: String::new(),
            input_device: None,
            output_device: None,
            theme: ThemeChoice::default(),
            screen_fps: DEFAULT_SCREEN_FPS,
            noise_suppression: false,
        }
    }
}

impl Config {
    /// Default location: `<config dir>/chattercrab/config.toml`.
    pub fn default_path() -> Option<PathBuf> {
        directories::BaseDirs::new()
            .map(|dirs| dirs.config_dir().join("chattercrab").join("config.toml"))
    }

    /// Loads the configuration from `path`. A missing file yields the defaults.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut config: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.screen_fps = config
            .screen_fps
            .clamp(*SCREEN_FPS_RANGE.start(), *SCREEN_FPS_RANGE.end());
        Ok(config)
    }

    /// Writes the configuration to `path`, creating parent directories.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serializing config")?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chattercrab-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("chattercrab").join("config.toml");
        let config = Config {
            server_url: "https://chat.example.com:8443".into(),
            display_name: "ada".into(),
            input_device: Some("USB Mic".into()),
            output_device: None,
            theme: ThemeChoice::Light,
            screen_fps: 20,
            noise_suppression: true,
        };
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = temp_dir("missing");
        assert_eq!(
            Config::load(&dir.join("nope.toml")).unwrap(),
            Config::default()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn partial_file_and_fps_clamp() {
        let dir = temp_dir("partial");
        let path = dir.join("config.toml");
        std::fs::write(&path, "display_name = \"grace\"\nscreen_fps = 120\n").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.display_name, "grace");
        assert_eq!(config.screen_fps, 30);
        assert_eq!(config.server_url, Config::default().server_url);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn noise_suppression_defaults_off_for_old_files() {
        let dir = temp_dir("old-file");
        let path = dir.join("config.toml");
        // A config written before the option existed.
        std::fs::write(
            &path,
            "server_url = \"https://chat.example.com:8443\"
display_name = \"ada\"
theme = \"dark\"
screen_fps = 15
",
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert!(!config.noise_suppression);
        assert_eq!(config.display_name, "ada");

        let enabled = Config {
            noise_suppression: true,
            ..config
        };
        enabled.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("noise_suppression = true"));
        assert!(Config::load(&path).unwrap().noise_suppression);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn default_path_ends_with_expected_components() {
        if let Some(path) = Config::default_path() {
            assert!(path.ends_with("chattercrab/config.toml"));
        }
    }
}
