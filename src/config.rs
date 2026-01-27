//! Configuration module for differ
//!
//! Loads user configuration from ~/.config/differ/config.toml

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Application configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Enable side-by-side diff view
    pub side_by_side: bool,
    /// Number of context lines around changes (default 3)
    pub context_lines: u32,
    /// Show annotation content inline (default true)
    pub show_annotations: bool,
    pub syntax_highlighting: bool,
    /// Syntax theme name (syntect/bat theme)
    pub syntax_theme: Option<String>,
    pub ai_target: AiTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AiTarget {
    Claude,
    Codex,
}

impl Default for AiTarget {
    fn default() -> Self {
        AiTarget::Claude
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            side_by_side: false,
            context_lines: 3,
            show_annotations: true,
            syntax_highlighting: true,
            syntax_theme: None,
            ai_target: AiTarget::default(),
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
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".differ")
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

    /// Create a default config file
    pub fn create_default() -> Result<()> {
        let config_path = Self::default_path();
        let config = Config::default();

        // Create parent directory if needed
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;
        }

        let contents = toml::to_string_pretty(&config)
            .context("Failed to serialize config")?;

        std::fs::write(&config_path, contents)
            .with_context(|| format!("Failed to write config file: {}", config_path.display()))?;

        Ok(())
    }
}
