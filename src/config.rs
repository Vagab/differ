//! Configuration module for differ
//!
//! Loads user configuration from ~/.config/differ/config.toml

use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

/// Application configuration
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Enable side-by-side diff view
    pub side_by_side: bool,
    /// Number of context lines around changes (default 7)
    pub context_lines: u32,
    /// Show annotation content inline (default true)
    pub show_annotations: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            side_by_side: false,
            context_lines: 3,
            show_annotations: true,
        }
    }
}

impl Config {
    /// Load configuration from default path (~/.config/differ/config.toml)
    pub fn load() -> Result<Self> {
        let config_path = Self::default_path();

        if config_path.exists() {
            let contents = std::fs::read_to_string(&config_path)?;
            let config: Config = toml::from_str(&contents)?;
            Ok(config)
        } else {
            Ok(Config::default())
        }
    }

    /// Get the default config file path
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("differ")
            .join("config.toml")
    }

    /// Merge CLI overrides into config
    pub fn with_overrides(mut self, side_by_side: Option<bool>, context_lines: Option<u32>) -> Self {
        if let Some(sbs) = side_by_side {
            self.side_by_side = sbs;
        }
        if let Some(ctx) = context_lines {
            self.context_lines = ctx;
        }
        self
    }
}
