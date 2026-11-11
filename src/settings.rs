//! Operator settings (`[plugin.settings]`), with the defaults from the design.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PointerPolicy {
    /// Large documents go to tool-using loopback callers as file paths.
    #[default]
    Auto,
    /// Only when the caller asks with `x-mesh-documents: pointer`.
    Header,
    /// Never hand out file paths (an agent that cannot see this machine).
    Off,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub inline_max_chars: usize,
    pub pointer: PointerPolicy,
    pub store_dir: Option<PathBuf>,
    pub ttl_hours: u64,
    pub max_store_mb: u64,
    pub max_total_pdf_mb: u64,
    pub max_total_chars: usize,
    /// A running Tika server on loopback, e.g. `http://127.0.0.1:9998`.
    pub tika_url: Option<String>,
    /// Otherwise the plugin starts `java -jar <tika_jar>` on a loopback port.
    pub tika_jar: Option<PathBuf>,
    pub java: String,
    pub tika_concurrency: usize,
    pub root_model: Option<String>,
    pub sub_model: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            inline_max_chars: 40_000,
            pointer: PointerPolicy::Auto,
            store_dir: None,
            ttl_hours: 24,
            max_store_mb: 2048,
            max_total_pdf_mb: 64,
            max_total_chars: 5_000_000,
            tika_url: None,
            tika_jar: None,
            java: "java".into(),
            tika_concurrency: 2,
            root_model: None,
            sub_model: None,
        }
    }
}

impl Settings {
    pub fn parse(settings_json: &str) -> Result<Self> {
        let settings: Self = if settings_json.trim().is_empty() {
            Self::default()
        } else {
            serde_json::from_str(settings_json).context("invalid documents plugin settings")?
        };
        if settings.tika_concurrency == 0 {
            bail!("tika_concurrency must be at least 1");
        }
        if settings.ttl_hours == 0 {
            bail!("ttl_hours must be at least 1");
        }
        Ok(settings)
    }

    pub fn store_root(&self) -> Result<PathBuf> {
        if let Some(dir) = self
            .store_dir
            .as_ref()
            .filter(|dir| !dir.as_os_str().is_empty())
        {
            if !dir.is_absolute() {
                bail!("store_dir must be an absolute path");
            }
            return Ok(dir.clone());
        }
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .context("HOME is not set; configure store_dir")?;
        Ok(PathBuf::from(home).join(".mesh-llm").join("documents"))
    }

    pub fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl_hours.saturating_mul(3600))
    }

    pub fn max_total_pdf_bytes(&self) -> u64 {
        self.max_total_pdf_mb.saturating_mul(1024 * 1024)
    }

    pub fn max_store_bytes(&self) -> u64 {
        self.max_store_mb.saturating_mul(1024 * 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_settings_use_the_design_defaults() {
        let settings = Settings::parse("{}").unwrap();
        assert_eq!(settings.inline_max_chars, 40_000);
        assert_eq!(settings.pointer, PointerPolicy::Auto);
        assert_eq!(settings.max_total_chars, 5_000_000);
    }

    #[test]
    fn unknown_keys_and_relative_store_dirs_are_rejected() {
        assert!(Settings::parse(r#"{"pointr":"off"}"#).is_err());
        let settings = Settings::parse(r#"{"store_dir":"rel/dir","pointer":"header"}"#).unwrap();
        assert_eq!(settings.pointer, PointerPolicy::Header);
        assert!(settings.store_root().is_err());
    }
}
